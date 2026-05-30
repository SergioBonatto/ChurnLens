pub mod ast;
pub mod cache;
pub mod error;
pub mod git;
pub mod metrics;

use anyhow::Result;
use ast::c::CSupport;
use ast::parser::AstParser;
use ast::rust::RustSupport;
use ast::typescript::TypeScriptSupport;
use ast::LanguageSupport;
use cache::{AnalysisCache, CacheManager, FileCacheEntry, GitCacheEntry};
use chrono::{DateTime, Utc};
use crossbeam_channel::{bounded, Receiver, Sender};
use git::{BugFixPatterns, GitAnalysisContext, GitAnalyzer};
use git2::Repository;
use ignore::WalkBuilder;
use memmap2::{Mmap, MmapOptions};
use metrics::{
    AnalysisMetadata, AnalysisQuality, AnalysisStatus, AnalysisWarning, CacheAnalysisStatus,
    Distributions, FunctionMetrics, GitAnalysisStatus, MaxValues, NormalizedMetrics,
    PercentileMetrics, Report, RiskMetrics, ScoringPolicy, SkippedFile, SummaryStats,
};
use rayon::iter::{ParallelBridge, ParallelIterator};
use serde::Deserialize;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use xxhash_rust::xxh3::xxh3_128;

const SCHEMA_VERSION: &str = "0.4.0";
const FALLBACK_TIMESTAMP: &str = "1970-01-01T00:00:00+00:00";

const WEIGHT_COGNITIVE: f64 = 0.30;
const WEIGHT_CYCLOMATIC: f64 = 0.10;
const WEIGHT_CHURN: f64 = 0.20;
const WEIGHT_CHURN_RECENT: f64 = 0.15;
const WEIGHT_FAN_IN: f64 = 0.10;
const WEIGHT_LOC: f64 = 0.05;
const WEIGHT_AUTHORS: f64 = 0.05;
const WEIGHT_COVERAGE_GAP: f64 = 0.05;

const THRESHOLD_CRITICAL: f64 = 95.0;
const THRESHOLD_HIGH: f64 = 80.0;
const THRESHOLD_MEDIUM: f64 = 50.0;
const MAX_PARALLEL_FILE_READS: usize = 8;
const MMAP_FILE_THRESHOLD_BYTES: u64 = 1024 * 1024;

struct RepoContext {
    repo: Option<Repository>,
    branch: String,
    commit_hash: String,
    timestamp: String,
    cache_manager: Option<CacheManager>,
    cache: AnalysisCache,
    cache_loaded: bool,
    repo_path_abs: std::path::PathBuf,
    config: UchikomiConfig,
    coverage: Option<CoverageIndex>,
}

#[derive(Default, Deserialize)]
struct UchikomiConfig {
    #[serde(default)]
    git: GitConfig,
}

#[derive(Default, Deserialize)]
struct GitConfig {
    #[serde(default)]
    bug_fix_patterns: Vec<String>,
}

#[derive(Default)]
struct CoverageIndex {
    files: HashMap<String, FileCoverageData>,
}

#[derive(Default)]
struct FileCoverageData {
    lines: HashMap<u32, u32>,
    branches: Vec<BranchCoverageData>,
}

struct BranchCoverageData {
    line: u32,
    taken: bool,
}

struct NormalizationCaps {
    cognitive: f64,
    churn: f64,
    churn_recent: f64,
    loc: f64,
    cyclomatic: f64,
    authors: f64,
    fan_in: f64,
    coverage_gap: f64,
}

struct AnalysisResults<'a> {
    warnings: Vec<AnalysisWarning>,
    skipped_files: Vec<SkippedFile>,
    functions: Vec<FunctionMetrics>,
    git_cache: &'a HashMap<String, GitCacheEntry>,
    total_unique_authors: usize,
}

struct WorkerContext<'a> {
    repo_path_abs: &'a Path,
    registry: Arc<LanguageRegistry>,
    cache: &'a AnalysisCache,
    git_cache: &'a HashMap<String, GitCacheEntry>,
    include_authors: bool,
    file_read_limiter: Arc<FileReadLimiter>,
    shutdown: &'a AtomicBool,
}

struct ScoringContext {
    max_values: MaxValues,
    caps: NormalizationCaps,
    cog_vals: Vec<u32>,
    churn_vals: Vec<f64>,
    p95_idx: usize,
}

pub struct LanguageRegistry {
    supports: Vec<Box<dyn LanguageSupport>>,
}

impl LanguageRegistry {
    pub fn new() -> Self {
        Self {
            supports: vec![
                Box::new(TypeScriptSupport::new(false)),
                Box::new(TypeScriptSupport::new(true)),
                Box::new(RustSupport),
                Box::new(CSupport),
            ],
        }
    }
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageRegistry {
    pub fn get_support(&self, file_path: &str) -> Option<&dyn LanguageSupport> {
        let path = Path::new(file_path);
        let ext = path.extension()?.to_str()?;
        self.supports
            .iter()
            .find(|s| s.extensions().contains(&ext))
            .map(|s| s.as_ref())
    }
}

pub fn analyze_repository(
    repo_path: &Path,
    sort_by: &str,
    limit: Option<usize>,
    shutdown: Arc<AtomicBool>,
) -> Result<Report> {
    analyze_repository_with_authors(repo_path, sort_by, limit, false, shutdown)
}

pub fn analyze_repository_with_authors(
    repo_path: &Path,
    sort_by: &str,
    limit: Option<usize>,
    include_authors: bool,
    shutdown: Arc<AtomicBool>,
) -> Result<Report> {
    log::info!("Analyzing repository: {}", repo_path.display());

    let registry = Arc::new(LanguageRegistry::new());
    let mut warnings = Vec::new();
    let mut skipped_files = Vec::new();

    let mut ctx = load_repository_context(repo_path, &mut warnings)?;
    let bug_fix_patterns = configured_bug_fix_patterns(&ctx.config, &mut warnings);
    let (mut git_cache, git_status) = collect_git_analysis(
        ctx.repo.as_ref(),
        &ctx.repo_path_abs,
        &ctx.branch,
        &ctx.commit_hash,
        &mut ctx.cache,
        &bug_fix_patterns,
        &mut warnings,
    );

    let total_unique_authors = git_cache
        .values()
        .flat_map(|entry| entry.authors.iter())
        .collect::<HashSet<_>>()
        .len();

    let worker_result = analyze_files_parallel(
        &ctx.repo_path_abs,
        registry,
        &ctx.cache,
        &git_cache,
        include_authors,
        shutdown,
    );

    let WorkerResult {
        mut functions,
        cache_entries,
        active_paths,
        cache_stats,
        warnings: worker_warnings,
        skipped_files: worker_skipped,
    } = worker_result;

    warnings.extend(worker_warnings);
    skipped_files.extend(worker_skipped);
    git_cache.retain(|path, _| active_paths.contains(path));
    apply_coverage(&mut functions, ctx.coverage.as_ref());

    let scoring_policy = default_scoring_policy();

    if functions.is_empty() {
        warnings.push(AnalysisWarning {
            code: "no_functions_found".to_string(),
            message: "No functions found to analyze.".to_string(),
        });
        return Ok(empty_analysis_report(
            &ctx,
            scoring_policy,
            git_status,
            cache_stats,
            AnalysisResults {
                warnings,
                skipped_files,
                functions,
                git_cache: &git_cache,
                total_unique_authors,
            },
        ));
    }

    apply_coupling_and_reachability(&mut functions);
    let scoring_context = compute_scoring_context(&functions);
    let distributions = apply_scoring_and_labels(&mut functions, &scoring_context);
    update_coverage_gaps(&mut functions);
    let project_stats = build_project_stats(&functions, &git_cache, total_unique_authors);

    sort_functions(&mut functions, sort_by, &mut warnings);
    apply_function_limit(&mut functions, limit);

    let cache_saved = save_analysis_cache(
        &ctx.cache_manager,
        ctx.cache,
        cache_entries,
        git_cache,
        &ctx.commit_hash,
        &mut warnings,
    );

    let quality = build_quality(
        git_status,
        ctx.cache_manager.is_some(),
        ctx.cache_loaded,
        cache_saved,
        cache_stats,
        warnings,
        skipped_files,
    );

    Ok(Report {
        schema_version: SCHEMA_VERSION.to_string(),
        analysis: analysis_metadata(
            &ctx.repo_path_abs,
            &ctx.commit_hash,
            &ctx.branch,
            &ctx.timestamp,
        ),
        scoring_policy,
        summary: SummaryStats {
            total_functions: functions.len(),
            project_stats,
            coverage: build_project_coverage(&functions),
            max_values: Some(scoring_context.max_values),
            distributions: Some(distributions),
        },
        quality,
        functions,
    })
}

fn apply_function_limit(functions: &mut Vec<FunctionMetrics>, limit: Option<usize>) {
    if let Some(limit) = limit {
        functions.truncate(limit);
    }
}

fn empty_analysis_report(
    ctx: &RepoContext,
    scoring_policy: ScoringPolicy,
    git_status: GitAnalysisStatus,
    cache_stats: CacheStats,
    results: AnalysisResults,
) -> Report {
    let quality = build_quality(
        git_status,
        ctx.cache_manager.is_some(),
        ctx.cache_loaded,
        false,
        cache_stats,
        results.warnings,
        results.skipped_files,
    );

    Report {
        schema_version: SCHEMA_VERSION.to_string(),
        analysis: analysis_metadata(
            &ctx.repo_path_abs,
            &ctx.commit_hash,
            &ctx.branch,
            &ctx.timestamp,
        ),
        scoring_policy,
        summary: SummaryStats {
            total_functions: 0,
            project_stats: build_project_stats(
                &results.functions,
                results.git_cache,
                results.total_unique_authors,
            ),
            coverage: build_project_coverage(&results.functions),
            max_values: None,
            distributions: None,
        },
        quality,
        functions: results.functions,
    }
}

fn configured_bug_fix_patterns(
    config: &UchikomiConfig,
    warnings: &mut Vec<AnalysisWarning>,
) -> BugFixPatterns {
    match BugFixPatterns::from_patterns(&config.git.bug_fix_patterns) {
        Ok(patterns) => patterns,
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "invalid_bug_fix_pattern".to_string(),
                message: format!("Invalid bug-fix pattern configuration: {err}"),
            });
            BugFixPatterns::default()
        }
    }
}

