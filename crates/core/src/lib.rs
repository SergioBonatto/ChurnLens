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
use git::{BugFixPatterns, GitAnalyzer};
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

const SCHEMA_VERSION: &str = "0.3.0";
const FALLBACK_TIMESTAMP: &str = "1970-01-01T00:00:00+00:00";

const WEIGHT_COGNITIVE: f64 = 0.35;
const WEIGHT_CYCLOMATIC: f64 = 0.15;
const WEIGHT_CHURN: f64 = 0.30;
const WEIGHT_LOC: f64 = 0.10;
const WEIGHT_AUTHORS: f64 = 0.10;

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
    config: ChurnLensConfig,
}

#[derive(Default, Deserialize)]
struct ChurnLensConfig {
    #[serde(default)]
    git: GitConfig,
}

#[derive(Default, Deserialize)]
struct GitConfig {
    #[serde(default)]
    bug_fix_patterns: Vec<String>,
}

struct NormalizationCaps {
    cognitive: f64,
    churn: f64,
    loc: f64,
    cyclomatic: f64,
    authors: f64,
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
    let bug_fix_patterns = match BugFixPatterns::from_patterns(&ctx.config.git.bug_fix_patterns) {
        Ok(patterns) => patterns,
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "invalid_bug_fix_pattern".to_string(),
                message: format!("Invalid bug-fix pattern configuration: {err}"),
            });
            BugFixPatterns::default()
        }
    };
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

    let scoring_policy = ScoringPolicy {
        weights: metrics::Weights {
            cognitive: WEIGHT_COGNITIVE,
            cyclomatic: WEIGHT_CYCLOMATIC,
            churn: WEIGHT_CHURN,
            loc: WEIGHT_LOC,
            authors: WEIGHT_AUTHORS,
        },
        thresholds: metrics::Thresholds {
            critical: THRESHOLD_CRITICAL,
            high: THRESHOLD_HIGH,
            medium: THRESHOLD_MEDIUM,
        },
        description: "Composite risk score based on complexity, churn, and team metrics."
            .to_string(),
    };

    if functions.is_empty() {
        warnings.push(AnalysisWarning {
            code: "no_functions_found".to_string(),
            message: "No functions found to analyze.".to_string(),
        });
        let quality = build_quality(
            git_status,
            ctx.cache_manager.is_some(),
            ctx.cache_loaded,
            false,
            cache_stats,
            warnings,
            skipped_files,
        );
        return Ok(Report {
            schema_version: SCHEMA_VERSION.to_string(),
            analysis: AnalysisMetadata {
                repository: ctx.repo_path_abs.to_string_lossy().to_string(),
                commit: ctx.commit_hash,
                branch: ctx.branch,
                timestamp: ctx.timestamp,
            },
            scoring_policy,
            summary: SummaryStats {
                total_functions: 0,
                project_stats: build_project_stats(&functions, &git_cache, total_unique_authors),
                max_values: None,
                distributions: None,
            },
            quality,
            functions,
        });
    }

    let scoring_context = compute_scoring_context(&functions);
    let distributions = apply_scoring_and_labels(&mut functions, &scoring_context);
    let project_stats = build_project_stats(&functions, &git_cache, total_unique_authors);

    sort_functions(&mut functions, sort_by, &mut warnings);
    if let Some(limit) = limit {
        functions.truncate(limit);
    }

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
        analysis: AnalysisMetadata {
            repository: ctx.repo_path_abs.to_string_lossy().to_string(),
            commit: ctx.commit_hash,
            branch: ctx.branch,
            timestamp: ctx.timestamp,
        },
        scoring_policy,
        summary: SummaryStats {
            total_functions: functions.len(),
            project_stats,
            max_values: Some(scoring_context.max_values),
            distributions: Some(distributions),
        },
        quality,
        functions,
    })
}

