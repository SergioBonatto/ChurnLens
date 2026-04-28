pub mod ast;
pub mod cache;
pub mod error;
pub mod git;
pub mod metrics;

use anyhow::Result;
use ast::parser::TypeScriptAnalyzer;
use cache::CacheManager;
use git::GitAnalyzer;
use git2::Repository;
use ignore::WalkBuilder;
use metrics::{FunctionMetrics, Report, AnalysisMetadata, SummaryStats, MaxValues, Distributions, NormalizedMetrics, RiskMetrics, PercentileMetrics};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use chrono::Utc;

const SCHEMA_VERSION: &str = "0.1.0";

pub fn analyze_repository(
    repo_path: &Path,
    _sort_by: &str,
    _limit: Option<usize>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    log::info!("Analyzing repository: {}", repo_path.display());

    let repo = Repository::open(repo_path)?;
    let head = repo.head()?;
    let branch_name = head.shorthand().unwrap_or("unknown").to_string();
    let commit_hash = head.peel_to_commit()?.id().to_string();

    let cache_manager = CacheManager::new(repo_path);
    let mut cache = cache_manager.load();
    
    // Análise Incremental de Git preservando integridade de autores
    let git_cache = GitAnalyzer::get_all_file_metrics(
        &repo, 
        cache.git_cache.clone(), 
        cache.last_commit_oid.clone()
    )?;

    let new_cache_files = Arc::new(Mutex::new(HashMap::new()));
    let repo_path_abs = repo_path.canonicalize()?;
    let all_functions = Arc::new(Mutex::new(Vec::new()));

    let walker = WalkBuilder::new(&repo_path_abs)
        .standard_filters(true)
        .hidden(false)
        .build_parallel();

    rayon::scope(|_| {
        walker.run(|| {
            let repo_path_abs = repo_path_abs.clone();
            let git_cache = &git_cache;
            let cache = &cache;
            let new_cache_files = Arc::clone(&new_cache_files);
            let all_functions = Arc::clone(&all_functions);
            let shutdown = Arc::clone(&shutdown);

            Box::new(move |entry| {
                if shutdown.load(Ordering::Relaxed) {
                    return ignore::WalkState::Quit;
                }

                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => return ignore::WalkState::Continue,
                };

                let path = entry.path();
                if !path.is_file() {
                    return ignore::WalkState::Continue;
                }

                if let Some(ext) = path.extension() {
                    if ext == "ts" || ext == "tsx" || ext == "js" || ext == "jsx" {
                        if let Ok(rel_path) = path.strip_prefix(&repo_path_abs) {
                            let rel_path_str = rel_path.to_string_lossy().to_string();
                            
                            let current_oid = GitAnalyzer::get_file_oid_tls(&repo_path_abs, rel_path)
                                .unwrap_or(None)
                                .map(|o| o.to_string())
                                .unwrap_or_default();

                            let mut functions = if let Some((cached_oid, cached_funcs)) = cache.files.get(&rel_path_str) {
                                if cached_oid == &current_oid && !current_oid.is_empty() {
                                    cached_funcs.clone()
                                } else {
                                    AnalysisWorker::process_file(path, &rel_path_str)
                                }
                            } else {
                                AnalysisWorker::process_file(path, &rel_path_str)
                            };

                            // Vincula métricas de churn do cache
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

                            // Coleta funções para análise global
                            {
                                let mut all = all_functions.lock().unwrap();
                                all.extend(functions.clone());
                            }

                            // Atualiza cache de arquivos
                            if !current_oid.is_empty() {
                                let mut new_files = new_cache_files.lock().unwrap();
                                new_files.insert(rel_path_str, (current_oid, functions));
                            }
                        }
                    }
                }

                ignore::WalkState::Continue
            })
        });
    });

    // Análise Estatística Global
    let mut functions = Arc::try_unwrap(all_functions).unwrap().into_inner().unwrap();
    if functions.is_empty() {
        log::warn!("No functions found to analyze.");
        return Ok(());
    }

    // 1. Cálculo de Max e Distribuição Percentil
    let max_values = MaxValues {
        cyclomatic: functions.iter().map(|f| f.cyclomatic_complexity).max().unwrap_or(1),
        cognitive: functions.iter().map(|f| f.cognitive_complexity).max().unwrap_or(1),
        churn: functions.iter().map(|f| f.churn_score).fold(0.0, f64::max),
        loc: functions.iter().map(|f| f.lines_of_code).max().unwrap_or(1),
    };

    let mut cog_vals: Vec<u32> = functions.iter().map(|f| f.cognitive_complexity).collect();
    let mut churn_vals: Vec<f64> = functions.iter().map(|f| f.churn_score).collect();
    let mut loc_vals: Vec<u32> = functions.iter().map(|f| f.lines_of_code).collect();
    let mut cyc_vals: Vec<u32> = functions.iter().map(|f| f.cyclomatic_complexity).collect();
    let mut auth_vals: Vec<usize> = functions.iter().map(|f| f.authors_count).collect();

    cog_vals.sort();
    churn_vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
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

    // Normalização com proteção contra outliers
    let cap_cog = if (max_values.cognitive as f64) > 3.0 * cognitive_p95 { cognitive_p99 } else { max_values.cognitive as f64 }.max(1.0);
    let cap_churn = if max_values.churn > 3.0 * churn_p95 { churn_p99 } else { max_values.churn }.max(1.0);
    let cap_loc = if (max_values.loc as f64) > 3.0 * loc_p95 { loc_p99 } else { max_values.loc as f64 }.max(1.0);
    let cap_cyc = if (max_values.cyclomatic as f64) > 3.0 * cyc_p95 { cyc_p99 } else { max_values.cyclomatic as f64 }.max(1.0);
    let cap_auth = if (auth_vals.iter().max().cloned().unwrap_or(0) as f64) > 3.0 * auth_p95 { auth_p99 } else { *auth_vals.iter().max().unwrap_or(&0) as f64 }.max(1.0);

    // 2. Cálculo de scores de risco
    for func in &mut functions {
        let norm_cog = (1.0 + func.cognitive_complexity as f64).ln() / (1.0 + cap_cog).ln();
        let norm_cyc = (1.0 + func.cyclomatic_complexity as f64).ln() / (1.0 + cap_cyc).ln();
        let norm_churn = (1.0 + func.churn_score).ln() / (1.0 + cap_churn).ln();
        let norm_loc = (1.0 + func.lines_of_code as f64).ln() / (1.0 + cap_loc).ln();
        let norm_auth = (1.0 + func.authors_count as f64).ln() / (1.0 + cap_auth).ln();

        func.normalized = Some(NormalizedMetrics {
            cyclomatic: norm_cyc,
            churn: norm_churn,
            cognitive: norm_cog,
            loc: norm_loc,
            authors: norm_auth,
        });

        let base_score = (0.35 * norm_cog) + (0.15 * norm_cyc) + (0.30 * norm_churn) + (0.10 * norm_loc) + (0.10 * norm_auth);
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

    // 3. Cálculo de Rankings Percentis e Metadados de Risco
    let mut risk_vals: Vec<f64> = functions.iter().map(|f| f.risk.as_ref().unwrap().final_score).collect();
    risk_vals.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let total_funcs = functions.len() as f64;
    for func in &mut functions {
        let risk_score = func.risk.as_ref().unwrap().final_score;
        let churn = func.churn_score;
        let cog = func.cognitive_complexity as f64;

        let risk_pct = (risk_vals.iter().position(|&v| v >= risk_score).unwrap_or(0) as f64 / total_funcs) * 100.0;
        
        func.percentile = Some(PercentileMetrics {
            risk: risk_pct,
            churn: (churn_vals.iter().position(|&v| v >= churn).unwrap_or(0) as f64 / total_funcs) * 100.0,
            cognitive: (cog_vals.iter().position(|&v| v >= cog as u32).unwrap_or(0) as f64 / total_funcs) * 100.0,
        });

        let level = match risk_pct {
            p if p > 95.0 => "critical",
            p if p > 80.0 => "high",
            p if p > 50.0 => "medium",
            _ => "low",
        }.to_string();

        if let Some(norm) = &func.normalized {
            let mut drivers = vec![
                ("cognitive", norm.cognitive),
                ("churn", norm.churn),
                ("cyclomatic", norm.cyclomatic),
                ("loc", norm.loc),
                ("authors", norm.authors),
            ];
            drivers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            func.risk.as_mut().unwrap().level = level;
            func.risk.as_mut().unwrap().primary_driver = drivers[0].0.to_string();
        }
    }

    // Geração do Report Final
    let risk_p95 = risk_vals[p95_idx];
    
    let report = Report {
        schema_version: SCHEMA_VERSION.to_string(),
        analysis: AnalysisMetadata {
            repository: repo_path_abs.to_string_lossy().to_string(),
            commit: commit_hash.clone(),
            branch: branch_name,
            timestamp: Utc::now().to_rfc3339(),
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
        functions,
    };

    if let Ok(json) = serde_json::to_string_pretty(&report) {
        println!("{}", json);
    }

    // Finaliza cache com integridade Git total
    let new_files = Arc::try_unwrap(new_cache_files).unwrap().into_inner().unwrap();
    cache.files = new_files;
    cache.git_cache = git_cache;
    cache.last_commit_oid = Some(commit_hash);
    let _ = cache_manager.save(cache);

    Ok(())
}

struct AnalysisWorker;
impl AnalysisWorker {
    fn process_file(path: &Path, rel_path_str: &str) -> Vec<FunctionMetrics> {
        if let Ok(source) = std::fs::read_to_string(path) {
            if let Ok(funcs) = TypeScriptAnalyzer::analyze_source(&source, rel_path_str) {
                return funcs;
            }
        }
        Vec::new()
    }
}