fn default_scoring_policy() -> ScoringPolicy {
    ScoringPolicy {
        weights: metrics::Weights {
            cognitive: WEIGHT_COGNITIVE,
            cyclomatic: WEIGHT_CYCLOMATIC,
            churn: WEIGHT_CHURN,
            churn_recent: WEIGHT_CHURN_RECENT,
            fan_in: WEIGHT_FAN_IN,
            loc: WEIGHT_LOC,
            authors: WEIGHT_AUTHORS,
            coverage_gap: WEIGHT_COVERAGE_GAP,
        },
        thresholds: metrics::Thresholds {
            critical: THRESHOLD_CRITICAL,
            high: THRESHOLD_HIGH,
            medium: THRESHOLD_MEDIUM,
        },
        description: "Composite risk score based on complexity, churn, and team metrics."
            .to_string(),
    }
}

fn analysis_metadata(
    repo_path_abs: &Path,
    commit_hash: &str,
    branch: &str,
    timestamp: &str,
) -> AnalysisMetadata {
    AnalysisMetadata {
        repository: repo_path_abs.to_string_lossy().to_string(),
        commit: commit_hash.to_string(),
        branch: branch.to_string(),
        timestamp: timestamp.to_string(),
    }
}

fn repository_metadata(repo: &Repository) -> (String, String, String) {
    match repo.head() {
        Ok(head) => metadata_from_head(head),
        Err(err) => {
            log::warn!("Git HEAD unavailable: {}", err);
            unknown_repository_metadata()
        }
    }
}

fn metadata_from_head(head: git2::Reference<'_>) -> (String, String, String) {
    let branch_name = head.shorthand().unwrap_or("unknown").to_string();

    let commit = match head.peel_to_commit() {
        Ok(commit) => commit,
        Err(err) => {
            log::warn!("Git HEAD commit unavailable: {}", err);
            return (branch_name, String::new(), FALLBACK_TIMESTAMP.to_string());
        }
    };

    let timestamp = commit_timestamp(&commit).unwrap_or_else(|| FALLBACK_TIMESTAMP.to_string());
    (branch_name, commit.id().to_string(), timestamp)
}

fn unknown_repository_metadata() -> (String, String, String) {
    (
        "unknown".to_string(),
        String::new(),
        FALLBACK_TIMESTAMP.to_string(),
    )
}

fn commit_timestamp(commit: &git2::Commit) -> Option<String> {
    DateTime::<Utc>::from_timestamp(commit.time().seconds(), 0).map(|dt| dt.to_rfc3339())
}

fn percentile_f64(sorted_values: &[f64], value: f64, total: f64) -> f64 {
    if sorted_values.len() <= 1 {
        return 100.0;
    }

    let idx = sorted_values
        .binary_search_by(|probe| {
            if probe.total_cmp(&value) == CmpOrdering::Less {
                CmpOrdering::Less
            } else {
                CmpOrdering::Greater
            }
        })
        .unwrap_or_else(|idx| idx);
    (idx as f64 / (total - 1.0)) * 100.0
}

fn percentile_u32(sorted_values: &[u32], value: u32, total: f64) -> f64 {
    if sorted_values.len() <= 1 {
        return 100.0;
    }

    let idx = match sorted_values.binary_search_by(|probe| {
        if *probe < value {
            CmpOrdering::Less
        } else {
            CmpOrdering::Greater
        }
    }) {
        Ok(idx) | Err(idx) => idx,
    };
    (idx as f64 / (total - 1.0)) * 100.0
}

fn normalized_value(value: f64, cap: f64) -> f64 {
    ((1.0 + value).ln() / (1.0 + cap).ln()).min(1.0)
}

fn stable_content_hash(bytes: &[u8]) -> String {
    format!("{:032x}", xxh3_128(bytes))
}

fn sort_functions(
    functions: &mut [FunctionMetrics],
    sort_by: &str,
    warnings: &mut Vec<AnalysisWarning>,
) {
    match SortKey::from_str(sort_by) {
        Some(SortKey::Location) => sort_by_location(functions),
        Some(sort_key) => functions.sort_by(|a, b| sort_key.compare(a, b)),
        None => {
            warnings.push(AnalysisWarning {
                code: "unsupported_sort".to_string(),
                message: format!("Unsupported sort field '{sort_by}'. Falling back to file order."),
            });
            sort_by_location(functions);
        }
    }
}

enum SortKey {
    Location,
    Churn,
    Risk,
    Cognitive,
    Cyclomatic,
    LinesOfCode,
}

impl SortKey {
    fn from_str(sort_by: &str) -> Option<Self> {
        match sort_by {
            "file" | "location" => Some(Self::Location),
            "churn_score" | "churn" => Some(Self::Churn),
            "risk" | "risk_score" => Some(Self::Risk),
            "cognitive" | "cognitive_complexity" => Some(Self::Cognitive),
            "cyclomatic" | "cyclomatic_complexity" => Some(Self::Cyclomatic),
            "loc" | "lines_of_code" => Some(Self::LinesOfCode),
            _ => None,
        }
    }