fn repository_metadata(repo: &Repository) -> (String, String, String) {
    let head = match repo.head() {
        Ok(head) => head,
        Err(err) => {
            log::warn!("Git HEAD unavailable: {}", err);
            return (
                "unknown".to_string(),
                String::new(),
                FALLBACK_TIMESTAMP.to_string(),
            );
        }
    };
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

fn commit_timestamp(commit: &git2::Commit) -> Option<String> {
    DateTime::<Utc>::from_timestamp(commit.time().seconds(), 0).map(|dt| dt.to_rfc3339())
}

fn percentile_f64(sorted_values: &[f64], value: f64, total: f64) -> f64 {
    if sorted_values.len() <= 1 {
        return 100.0;
    }

    let idx = match sorted_values.binary_search_by(|probe| {
        if probe.total_cmp(&value) == CmpOrdering::Less {
            CmpOrdering::Less
        } else {
            CmpOrdering::Greater
        }
    }) {
        Ok(idx) | Err(idx) => idx,
    };
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
    match sort_by {
        "file" | "location" => sort_by_location(functions),
        "churn_score" | "churn" => functions.sort_by(|a, b| {
            b.churn_score
                .total_cmp(&a.churn_score)
                .then_with(|| location_order(a, b))
        }),
        "risk" | "risk_score" => functions.sort_by(|a, b| {
            risk_score(b)
                .total_cmp(&risk_score(a))
                .then_with(|| location_order(a, b))
        }),
        "cognitive" | "cognitive_complexity" => functions.sort_by(|a, b| {
            b.cognitive_complexity
                .cmp(&a.cognitive_complexity)
                .then_with(|| location_order(a, b))
        }),
        "cyclomatic" | "cyclomatic_complexity" => functions.sort_by(|a, b| {
            b.cyclomatic_complexity
                .cmp(&a.cyclomatic_complexity)
                .then_with(|| location_order(a, b))
        }),
        "loc" | "lines_of_code" => functions.sort_by(|a, b| {
            b.lines_of_code
                .cmp(&a.lines_of_code)
                .then_with(|| location_order(a, b))
        }),
        other => {
            warnings.push(AnalysisWarning {
                code: "unsupported_sort".to_string(),
                message: format!("Unsupported sort field '{other}'. Falling back to file order."),
            });
            sort_by_location(functions);
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
    let mut author_contributions: HashMap<String, usize> = HashMap::new();
    for entry in git_cache.values() {
        if entry.line_changes.is_empty() {
            for author in &entry.authors {
                *author_contributions.entry(author.clone()).or_default() += entry.times_modified;
            }
        } else {
            for change in &entry.line_changes {
                *author_contributions
                    .entry(change.author.clone())
                    .or_default() += 1;
            }
        }
    }

    let total_contributions = author_contributions.values().sum::<usize>();
    let bus_factor = if total_contributions == 0 {
        0
    } else {
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
    };

    let total_loc = functions
        .iter()
        .map(|function| function.lines_of_code)
        .sum::<u32>();
    let total_complexity = functions
        .iter()
        .map(|function| function.cognitive_complexity + function.cyclomatic_complexity)
        .sum::<u32>();
    let tech_debt_density = if total_loc == 0 {
        0.0
    } else {
        total_complexity as f64 / total_loc as f64
    };

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

    metrics::ProjectStats {
        total_unique_authors,
        bus_factor,
        tech_debt_density,
        top_hotspots: hotspots,
    }
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
    let repo = match Repository::open(&repo_path_abs) {
        Ok(repo) => Some(repo),
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "git_unavailable".to_string(),
                message: format!("Git repository unavailable: {err}"),
            });
            None
        }
    };

    let (branch, commit_hash, timestamp) = match repo.as_ref() {
        Some(repo) => repository_metadata(repo),
        None => (
            "unknown".to_string(),
            String::new(),
            FALLBACK_TIMESTAMP.to_string(),
        ),
    };

    let cache_manager = match CacheManager::new(&repo_path_abs) {
        Ok(cm) => Some(cm),
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "cache_unavailable".to_string(),
                message: format!("Failed to initialize cache: {err}"),
            });
            None
        }
    };

    let cache = if let Some(cm) = &cache_manager {
        match cm.load() {
            Ok(cache) => cache,
            Err(err) => {
                warnings.push(AnalysisWarning {
                    code: "cache_load_failed".to_string(),
                    message: format!("Failed to load cache: {err}"),
                });
                AnalysisCache::default()
            }
        }
    } else {
        AnalysisCache::default()
    };

    let cache_loaded = cache_manager.is_some()
        && (!cache.files.is_empty()
            || !cache.git_cache.is_empty()
            || cache.last_commit_oid.is_some());
    let config = load_config(&repo_path_abs, warnings);

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
    })
}

