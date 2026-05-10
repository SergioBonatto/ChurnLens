pub mod ast;
pub mod cache;
pub mod error;
pub mod git;
pub mod metrics;

use anyhow::Result;
use ast::parser::TypeScriptAnalyzer;
use cache::{AnalysisCache, CacheManager, FileCacheEntry};
use chrono::{DateTime, Utc};
use git::GitAnalyzer;
use git2::Repository;
use ignore::WalkBuilder;
use metrics::{
    AnalysisMetadata, AnalysisQuality, AnalysisStatus, AnalysisWarning, CacheAnalysisStatus,
    Distributions, FunctionMetrics, GitAnalysisStatus, MaxValues, NormalizedMetrics,
    PercentileMetrics, Report, RiskMetrics, SkippedFile, SummaryStats,
};
use rayon::iter::{ParallelBridge, ParallelIterator};
use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const SCHEMA_VERSION: &str = "0.2.0";
const FALLBACK_TIMESTAMP: &str = "1970-01-01T00:00:00+00:00";

pub fn analyze_repository(
    repo_path: &Path,
    sort_by: &str,
    limit: Option<usize>,
    shutdown: Arc<AtomicBool>,
) -> Result<Report> {
    log::info!("Analyzing repository: {}", repo_path.display());

    // Canonicalize the repository root once so Git, cache, and filesystem reads share the same path.
    let repo_path_abs = repo_path.canonicalize()?;
    let mut warnings = Vec::new();
    let mut skipped_files = Vec::new();

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
    let (branch_name, commit_hash, timestamp) = match repo.as_ref() {
        Some(repo) => repository_metadata(repo),
        None => (
            "unknown".to_string(),
            String::new(),
            FALLBACK_TIMESTAMP.to_string(),
        ),
    };

    let cache_manager = match CacheManager::new(&repo_path_abs) {
        Ok(cache_manager) => Some(cache_manager),
        Err(err) => {
            warnings.push(AnalysisWarning {
                code: "cache_unavailable".to_string(),
                message: format!("Failed to initialize cache: {err}"),
            });
            None
        }
    };
    // Load the local cache if it exists. Any failure is recorded as partial analysis metadata.
    let mut cache = if let Some(cache_manager) = &cache_manager {
        match cache_manager.load() {
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

    // Collect Git churn first so file-level metrics can be enriched before normalization and scoring.
    let mut git_status = GitAnalysisStatus {
        available: repo.is_some(),
        partial: false,
        cache_reset: false,
        processed_commits: 0,
    };

    let mut git_cache = match repo.as_ref() {
        Some(repo) => match GitAnalyzer::get_all_file_metrics(
            repo,
            std::mem::take(&mut cache.git_cache),
            cache.git_metadata.clone(),
            cache.last_commit_oid.clone(),
            &repo_path_abs,
            &branch_name,
            &commit_hash,
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

    let walker = WalkBuilder::new(&repo_path_abs)
        .standard_filters(true)
        .hidden(false)
        .build();

    // Walk the working tree, reuse AST cache entries when the content hash matches, and collect per-file results.
    let worker_result = walker
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

            let ext = path.extension()?;
            if ext != "ts" && ext != "tsx" && ext != "js" && ext != "jsx" {
                return None;
            }

            let rel_path = path.strip_prefix(&repo_path_abs).ok()?;
            let rel_path_str = rel_path.to_string_lossy().to_string();
            let mut output = WorkerOutput {
                active_path: Some(rel_path_str.clone()),
                ..WorkerOutput::default()
            };

            let source = match std::fs::read(path) {
                Ok(source) => source,
                Err(err) => {
                    output.skipped_files.push(SkippedFile {
                        path: rel_path_str,
                        reason: format!("failed to read file: {err}"),
                    });
                    return Some(output);
                }
            };
            let content_hash = stable_content_hash(&source);

            let mut functions = if let Some(cached_entry) = cache.files.get(&rel_path_str) {
                if cached_entry.content_hash == content_hash {
                    output.cache_hit = 1;
                    cached_entry.functions.clone()
                } else {
                    output.cache_miss = 1;
                    match AnalysisWorker::process_file_from_source(&source, &rel_path_str) {
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
                match AnalysisWorker::process_file_from_source(&source, &rel_path_str) {
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
                let churn = GitAnalyzer::compute_churn_metrics(entry);
                for func in &mut functions {
                    func.times_modified = churn.times_modified;
                    func.bug_fix_commits = churn.bug_fix_commits;
                    func.authors_count = churn.authors_count;
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
        .into_parts();

    let mut functions = worker_result.functions;
    warnings.extend(worker_result.warnings);
    skipped_files.extend(worker_result.skipped_files);
    git_cache.retain(|path, _| worker_result.active_paths.contains(path));

    if functions.is_empty() {
        warnings.push(AnalysisWarning {
            code: "no_functions_found".to_string(),
            message: "No functions found to analyze.".to_string(),
        });
        let quality = build_quality(
            git_status,
            cache_manager.is_some(),
            cache_loaded,
            false,
            worker_result.cache_stats,
            warnings,
            skipped_files,
        );
        return Ok(Report {
            schema_version: SCHEMA_VERSION.to_string(),
            analysis: AnalysisMetadata {
                repository: repo_path_abs.to_string_lossy().to_string(),
                commit: commit_hash,
                branch: branch_name,
                timestamp,
            },
            summary: SummaryStats {
                total_functions: 0,
                max_values: None,
                distributions: None,
            },
            quality,
            functions,
        });
    }

    // Compute repository-wide caps before deriving normalized metrics and risk scores.
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

    // Normalize metrics, then derive the composite risk score for each function.
    for func in &mut functions {
        let norm_cog = normalized_value(func.cognitive_complexity as f64, cap_cog);
        let norm_cyc = normalized_value(func.cyclomatic_complexity as f64, cap_cyc);
        let norm_churn = normalized_value(func.churn_score, cap_churn);
        let norm_loc = normalized_value(func.lines_of_code as f64, cap_loc);
        let norm_auth = normalized_value(func.authors_count as f64, cap_auth);

        func.normalized = Some(NormalizedMetrics {
            cyclomatic: norm_cyc,
            churn: norm_churn,
            cognitive: norm_cog,
            loc: norm_loc,
            authors: norm_auth,
        });

        let base_score = (0.35 * norm_cog)
            + (0.15 * norm_cyc)
            + (0.30 * norm_churn)
            + (0.10 * norm_loc)
            + (0.10 * norm_auth);
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
    // Assign percentile ranks and human-readable risk labels.
    for func in &mut functions {
        let Some(risk_score) = func.risk.as_ref().map(|risk| risk.final_score) else {
            continue;
        };
        let churn = func.churn_score;
        let cog = func.cognitive_complexity as f64;

        let risk_pct = percentile_f64(&risk_vals, risk_score, total_funcs);

        func.percentile = Some(PercentileMetrics {
            risk: risk_pct,
            churn: percentile_f64(&churn_vals, churn, total_funcs),
            cognitive: percentile_u32(&cog_vals, cog as u32, total_funcs),
        });

        let level = match risk_pct {
            p if p >= 95.0 => "critical",
            p if p >= 80.0 => "high",
            p if p >= 50.0 => "medium",
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

    let risk_p95 = risk_vals[p95_idx];

    // Apply user-facing ordering and optional truncation before emitting the report.
    sort_functions(&mut functions, sort_by, &mut warnings);
    if let Some(limit) = limit {
        functions.truncate(limit);
    }

    let mut cache_saved = false;
    cache.files = worker_result.cache_entries;
    cache.git_cache = git_cache;
    cache.last_commit_oid = if commit_hash.is_empty() {
        None
    } else {
        Some(commit_hash.clone())
    };
    if let Some(cache_manager) = &cache_manager {
        match cache_manager.save(cache) {
            Ok(()) => cache_saved = true,
            Err(err) => warnings.push(AnalysisWarning {
                code: "cache_save_failed".to_string(),
                message: format!("Failed to save cache: {err}"),
            }),
        }
    }

    let quality = build_quality(
        git_status,
        cache_manager.is_some(),
        cache_loaded,
        cache_saved,
        worker_result.cache_stats,
        warnings,
        skipped_files,
    );

    let report = Report {
        schema_version: SCHEMA_VERSION.to_string(),
        analysis: AnalysisMetadata {
            repository: repo_path_abs.to_string_lossy().to_string(),
            commit: commit_hash.clone(),
            branch: branch_name,
            timestamp,
        },
        summary: SummaryStats {
            total_functions: functions.len(),
            max_values: Some(max_values),
            distributions: Some(Distributions {
                risk_p95,
                churn_p95,
                cognitive_p95,
            }),
        },
        quality,
        functions,
    };

    Ok(report)
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
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
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
    fn process_file_from_source(source: &[u8], rel_path_str: &str) -> Result<Vec<FunctionMetrics>> {
        let source = std::str::from_utf8(source)?;
        TypeScriptAnalyzer::analyze_source(source, rel_path_str)
    }
}