    fn compare(&self, a: &FunctionMetrics, b: &FunctionMetrics) -> CmpOrdering {
        match self {
            Self::Location => location_order(a, b),
            _ => self
                .metric_ordering(a, b)
                .then_with(|| location_order(a, b)),
        }
    }

    fn metric_ordering(&self, a: &FunctionMetrics, b: &FunctionMetrics) -> CmpOrdering {
        match self {
            Self::Location => CmpOrdering::Equal,
            Self::Churn | Self::Risk => self.float_metric_ordering(a, b),
            Self::Cognitive | Self::Cyclomatic | Self::LinesOfCode => {
                self.integer_metric_ordering(a, b)
            }
        }
    }

    fn float_metric_ordering(&self, a: &FunctionMetrics, b: &FunctionMetrics) -> CmpOrdering {
        match self {
            Self::Churn => b.churn_score.total_cmp(&a.churn_score),
            Self::Risk => risk_score(b).total_cmp(&risk_score(a)),
            _ => CmpOrdering::Equal,
        }
    }

    fn integer_metric_ordering(&self, a: &FunctionMetrics, b: &FunctionMetrics) -> CmpOrdering {
        match self {
            Self::Cognitive => b.cognitive_complexity.cmp(&a.cognitive_complexity),
            Self::Cyclomatic => b.cyclomatic_complexity.cmp(&a.cyclomatic_complexity),
            Self::LinesOfCode => b.lines_of_code.cmp(&a.lines_of_code),
            _ => CmpOrdering::Equal,
        }
    }
}

fn sort_by_location(functions: &mut [FunctionMetrics]) {
    functions.sort_by(location_order);
}

fn location_order(a: &FunctionMetrics, b: &FunctionMetrics) -> CmpOrdering {
    a.file
        .cmp(&b.file)
        .then(a.name.cmp(&b.name))
        .then(a.line.cmp(&b.line))
}

fn risk_score(function: &FunctionMetrics) -> f64 {
    function
        .risk
        .as_ref()
        .map(|risk| risk.final_score)
        .unwrap_or(0.0)
}

fn build_project_stats(
    functions: &[FunctionMetrics],
    git_cache: &HashMap<String, GitCacheEntry>,
    total_unique_authors: usize,
) -> metrics::ProjectStats {
    let author_contributions = author_contributions(git_cache);

    metrics::ProjectStats {
        total_unique_authors,
        bus_factor: bus_factor(author_contributions),
        tech_debt_density: tech_debt_density(functions),
        top_hotspots: top_hotspots(functions),
        dead_code: build_dead_code_stats(functions),
    }
}

fn author_contributions(git_cache: &HashMap<String, GitCacheEntry>) -> HashMap<String, usize> {
    let mut author_contributions: HashMap<String, usize> = HashMap::new();
    for entry in git_cache.values() {
        if entry.line_changes.is_empty() {
            add_file_author_contributions(&mut author_contributions, entry);
        } else {
            add_line_author_contributions(&mut author_contributions, entry);
        }
    }
    author_contributions
}

fn add_file_author_contributions(
    author_contributions: &mut HashMap<String, usize>,
    entry: &GitCacheEntry,
) {
    for author in &entry.authors {
        *author_contributions.entry(author.clone()).or_default() += entry.times_modified;
    }
}

fn add_line_author_contributions(
    author_contributions: &mut HashMap<String, usize>,
    entry: &GitCacheEntry,
) {
    for change in &entry.line_changes {
        *author_contributions
            .entry(change.author.clone())
            .or_default() += 1;
    }
}

fn bus_factor(author_contributions: HashMap<String, usize>) -> usize {
    let total_contributions = author_contributions.values().sum::<usize>();
    if total_contributions == 0 {
        return 0;
    }

    let mut counts = author_contributions.into_values().collect::<Vec<_>>();
    counts.sort_by(|a, b| b.cmp(a));
    let majority = total_contributions.div_ceil(2);
    let mut accumulated = 0usize;
    let mut authors = 0usize;
    for count in counts {
        accumulated += count;
        authors += 1;
        if accumulated >= majority {
            break;
        }
    }
    authors
}

fn tech_debt_density(functions: &[FunctionMetrics]) -> f64 {
    let total_loc = functions
        .iter()
        .map(|function| function.lines_of_code)
        .sum::<u32>();
    let total_complexity = functions
        .iter()
        .map(|function| function.cognitive_complexity + function.cyclomatic_complexity)
        .sum::<u32>();
    if total_loc == 0 {
        0.0
    } else {
        total_complexity as f64 / total_loc as f64
    }
}

