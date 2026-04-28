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
use metrics::FunctionMetrics;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::io::{self, Write, BufWriter};
use std::thread;

pub fn analyze_repository(
    repo_path: &Path,
    _sort_by: &str,
    _limit: Option<usize>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    log::info!("Analyzing repository: {}", repo_path.display());

    let repo = Repository::open(repo_path)?;
    let git_metrics = GitAnalyzer::get_all_file_metrics(&repo)?;

    let cache_manager = CacheManager::new(repo_path);
    let mut cache = cache_manager.load();
    let new_cache_files = Arc::new(Mutex::new(HashMap::new()));

    let repo_path_abs = repo_path.canonicalize()?;
    
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<FunctionMetrics<'static>>>();

    // Reporting Thread
    let reporter = thread::spawn(move || {
        let stdout = io::stdout();
        let mut writer = BufWriter::new(stdout.lock());
        for functions in rx {
            for func in functions {
                if let Ok(json) = serde_json::to_string(&func) {
                    let _ = writeln!(writer, "{}", json);
                }
            }
        }
        let _ = writer.flush();
    });

    let walker = WalkBuilder::new(&repo_path_abs)
        .standard_filters(true)
        .hidden(false)
        .build_parallel();

    rayon::scope(|_| {
        walker.run(|| {
            let repo_path_abs = repo_path_abs.clone();
            let git_metrics = &git_metrics;
            let cache = &cache;
            let new_cache_files = Arc::clone(&new_cache_files);
            let tx = tx.clone();
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

                            // Update with fresh churn metrics
                            if let Some(churn) = git_metrics.get(&rel_path_str) {
                                for func in &mut functions {
                                    func.times_modified = churn.times_modified;
                                    func.bug_fix_commits = churn.bug_fix_commits;
                                    func.authors_count = churn.authors_count;
                                    func.churn_score = churn.churn_score;
                                    func.file = std::borrow::Cow::Owned(rel_path_str.clone());
                                }
                            }

                            // Send to reporter
                            let _ = tx.send(functions.clone());

                            // Update new cache
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

    drop(tx);
    let _ = reporter.join();

    // Finalize cache
    let new_files = Arc::try_unwrap(new_cache_files).unwrap().into_inner().unwrap();
    cache.files = new_files;
    let _ = cache_manager.save(cache);

    Ok(())
}

struct AnalysisWorker;
impl AnalysisWorker {
    fn process_file(path: &Path, rel_path_str: &str) -> Vec<FunctionMetrics<'static>> {
        if let Ok(source) = std::fs::read_to_string(path) {
            if let Ok(funcs) = TypeScriptAnalyzer::analyze_source(&source, rel_path_str) {
                return funcs.into_iter().map(|f| FunctionMetrics {
                    name: std::borrow::Cow::Owned(f.name.into_owned()),
                    file: std::borrow::Cow::Owned(f.file.into_owned()),
                    line: f.line,
                    cyclomatic_complexity: f.cyclomatic_complexity,
                    cognitive_complexity: f.cognitive_complexity,
                    nesting_depth: f.nesting_depth,
                    lines_of_code: f.lines_of_code,
                    times_modified: f.times_modified,
                    bug_fix_commits: f.bug_fix_commits,
                    authors_count: f.authors_count,
                    churn_score: f.churn_score,
                }).collect();
            }
        }
        Vec::new()
    }
}