fn load_config(repo_path: &Path, warnings: &mut Vec<AnalysisWarning>) -> ChurnLensConfig {
    let config_path = repo_path.join("churnlens.toml");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return ChurnLensConfig::default();
        }
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "config_read_failed".to_string(),
                message: format!("Failed to read {}: {err}", config_path.display()),
            });
            return ChurnLensConfig::default();
        }
    };

    match toml::from_str(&content) {
        Ok(config) => config,
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "config_parse_failed".to_string(),
                message: format!("Failed to parse {}: {err}", config_path.display()),
            });
            ChurnLensConfig::default()
        }
    }
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

    let git_cache = match repo {
        Some(repo) => match GitAnalyzer::get_all_file_metrics(
            repo,
            std::mem::take(&mut cache.git_cache),
            cache.git_metadata.clone(),
            cache.last_commit_oid.clone(),
            repo_path_abs,
            branch,
            commit_hash,
            bug_fix_patterns,
        ) {
            Ok(git_result) => {
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
            Err(err) => {
                git_status.partial = true;
                warnings.push(AnalysisWarning {
                    code: "git_metrics_failed".to_string(),
                    message: format!("Failed to collect Git metrics: {err}"),
                });
                HashMap::new()
            }
        },
        None => HashMap::new(),
    };

    (git_cache, git_status)
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

    walker
        .par_bridge()
        .filter_map(|entry| {
            if shutdown.load(Ordering::Relaxed) {
                return None;
            }

            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    return Some(WorkerOutput {
                        warnings: vec![AnalysisWarning {
                            code: "walk_entry_failed".to_string(),
                            message: format!("Failed to read directory entry: {err}"),
                        }],
                        ..WorkerOutput::default()
                    });
                }
            };

            let path = entry.path();
            if !path.is_file() {
                return None;
            }

            let rel_path = match path.strip_prefix(repo_path_abs) {
                Ok(p) => p,
                Err(err) => {
                    return Some(WorkerOutput {
                        warnings: vec![AnalysisWarning {
                            code: "path_error".to_string(),
                            message: format!(
                                "Failed to strip prefix for {}: {}",
                                path.display(),
                                err
                            ),
                        }],
                        ..WorkerOutput::default()
                    });
                }
            };
            let rel_path_str = rel_path.to_string_lossy().to_string();

            let registry = Arc::clone(&registry);
            let file_read_limiter = Arc::clone(&file_read_limiter);
            if registry.get_support(&rel_path_str).is_none() {
                return None;
            }

            let mut output = WorkerOutput {
                active_path: Some(rel_path_str.clone()),
                ..WorkerOutput::default()
            };

            let source = match FileContent::read(path, &file_read_limiter) {
                Ok(source) => source,
                Err(err) => {
                    output.skipped_files.push(SkippedFile {
                        path: rel_path_str,
                        reason: format!("failed to read file: {err}"),
                    });
                    return Some(output);
                }
            };
            let source_bytes = source.as_bytes();
            let content_hash = stable_content_hash(source_bytes);

            let mut functions = if let Some(cached_entry) = cache.files.get(&rel_path_str) {
                if cached_entry.content_hash == content_hash {
                    output.cache_hit = 1;
                    cached_entry.functions.clone()
                } else {
                    output.cache_miss = 1;
                    match AnalysisWorker::process_file_from_source(
                        source_bytes,
                        &rel_path_str,
                        &registry,
                    ) {
                        Ok(functions) => functions,
                        Err(err) => {
                            output.skipped_files.push(SkippedFile {
                                path: rel_path_str,
                                reason: format!("failed to analyze file: {err}"),
                            });
                            return Some(output);
                        }
                    }
                }
            } else {
                output.cache_miss = 1;
                match AnalysisWorker::process_file_from_source(
                    source_bytes,
                    &rel_path_str,
                    &registry,
                ) {
                    Ok(functions) => functions,
                    Err(err) => {
                        output.skipped_files.push(SkippedFile {
                            path: rel_path_str,
                            reason: format!("failed to analyze file: {err}"),
                        });
                        return Some(output);
                    }
                }
            };

            if let Some(entry) = git_cache.get(&rel_path_str) {
                for func in &mut functions {
                    let churn = GitAnalyzer::compute_churn_metrics_for_range(
                        entry,
                        func.line,
                        func.end_line,
                    );
                    func.times_modified = churn.times_modified;
                    func.bug_fix_commits = churn.bug_fix_commits;
                    func.authors_count = churn.authors_count;
                    func.authors = if include_authors {
                        Some(churn.authors.clone())
                    } else {
                        None
                    };
                    func.churn_score = churn.churn_score;
                    func.file = rel_path_str.clone();
                }
            }

            output.cache_entry = Some((
                rel_path_str,
                FileCacheEntry {
                    content_hash,
                    functions: functions.clone(),
                },
            ));
            output.functions = functions;

            Some(output)
        })
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

    cog_vals.sort();
    churn_vals.sort_by(|a, b| a.total_cmp(b));
    loc_vals.sort();
    cyc_vals.sort();
    auth_vals.sort();

    let p95_idx = (functions.len() * 95 / 100).min(functions.len() - 1);
    let p99_idx = (functions.len() * 99 / 100).min(functions.len() - 1);

    let cognitive_p95 = cog_vals[p95_idx] as f64;
    let churn_p95 = churn_vals[p95_idx];
    let loc_p95 = loc_vals[p95_idx] as f64;
    let cyc_p95 = cyc_vals[p95_idx] as f64;
    let auth_p95 = auth_vals[p95_idx] as f64;

    let cognitive_p99 = cog_vals[p99_idx] as f64;
    let churn_p99 = churn_vals[p99_idx];
    let loc_p99 = loc_vals[p99_idx] as f64;
    let cyc_p99 = cyc_vals[p99_idx] as f64;
    let auth_p99 = auth_vals[p99_idx] as f64;

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

    ScoringContext {
        max_values,
        caps: NormalizationCaps {
            cognitive: cap_cog,
            churn: cap_churn,
            loc: cap_loc,
            cyclomatic: cap_cyc,
            authors: cap_auth,
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
        let norm_cog = normalized_value(func.cognitive_complexity as f64, context.caps.cognitive);
        let norm_cyc = normalized_value(func.cyclomatic_complexity as f64, context.caps.cyclomatic);
        let norm_churn = normalized_value(func.churn_score, context.caps.churn);
        let norm_loc = normalized_value(func.lines_of_code as f64, context.caps.loc);
        let norm_auth = normalized_value(func.authors_count as f64, context.caps.authors);

        func.normalized = Some(NormalizedMetrics {
            cyclomatic: norm_cyc,
            churn: norm_churn,
            cognitive: norm_cog,
            loc: norm_loc,
            authors: norm_auth,
        });

        let base_score = (WEIGHT_COGNITIVE * norm_cog)
            + (WEIGHT_CYCLOMATIC * norm_cyc)
            + (WEIGHT_CHURN * norm_churn)
            + (WEIGHT_LOC * norm_loc)
            + (WEIGHT_AUTHORS * norm_auth);
        let nesting_penalty = 1.0 + (func.nesting_depth as f64 / 4.0).powi(2) * 0.20;
        let final_score = base_score * nesting_penalty;

        func.risk = Some(RiskMetrics {
            base_score,
            nesting_penalty,
            final_score,
            level: String::new(),
            primary_driver: String::new(),
        });
    }

    let mut risk_vals: Vec<f64> = functions
        .iter()
        .filter_map(|f| f.risk.as_ref().map(|risk| risk.final_score))
        .collect();
    risk_vals.sort_by(|a, b| a.total_cmp(b));

    let total_funcs = functions.len() as f64;
    for func in functions.iter_mut() {
        let Some(risk_score) = func.risk.as_ref().map(|risk| risk.final_score) else {
            continue;
        };
        let churn = func.churn_score;
        let cog = func.cognitive_complexity as f64;

        let risk_pct = percentile_f64(&risk_vals, risk_score, total_funcs);

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
                ("cognitive", norm.cognitive),
                ("churn", norm.churn),
                ("cyclomatic", norm.cyclomatic),
                ("loc", norm.loc),
                ("authors", norm.authors),
            ];
            drivers.sort_by(|a, b| b.1.total_cmp(&a.1));
            if let Some(risk) = func.risk.as_mut() {
                risk.level = level;
                risk.primary_driver = drivers[0].0.to_string();
            }
        }
    }

    Distributions {
        risk_p95: risk_vals[context.p95_idx],
        churn_p95: context.churn_vals[context.p95_idx],
        cognitive_p95: context.cog_vals[context.p95_idx] as f64,
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