fn top_hotspots(functions: &[FunctionMetrics]) -> Vec<metrics::Hotspot> {
    let mut hotspots = functions
        .iter()
        .map(|function| metrics::Hotspot {
            id: function.id.clone(),
            name: function.name.clone(),
            file: function.file.clone(),
            line: function.line,
            risk_score: risk_score(function),
            churn_score: function.churn_score,
            cognitive_complexity: function.cognitive_complexity,
        })
        .collect::<Vec<_>>();
    hotspots.sort_by(|a, b| {
        b.risk_score
            .total_cmp(&a.risk_score)
            .then_with(|| b.churn_score.total_cmp(&a.churn_score))
            .then_with(|| b.cognitive_complexity.cmp(&a.cognitive_complexity))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    hotspots.truncate(5);
    hotspots
}

fn build_dead_code_stats(functions: &[FunctionMetrics]) -> metrics::DeadCodeStats {
    let unreachable = functions
        .iter()
        .filter(|function| !function.reachability.is_reachable)
        .collect::<Vec<_>>();
    let functions = unreachable
        .iter()
        .map(|function| metrics::DeadCodeFunction {
            id: function.id.clone(),
            name: function.name.clone(),
            file: function.file.clone(),
            line: function.line,
            lines_of_code: function.lines_of_code,
            kind: function.reachability.kind.clone(),
            safe_to_delete: function.reachability.kind == "unreachable_private",
        })
        .collect();

    metrics::DeadCodeStats {
        unreachable_functions: unreachable.len(),
        unreachable_loc: unreachable
            .iter()
            .map(|function| function.lines_of_code)
            .sum(),
        safe_to_delete: unreachable
            .iter()
            .filter(|function| function.reachability.kind == "unreachable_private")
            .count(),
        functions,
    }
}

fn build_project_coverage(functions: &[FunctionMetrics]) -> Option<metrics::ProjectCoverage> {
    let covered = functions
        .iter()
        .filter_map(|function| function.coverage.as_ref())
        .collect::<Vec<_>>();
    if covered.is_empty() {
        return None;
    }

    let project_line_coverage = covered
        .iter()
        .map(|coverage| coverage.line_coverage)
        .sum::<f64>()
        / covered.len() as f64;
    let high_risk_uncovered = functions
        .iter()
        .filter(|function| {
            risk_score(function) >= 0.80
                && function
                    .coverage
                    .as_ref()
                    .is_some_and(|coverage| coverage.line_coverage < 0.50)
        })
        .count();

    Some(metrics::ProjectCoverage {
        available: true,
        project_line_coverage,
        high_risk_uncovered,
    })
}

fn apply_coupling_and_reachability(functions: &mut [FunctionMetrics]) {
    let by_name = function_indexes_by_name(functions);
    let (mut callers_by_index, mut callees_by_index, reachable) =
        collect_coupling_links(functions, &by_name);

    for (index, function) in functions.iter_mut().enumerate() {
        sort_and_dedup(&mut callers_by_index[index]);
        sort_and_dedup(&mut callees_by_index[index]);

        apply_coupling_metrics(function, &callers_by_index[index], &callees_by_index[index]);
        mark_reachable_if_needed(function, reachable[index]);
    }
}

fn function_indexes_by_name(functions: &[FunctionMetrics]) -> HashMap<String, Vec<usize>> {
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, function) in functions.iter().enumerate() {
        by_name
            .entry(function.name.clone())
            .or_default()
            .push(index);
    }
    by_name
}

fn collect_coupling_links(
    functions: &[FunctionMetrics],
    by_name: &HashMap<String, Vec<usize>>,
) -> (Vec<Vec<String>>, Vec<Vec<String>>, Vec<bool>) {
    let mut callers_by_index: Vec<Vec<String>> = vec![Vec::new(); functions.len()];
    let mut callees_by_index: Vec<Vec<String>> = vec![Vec::new(); functions.len()];
    let mut reachable = vec![false; functions.len()];

    for caller_index in 0..functions.len() {
        let caller_ref = function_ref(&functions[caller_index]);
        let raw_callees = functions[caller_index].coupling.callees.clone();
        for callee_name in raw_callees {
            let Some(targets) = by_name.get(&callee_name) else {
                continue;
            };
            collect_callee_targets(
                functions,
                caller_index,
                &caller_ref,
                targets,
                &mut callers_by_index,
                &mut callees_by_index,
                &mut reachable,
            );
        }
    }

    (callers_by_index, callees_by_index, reachable)
}

fn collect_callee_targets(
    functions: &[FunctionMetrics],
    caller_index: usize,
    caller_ref: &str,
    targets: &[usize],
    callers_by_index: &mut [Vec<String>],
    callees_by_index: &mut [Vec<String>],
    reachable: &mut [bool],
) {
    for &callee_index in targets {
        if callee_index == caller_index {
            continue;
        }
        reachable[callee_index] = true;
        callers_by_index[callee_index].push(caller_ref.to_string());
        callees_by_index[caller_index].push(function_ref(&functions[callee_index]));
    }
}

fn apply_coupling_metrics(function: &mut FunctionMetrics, callers: &[String], callees: &[String]) {
    let fan_in = callers.len();
    let fan_out = callees.len();
    let denominator = fan_in + fan_out;
    function.coupling = metrics::CouplingMetrics {
        fan_in,
        fan_out,
        callers: callers.to_vec(),
        callees: callees.to_vec(),
        instability: if denominator == 0 {
            0.0
        } else {
            fan_out as f64 / denominator as f64
        },
    };
}

fn mark_reachable_if_needed(function: &mut FunctionMetrics, reachable: bool) {
    let public_or_exported = function.reachability.kind == "unreachable_export";
    let test_entry = function.reachability.kind == "test_entry" || is_test_file(function);
    if reachable || public_or_exported || test_entry || function.name == "main" {
        function.reachability.is_reachable = true;
        function.reachability.kind = if test_entry {
            "test_only".to_string()
        } else {
            reachability_kind(function)
        };
    }
}

fn is_test_file(function: &FunctionMetrics) -> bool {
    function.file.contains("/tests/")
        || function.file.ends_with("_test.rs")
        || function.file.ends_with(".test.ts")
        || function.file.ends_with(".spec.ts")
}

fn reachability_kind(function: &FunctionMetrics) -> String {
    if is_test_file(function) {
        "test_only".to_string()
    } else {
        "reachable".to_string()
    }
}

fn apply_coverage(functions: &mut [FunctionMetrics], coverage: Option<&CoverageIndex>) {
    let Some(coverage) = coverage else {
        return;
    };

    for function in functions {
        let Some(file_coverage) = coverage.files.get(&function.file) else {
            continue;
        };

        function.coverage = Some(function_coverage(function, file_coverage));
    }
}

fn function_coverage(
    function: &FunctionMetrics,
    file_coverage: &FileCoverageData,
) -> metrics::CoverageMetrics {
    let line_coverage = line_coverage(function, file_coverage);
    metrics::CoverageMetrics {
        available: true,
        line_coverage,
        branch_coverage: branch_coverage(function, file_coverage),
        covered_by: Vec::new(),
        risk_coverage_gap: 1.0 - line_coverage,
    }
}

fn line_coverage(function: &FunctionMetrics, file_coverage: &FileCoverageData) -> f64 {
    let covered_lines = file_coverage
        .lines
        .iter()
        .filter(|(line, hits)| **line >= function.line && **line <= function.end_line && **hits > 0)
        .count();
    let relevant_lines = file_coverage
        .lines
        .keys()
        .filter(|line| **line >= function.line && **line <= function.end_line)
        .count();

    if relevant_lines == 0 {
        0.0
    } else {
        covered_lines as f64 / relevant_lines as f64
    }
}

fn branch_coverage(function: &FunctionMetrics, file_coverage: &FileCoverageData) -> Option<f64> {
    let branch_total = file_coverage
        .branches
        .iter()
        .filter(|branch| branch.line >= function.line && branch.line <= function.end_line)
        .count();

    if branch_total == 0 {
        return None;
    }

    let branch_covered = file_coverage
        .branches
        .iter()
        .filter(|branch| {
            branch.line >= function.line && branch.line <= function.end_line && branch.taken
        })
        .count();
    Some(branch_covered as f64 / branch_total as f64)
}

fn update_coverage_gaps(functions: &mut [FunctionMetrics]) {
    for function in functions {
        let risk_score = risk_score(function);
        if let Some(coverage) = function.coverage.as_mut() {
            coverage.risk_coverage_gap = risk_score * (1.0 - coverage.line_coverage);
        }
    }
}

fn function_ref(function: &FunctionMetrics) -> String {
    format!("{}:{}", function.file, function.name)
}

fn sort_and_dedup(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

fn build_quality(
    git: GitAnalysisStatus,
    cache_enabled: bool,
    cache_loaded: bool,
    cache_saved: bool,
    cache_stats: CacheStats,
    warnings: Vec<AnalysisWarning>,
    skipped_files: Vec<SkippedFile>,
) -> AnalysisQuality {
    let status = if warnings.is_empty() && skipped_files.is_empty() && !git.partial {
        AnalysisStatus::Complete
    } else {
        AnalysisStatus::Partial
    };

    AnalysisQuality {
        status,
        git,
        cache: CacheAnalysisStatus {
            enabled: cache_enabled,
            loaded: cache_loaded,
            saved: cache_saved,
            ast_hits: cache_stats.hits,
            ast_misses: cache_stats.misses,
        },
        warnings,
        skipped_files,
    }
}

#[derive(Default)]
struct CacheStats {
    hits: usize,
    misses: usize,
}

#[derive(Default)]
struct WorkerOutput {
    functions: Vec<FunctionMetrics>,
    cache_entry: Option<(String, FileCacheEntry)>,
    active_path: Option<String>,
    cache_hit: usize,
    cache_miss: usize,
    warnings: Vec<AnalysisWarning>,
    skipped_files: Vec<SkippedFile>,
}

#[derive(Default)]
struct WorkerAccumulator {
    functions: Vec<FunctionMetrics>,
    cache_entries: HashMap<String, FileCacheEntry>,
    active_paths: HashSet<String>,
    cache_hits: usize,
    cache_misses: usize,
    warnings: Vec<AnalysisWarning>,
    skipped_files: Vec<SkippedFile>,
}

struct WorkerResult {
    functions: Vec<FunctionMetrics>,
    cache_entries: HashMap<String, FileCacheEntry>,
    active_paths: HashSet<String>,
    cache_stats: CacheStats,
    warnings: Vec<AnalysisWarning>,
    skipped_files: Vec<SkippedFile>,
}

#[derive(Clone)]
struct FileReadLimiter {
    sender: Sender<()>,
    receiver: Receiver<()>,
}

struct FileReadPermit {
    sender: Sender<()>,
}

enum FileContent {
    Bytes(Vec<u8>),
    Mmap(Mmap),
}

impl FileReadLimiter {
    fn new(limit: usize) -> Self {
        let limit = limit.max(1);
        let (sender, receiver) = bounded(limit);
        for _ in 0..limit {
            sender
                .send(())
                .expect("file read limiter should accept initial permits");
        }
        Self { sender, receiver }
    }

    fn acquire(&self) -> Result<FileReadPermit> {
        self.receiver.recv()?;
        Ok(FileReadPermit {
            sender: self.sender.clone(),
        })
    }
}

impl Drop for FileReadPermit {
    fn drop(&mut self) {
        let _ = self.sender.send(());
    }
}

impl FileContent {
    fn read(path: &Path, limiter: &FileReadLimiter) -> Result<Self> {
        let _permit = limiter.acquire()?;
        let metadata = std::fs::metadata(path)?;
        if metadata.len() >= MMAP_FILE_THRESHOLD_BYTES {
            let file = File::open(path)?;
            // SAFETY: The file is opened read-only and the mapping is only exposed as immutable bytes.
            let mmap = unsafe { MmapOptions::new().map(&file)? };
            Ok(Self::Mmap(mmap))
        } else {
            Ok(Self::Bytes(std::fs::read(path)?))
        }
    }

    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::Mmap(mmap) => mmap,
        }
    }
}

impl WorkerAccumulator {
    fn into_parts(self) -> WorkerResult {
        WorkerResult {
            functions: self.functions,
            cache_entries: self.cache_entries,
            active_paths: self.active_paths,
            cache_stats: CacheStats {
                hits: self.cache_hits,
                misses: self.cache_misses,
            },
            warnings: self.warnings,
            skipped_files: self.skipped_files,
        }
    }
}

struct AnalysisWorker;
impl AnalysisWorker {
    fn process_file_from_source(
        source: &[u8],
        rel_path_str: &str,
        registry: &LanguageRegistry,
    ) -> Result<Vec<FunctionMetrics>> {
        let support = registry
            .get_support(rel_path_str)
            .ok_or_else(|| anyhow::anyhow!("Unsupported language for {}", rel_path_str))?;
        let source_str = std::str::from_utf8(source)?;
        AstParser::analyze_source(source_str, rel_path_str, support)
    }
}

fn load_repository_context(
    repo_path: &Path,
    warnings: &mut Vec<AnalysisWarning>,
) -> Result<RepoContext> {
    let repo_path_abs = repo_path.canonicalize()?;
    let repo = open_repository(&repo_path_abs, warnings);
    let (branch, commit_hash, timestamp) = repository_context_metadata(repo.as_ref());
    let cache_manager = initialize_cache_manager(&repo_path_abs, warnings);
    let cache = load_analysis_cache(&cache_manager, warnings);
    let cache_loaded = cache_manager.is_some() && analysis_cache_has_entries(&cache);
    let config = load_config(&repo_path_abs, warnings);
    let coverage = load_coverage_index(&repo_path_abs, warnings);

    Ok(RepoContext {
        repo,
        branch,
        commit_hash,
        timestamp,
        cache_manager,
        cache,
        cache_loaded,
        repo_path_abs,
        config,
        coverage,
    })
}

fn open_repository(
    repo_path_abs: &Path,
    warnings: &mut Vec<AnalysisWarning>,
) -> Option<Repository> {
    match Repository::open(repo_path_abs) {
        Ok(repo) => Some(repo),
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "git_unavailable".to_string(),
                message: format!("Git repository unavailable: {err}"),
            });
            None
        }
    }
}

fn repository_context_metadata(repo: Option<&Repository>) -> (String, String, String) {
    match repo {
        Some(repo) => repository_metadata(repo),
        None => (
            "unknown".to_string(),
            String::new(),
            FALLBACK_TIMESTAMP.to_string(),
        ),
    }
}

fn initialize_cache_manager(
    repo_path_abs: &Path,
    warnings: &mut Vec<AnalysisWarning>,
) -> Option<CacheManager> {
    match CacheManager::new(repo_path_abs) {
        Ok(cm) => Some(cm),
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "cache_unavailable".to_string(),
                message: format!("Failed to initialize cache: {err}"),
            });
            None
        }
    }
}

fn load_analysis_cache(
    cache_manager: &Option<CacheManager>,
    warnings: &mut Vec<AnalysisWarning>,
) -> AnalysisCache {
    let Some(cm) = cache_manager else {
        return AnalysisCache::default();
    };

    match cm.load() {
        Ok(cache) => cache,
        Err(err) => {
            warnings.push(cache_load_failed_warning(err));
            AnalysisCache::default()
        }
    }
}

fn cache_load_failed_warning(err: anyhow::Error) -> AnalysisWarning {
    AnalysisWarning {
        code: "cache_load_failed".to_string(),
        message: format!("Failed to load cache: {err}"),
    }
}

fn analysis_cache_has_entries(cache: &AnalysisCache) -> bool {
    !cache.files.is_empty() || !cache.git_cache.is_empty() || cache.last_commit_oid.is_some()
}

fn load_config(repo_path: &Path, warnings: &mut Vec<AnalysisWarning>) -> UchikomiConfig {
    let config_path = repo_path.join("uchikomi.toml");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return UchikomiConfig::default();
        }
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "config_read_failed".to_string(),
                message: format!("Failed to read {}: {err}", config_path.display()),
            });
            return UchikomiConfig::default();
        }
    };

    match toml::from_str(&content) {
        Ok(config) => config,
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "config_parse_failed".to_string(),
                message: format!("Failed to parse {}: {err}", config_path.display()),
            });
            UchikomiConfig::default()
        }
    }
}

fn load_coverage_index(
    repo_path: &Path,
    warnings: &mut Vec<AnalysisWarning>,
) -> Option<CoverageIndex> {
    let coverage_path = repo_path.join("coverage").join("lcov.info");
    let content = match std::fs::read_to_string(&coverage_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "coverage_read_failed".to_string(),
                message: format!("Failed to read {}: {err}", coverage_path.display()),
            });
            return None;
        }
    };

    match parse_lcov(&content, repo_path) {
        Ok(index) => Some(index),
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "coverage_parse_failed".to_string(),
                message: format!("Failed to parse {}: {err}", coverage_path.display()),
            });
            None
        }
    }
}

fn parse_lcov(content: &str, repo_path: &Path) -> Result<CoverageIndex> {
    let mut index = CoverageIndex::default();
    let mut current_file: Option<String> = None;
    let mut current_data = FileCoverageData::default();

    for line in content.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            start_coverage_record(
                &mut index,
                &mut current_file,
                &mut current_data,
                path,
                repo_path,
            );
            continue;
        }

        if let Some(data) = line.strip_prefix("DA:") {
            add_line_coverage(&mut current_data, data);
            continue;
        }

        if let Some(data) = line.strip_prefix("BRDA:") {
            add_branch_coverage(&mut current_data, data);
            continue;
        }

        if line == "end_of_record" {
            finish_coverage_record(&mut index, &mut current_file, &mut current_data);
        }
    }

    finish_coverage_record(&mut index, &mut current_file, &mut current_data);

    Ok(index)
}

fn start_coverage_record(
    index: &mut CoverageIndex,
    current_file: &mut Option<String>,
    current_data: &mut FileCoverageData,
    path: &str,
    repo_path: &Path,
) {
    if let Some(file) = current_file.replace(normalize_coverage_path(path, repo_path)) {
        index.files.insert(file, std::mem::take(current_data));
    }
}

fn finish_coverage_record(
    index: &mut CoverageIndex,
    current_file: &mut Option<String>,
    current_data: &mut FileCoverageData,
) {
    if let Some(file) = current_file.take() {
        index.files.insert(file, std::mem::take(current_data));
    }
}

fn add_line_coverage(current_data: &mut FileCoverageData, data: &str) {
    if let Some((line_no, hits)) = parse_line_coverage(data) {
        current_data.lines.insert(line_no, hits);
    }
}

fn add_branch_coverage(current_data: &mut FileCoverageData, data: &str) {
    if let Some(branch) = parse_branch_coverage(data) {
        current_data.branches.push(branch);
    }
}

fn normalize_coverage_path(path: &str, repo_path: &Path) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        path.strip_prefix(repo_path)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    } else {
        path.to_string_lossy().to_string()
    }
}

fn parse_line_coverage(data: &str) -> Option<(u32, u32)> {
    let mut parts = data.split(',');
    let line = parts.next()?.parse().ok()?;
    let hits = parts.next()?.parse().ok()?;
    Some((line, hits))
}

fn parse_branch_coverage(data: &str) -> Option<BranchCoverageData> {
    let mut parts = data.split(',');
    let line = parts.next()?.parse().ok()?;
    let _block = parts.next()?;
    let _branch = parts.next()?;
    let taken = parts.next()? != "-";
    Some(BranchCoverageData { line, taken })
}

fn collect_git_analysis(
    repo: Option<&Repository>,
    repo_path_abs: &Path,
    branch: &str,
    commit_hash: &str,
    cache: &mut AnalysisCache,
    bug_fix_patterns: &BugFixPatterns,
    warnings: &mut Vec<AnalysisWarning>,
) -> (HashMap<String, GitCacheEntry>, GitAnalysisStatus) {
    let mut git_status = GitAnalysisStatus {
        available: repo.is_some(),
        partial: false,
        cache_reset: false,
        processed_commits: 0,
    };

    let Some(repo) = repo else {
        return (HashMap::new(), git_status);
    };

    let git_cache = match GitAnalyzer::get_all_file_metrics(
        repo,
        std::mem::take(&mut cache.git_cache),
        cache.git_metadata.clone(),
        cache.last_commit_oid.clone(),
        GitAnalysisContext {
            repo_path: repo_path_abs,
            branch,
            head_oid: commit_hash,
        },
        bug_fix_patterns,
    ) {
        Ok(git_result) => apply_git_metrics_result(git_result, cache, &mut git_status, warnings),
        Err(err) => {
            git_status.partial = true;
            warnings.push(git_metrics_failed_warning(err));
            HashMap::new()
        }
    };

    (git_cache, git_status)
}

fn apply_git_metrics_result(
    git_result: git::GitMetricsResult,
    cache: &mut AnalysisCache,
    git_status: &mut GitAnalysisStatus,
    warnings: &mut Vec<AnalysisWarning>,
) -> HashMap<String, GitCacheEntry> {
    git_status.partial = git_result.partial;
    git_status.cache_reset = git_result.cache_reset;
    git_status.processed_commits = git_result.processed_commits;
    for warning in git_result.warnings {
        warnings.push(AnalysisWarning {
            code: "git_partial".to_string(),
            message: warning,
        });
    }
    cache.git_metadata = Some(git_result.metadata);
    git_result.cache
}

fn git_metrics_failed_warning(err: anyhow::Error) -> AnalysisWarning {
    AnalysisWarning {
        code: "git_metrics_failed".to_string(),
        message: format!("Failed to collect Git metrics: {err}"),
    }
}

fn analyze_files_parallel(
    repo_path_abs: &Path,
    registry: Arc<LanguageRegistry>,
    cache: &AnalysisCache,
    git_cache: &HashMap<String, GitCacheEntry>,
    include_authors: bool,
    shutdown: Arc<AtomicBool>,
) -> WorkerResult {
    let file_read_limiter = Arc::new(FileReadLimiter::new(MAX_PARALLEL_FILE_READS));
    let walker = WalkBuilder::new(repo_path_abs)
        .standard_filters(true)
        .hidden(false)
        .build();

    let ctx = WorkerContext {
        repo_path_abs,
        registry: Arc::clone(&registry),
        cache,
        git_cache,
        include_authors,
        file_read_limiter: Arc::clone(&file_read_limiter),
        shutdown: &shutdown,
    };

    walker
        .par_bridge()
        .filter_map(|entry| process_walk_entry(entry, &ctx))
        .fold(WorkerAccumulator::default, |mut acc, output| {
            acc.functions.extend(output.functions);
            if let Some((path, entry)) = output.cache_entry {
                acc.cache_entries.insert(path, entry);
            }
            if let Some(path) = output.active_path {
                acc.active_paths.insert(path);
            }
            acc.cache_hits += output.cache_hit;
            acc.cache_misses += output.cache_miss;
            acc.warnings.extend(output.warnings);
            acc.skipped_files.extend(output.skipped_files);
            acc
        })
        .reduce(WorkerAccumulator::default, |mut acc, output| {
            acc.functions.extend(output.functions);
            acc.cache_entries.extend(output.cache_entries);
            acc.active_paths.extend(output.active_paths);
            acc.cache_hits += output.cache_hits;
            acc.cache_misses += output.cache_misses;
            acc.warnings.extend(output.warnings);
            acc.skipped_files.extend(output.skipped_files);
            acc
        })
        .into_parts()
}

fn process_walk_entry(
    entry: std::result::Result<ignore::DirEntry, ignore::Error>,
    ctx: &WorkerContext,
) -> Option<WorkerOutput> {
    let entry = match prepare_walk_entry(entry, ctx.shutdown)? {
        Ok(entry) => entry,
        Err(output) => return Some(output),
    };
    let path = entry.path();
    let rel_path_str =
        match supported_relative_path(path, ctx.repo_path_abs, ctx.registry.as_ref())? {
            Ok(path) => path,
            Err(output) => return Some(*output),
        };
    analyze_supported_file(
        path,
        rel_path_str,
        Arc::clone(&ctx.registry),
        ctx.cache,
        ctx.git_cache,
        ctx.include_authors,
        Arc::clone(&ctx.file_read_limiter),
    )
}

fn prepare_walk_entry(
    entry: std::result::Result<ignore::DirEntry, ignore::Error>,
    shutdown: &AtomicBool,
) -> Option<Result<ignore::DirEntry, WorkerOutput>> {
    if shutdown.load(Ordering::Relaxed) {
        return None;
    }

    Some(entry.map_err(walk_entry_failed_output))
}

fn supported_relative_path(
    path: &Path,
    repo_path_abs: &Path,
    registry: &LanguageRegistry,
) -> Option<Result<String, Box<WorkerOutput>>> {
    if !path.is_file() {
        return None;
    }

    let rel_path_str = match relative_path_string(path, repo_path_abs) {
        Ok(path) => path,
        Err(err) => return Some(Err(err)),
    };

    registry
        .get_support(&rel_path_str)
        .is_some()
        .then_some(Ok(rel_path_str))
}

fn walk_entry_failed_output(err: ignore::Error) -> WorkerOutput {
    WorkerOutput {
        warnings: vec![AnalysisWarning {
            code: "walk_entry_failed".to_string(),
            message: format!("Failed to read directory entry: {err}"),
        }],
        ..WorkerOutput::default()
    }
}

fn relative_path_string(path: &Path, repo_path_abs: &Path) -> Result<String, Box<WorkerOutput>> {
    match path.strip_prefix(repo_path_abs) {
        Ok(path) => Ok(path.to_string_lossy().to_string()),
        Err(err) => Err(Box::new(WorkerOutput {
            warnings: vec![AnalysisWarning {
                code: "path_error".to_string(),
                message: format!("Failed to strip prefix for {}: {}", path.display(), err),
            }],
            ..WorkerOutput::default()
        })),
    }
}

fn analyze_supported_file(
    path: &Path,
    rel_path_str: String,
    registry: Arc<LanguageRegistry>,
    cache: &AnalysisCache,
    git_cache: &HashMap<String, GitCacheEntry>,
    include_authors: bool,
    file_read_limiter: Arc<FileReadLimiter>,
) -> Option<WorkerOutput> {
    let mut output = WorkerOutput {
        active_path: Some(rel_path_str.clone()),
        ..WorkerOutput::default()
    };

    let Some(source) = read_supported_file(path, &rel_path_str, &file_read_limiter, &mut output)
    else {
        return Some(output);
    };
    let source_bytes = source.as_bytes();
    let content_hash = stable_content_hash(source_bytes);
    let mut functions = match cached_or_analyzed_functions(
        cache,
        &rel_path_str,
        source_bytes,
        &content_hash,
        &registry,
        &mut output,
    ) {
        Some(functions) => functions,
        None => return Some(output),
    };

    apply_git_metrics_to_functions(&mut functions, git_cache, &rel_path_str, include_authors);
    output.cache_entry = Some((
        rel_path_str,
        FileCacheEntry {
            content_hash,
            functions: functions.clone(),
        },
    ));
    output.functions = functions;

    Some(output)
}

fn read_supported_file(
    path: &Path,
    rel_path_str: &str,
    file_read_limiter: &FileReadLimiter,
    output: &mut WorkerOutput,
) -> Option<FileContent> {
    match FileContent::read(path, file_read_limiter) {
        Ok(source) => Some(source),
        Err(err) => {
            output.skipped_files.push(SkippedFile {
                path: rel_path_str.to_string(),
                reason: format!("failed to read file: {err}"),
            });
            None
        }
    }
}

fn cached_or_analyzed_functions(
    cache: &AnalysisCache,
    rel_path_str: &str,
    source_bytes: &[u8],
    content_hash: &str,
    registry: &LanguageRegistry,
    output: &mut WorkerOutput,
) -> Option<Vec<FunctionMetrics>> {
    if let Some(cached_entry) = cache.files.get(rel_path_str) {
        if cached_entry.content_hash == content_hash {
            output.cache_hit = 1;
            return Some(cached_entry.functions.clone());
        }
    }

    output.cache_miss = 1;
    match AnalysisWorker::process_file_from_source(source_bytes, rel_path_str, registry) {
        Ok(functions) => Some(functions),
        Err(err) => {
            output.skipped_files.push(SkippedFile {
                path: rel_path_str.to_string(),
                reason: format!("failed to analyze file: {err}"),
            });
            None
        }
    }
}

fn apply_git_metrics_to_functions(
    functions: &mut [FunctionMetrics],
    git_cache: &HashMap<String, GitCacheEntry>,
    rel_path_str: &str,
    include_authors: bool,
) {
    if let Some(entry) = git_cache.get(rel_path_str) {
        for func in functions {
            let churn =
                GitAnalyzer::compute_churn_metrics_for_range(entry, func.line, func.end_line);
            func.times_modified = churn.times_modified;
            func.bug_fix_commits = churn.bug_fix_commits;
            func.authors_count = churn.authors_count;
            func.authors = if include_authors {
                Some(churn.authors.clone())
            } else {
                None
            };
            func.churn = GitAnalyzer::churn_details(&churn);
            func.churn_score = churn.churn_score;
            func.file = rel_path_str.to_string();
        }
    }
}

fn compute_scoring_context(functions: &[FunctionMetrics]) -> ScoringContext {
    let max_values = MaxValues {
        cyclomatic: functions
            .iter()
            .map(|f| f.cyclomatic_complexity)
            .max()
            .unwrap_or(1),
        cognitive: functions
            .iter()
            .map(|f| f.cognitive_complexity)
            .max()
            .unwrap_or(1),
        churn: functions.iter().map(|f| f.churn_score).fold(0.0, f64::max),
        loc: functions.iter().map(|f| f.lines_of_code).max().unwrap_or(1),
    };

    let mut cog_vals: Vec<u32> = functions.iter().map(|f| f.cognitive_complexity).collect();
    let mut churn_vals: Vec<f64> = functions.iter().map(|f| f.churn_score).collect();
    let mut loc_vals: Vec<u32> = functions.iter().map(|f| f.lines_of_code).collect();
    let mut cyc_vals: Vec<u32> = functions.iter().map(|f| f.cyclomatic_complexity).collect();
    let mut auth_vals: Vec<usize> = functions.iter().map(|f| f.authors_count).collect();
    let mut recent_churn_vals: Vec<f64> =
        functions.iter().map(|f| f.churn.windows.d7.score).collect();
    let mut fan_in_vals: Vec<usize> = functions.iter().map(|f| f.coupling.fan_in).collect();
    let mut coverage_gap_vals: Vec<f64> = functions
        .iter()
        .map(|f| {
            f.coverage
                .as_ref()
                .map(|coverage| coverage.risk_coverage_gap)
                .unwrap_or(0.0)
        })
        .collect();

    cog_vals.sort();
    churn_vals.sort_by(|a, b| a.total_cmp(b));
    loc_vals.sort();
    cyc_vals.sort();
    auth_vals.sort();
    recent_churn_vals.sort_by(|a, b| a.total_cmp(b));
    fan_in_vals.sort();
    coverage_gap_vals.sort_by(|a, b| a.total_cmp(b));

    let p95_idx = (functions.len() * 95 / 100).min(functions.len() - 1);
    let p99_idx = (functions.len() * 99 / 100).min(functions.len() - 1);

    let cognitive_p95 = cog_vals[p95_idx] as f64;
    let churn_p95 = churn_vals[p95_idx];
    let loc_p95 = loc_vals[p95_idx] as f64;
    let cyc_p95 = cyc_vals[p95_idx] as f64;
    let auth_p95 = auth_vals[p95_idx] as f64;
    let recent_churn_p95 = recent_churn_vals[p95_idx];
    let fan_in_p95 = fan_in_vals[p95_idx] as f64;
    let coverage_gap_p95 = coverage_gap_vals[p95_idx];

    let cognitive_p99 = cog_vals[p99_idx] as f64;
    let churn_p99 = churn_vals[p99_idx];
    let loc_p99 = loc_vals[p99_idx] as f64;
    let cyc_p99 = cyc_vals[p99_idx] as f64;
    let auth_p99 = auth_vals[p99_idx] as f64;
    let recent_churn_p99 = recent_churn_vals[p99_idx];
    let fan_in_p99 = fan_in_vals[p99_idx] as f64;
    let coverage_gap_p99 = coverage_gap_vals[p99_idx];

    let cap_cog = if (max_values.cognitive as f64) > 3.0 * cognitive_p95 {
        cognitive_p99
    } else {
        max_values.cognitive as f64
    }
    .max(1.0);
    let cap_churn = if max_values.churn > 3.0 * churn_p95 {
        churn_p99
    } else {
        max_values.churn
    }
    .max(1.0);
    let cap_loc = if (max_values.loc as f64) > 3.0 * loc_p95 {
        loc_p99
    } else {
        max_values.loc as f64
    }
    .max(1.0);
    let cap_cyc = if (max_values.cyclomatic as f64) > 3.0 * cyc_p95 {
        cyc_p99
    } else {
        max_values.cyclomatic as f64
    }
    .max(1.0);
    let cap_auth = if (auth_vals.iter().max().cloned().unwrap_or(0) as f64) > 3.0 * auth_p95 {
        auth_p99
    } else {
        *auth_vals.iter().max().unwrap_or(&0) as f64
    }
    .max(1.0);
    let cap_recent_churn = if *recent_churn_vals.last().unwrap_or(&0.0) > 3.0 * recent_churn_p95 {
        recent_churn_p99
    } else {
        *recent_churn_vals.last().unwrap_or(&0.0)
    }
    .max(1.0);
    let cap_fan_in = if (*fan_in_vals.last().unwrap_or(&0) as f64) > 3.0 * fan_in_p95 {
        fan_in_p99
    } else {
        *fan_in_vals.last().unwrap_or(&0) as f64
    }
    .max(1.0);
    let cap_coverage_gap = if *coverage_gap_vals.last().unwrap_or(&0.0) > 3.0 * coverage_gap_p95 {
        coverage_gap_p99
    } else {
        *coverage_gap_vals.last().unwrap_or(&0.0)
    }
    .max(1.0);

    ScoringContext {
        max_values,
        caps: NormalizationCaps {
            cognitive: cap_cog,
            churn: cap_churn,
            churn_recent: cap_recent_churn,
            loc: cap_loc,
            cyclomatic: cap_cyc,
            authors: cap_auth,
            fan_in: cap_fan_in,
            coverage_gap: cap_coverage_gap,
        },
        cog_vals,
        churn_vals,
        p95_idx,
    }
}

fn apply_scoring_and_labels(
    functions: &mut [FunctionMetrics],
    context: &ScoringContext,
) -> Distributions {
    // Normalize metrics and derive composite risk score.
    for func in functions.iter_mut() {
        apply_normalized_risk(func, context);
    }

    let mut risk_vals: Vec<f64> = functions
        .iter()
        .filter_map(|f| f.risk.as_ref().map(|risk| risk.final_score))
        .collect();
    risk_vals.sort_by(|a, b| a.total_cmp(b));

    let total_funcs = functions.len() as f64;
    for func in functions.iter_mut() {
        apply_percentiles_and_labels(func, context, &risk_vals, total_funcs);
    }

    Distributions {
        risk_p95: risk_vals[context.p95_idx],
        churn_p95: context.churn_vals[context.p95_idx],
        cognitive_p95: context.cog_vals[context.p95_idx] as f64,
    }
}

fn apply_normalized_risk(func: &mut FunctionMetrics, context: &ScoringContext) {
    let norm_cog = normalized_value(func.cognitive_complexity as f64, context.caps.cognitive);
    let norm_cyc = normalized_value(func.cyclomatic_complexity as f64, context.caps.cyclomatic);
    let norm_churn = normalized_value(func.churn_score, context.caps.churn);
    let norm_recent_churn =
        normalized_value(func.churn.windows.d7.score, context.caps.churn_recent);
    let norm_loc = normalized_value(func.lines_of_code as f64, context.caps.loc);
    let norm_auth = normalized_value(func.authors_count as f64, context.caps.authors);
    let norm_fan_in = normalized_value(func.coupling.fan_in as f64, context.caps.fan_in);
    let coverage_gap = func
        .coverage
        .as_ref()
        .map(|coverage| coverage.risk_coverage_gap);
    let norm_coverage_gap =
        coverage_gap.map(|gap| normalized_value(gap, context.caps.coverage_gap));

    func.normalized = Some(NormalizedMetrics {
        cyclomatic: norm_cyc,
        churn: norm_churn,
        churn_recent: norm_recent_churn,
        cognitive: norm_cog,
        fan_in: norm_fan_in,
        loc: norm_loc,
        authors: norm_auth,
        coverage_gap: norm_coverage_gap,
    });

    let base_score = (WEIGHT_COGNITIVE * norm_cog)
        + (WEIGHT_CYCLOMATIC * norm_cyc)
        + (WEIGHT_CHURN * norm_churn)
        + (WEIGHT_CHURN_RECENT * norm_recent_churn)
        + (WEIGHT_FAN_IN * norm_fan_in)
        + (WEIGHT_LOC * norm_loc)
        + (WEIGHT_AUTHORS * norm_auth)
        + (WEIGHT_COVERAGE_GAP * norm_coverage_gap.unwrap_or(0.0));
    let nesting_penalty = 1.0 + (func.nesting_depth as f64 / 4.0).powi(2) * 0.20;
    let fan_in_multiplier = 1.0 + norm_fan_in * 0.25;
    let final_score = base_score * nesting_penalty * fan_in_multiplier;

    func.risk = Some(RiskMetrics {
        base_score,
        nesting_penalty,
        final_score,
        level: String::new(),
        primary_driver: String::new(),
    });
}

fn apply_percentiles_and_labels(
    func: &mut FunctionMetrics,
    context: &ScoringContext,
    risk_vals: &[f64],
    total_funcs: f64,
) {
    let Some(risk_score) = func.risk.as_ref().map(|risk| risk.final_score) else {
        return;
    };
    let churn = func.churn_score;
    let cog = func.cognitive_complexity as f64;

    let risk_pct = percentile_f64(risk_vals, risk_score, total_funcs);

    func.percentile = Some(PercentileMetrics {
        risk: risk_pct,
        churn: percentile_f64(&context.churn_vals, churn, total_funcs),
        cognitive: percentile_u32(&context.cog_vals, cog as u32, total_funcs),
    });

    let level = match risk_pct {
        p if p >= THRESHOLD_CRITICAL => "critical",
        p if p >= THRESHOLD_HIGH => "high",
        p if p >= THRESHOLD_MEDIUM => "medium",
        _ => "low",
    }
    .to_string();

    if let Some(norm) = &func.normalized {
        let mut drivers = [
            ("cognitive", norm.cognitive * WEIGHT_COGNITIVE),
            ("churn", norm.churn * WEIGHT_CHURN),
            ("churn_recent", norm.churn_recent * WEIGHT_CHURN_RECENT),
            ("fan_in", norm.fan_in * WEIGHT_FAN_IN),
            ("cyclomatic", norm.cyclomatic * WEIGHT_CYCLOMATIC),
            ("loc", norm.loc * WEIGHT_LOC),
            ("authors", norm.authors * WEIGHT_AUTHORS),
            (
                "coverage_gap",
                norm.coverage_gap.unwrap_or(0.0) * WEIGHT_COVERAGE_GAP,
            ),
        ];
        drivers.sort_by(|a, b| b.1.total_cmp(&a.1));
        if let Some(risk) = func.risk.as_mut() {
            risk.level = level;
            risk.primary_driver = drivers[0].0.to_string();
        }
    }
}

fn save_analysis_cache(
    cache_manager: &Option<CacheManager>,
    mut cache: AnalysisCache,
    cache_entries: HashMap<String, FileCacheEntry>,
    git_cache: HashMap<String, GitCacheEntry>,
    commit_hash: &str,
    warnings: &mut Vec<AnalysisWarning>,
) -> bool {
    let mut cache_saved = false;
    cache.files = cache_entries;
    cache.git_cache = git_cache;
    cache.last_commit_oid = if commit_hash.is_empty() {
        None
    } else {
        Some(commit_hash.to_string())
    };

    if let Some(cm) = cache_manager {
        match cm.save(cache) {
            Ok(()) => cache_saved = true,
            Err(err) => warnings.push(AnalysisWarning {
                code: "cache_save_failed".to_string(),
                message: format!("Failed to save cache: {err}"),
            }),
        }
    }
    cache_saved
}
