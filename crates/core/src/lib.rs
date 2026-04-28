pub mod ast;
pub mod cache;
pub mod error;
pub mod git;
pub mod metrics;

use anyhow::Result;
use ast::parser::TypeScriptAnalyzer;
use cache::{AnalysisCache, CacheManager};
use git::GitAnalyzer;
use git2::Repository;
use ignore::WalkBuilder;
use metrics::FunctionMetrics;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::borrow::Cow;
use std::io::{self, Write};

pub fn analyze_repository(
    repo_path: &Path,
    _sort_by: &str,
    _limit: Option<usize>,
) -> Result<()> {
    log::info!("Analyzing repository: {}", repo_path.display());

    let repo = Repository::open(repo_path)?;
    let git_metrics = GitAnalyzer::get_all_file_metrics(&repo)?;

    let cache_manager = CacheManager::new(repo_path);
    let mut cache = cache_manager.load();
    let new_cache_files = Arc::new(Mutex::new(HashMap::new()));

    let repo_path_abs = repo_path.canonicalize()?;
    
    let walker = WalkBuilder::new(&repo_path_abs)
        .standard_filters(true)
        .hidden(false)
        .build_parallel();

    let stdout = io::stdout();
    let stdout_mutex = Arc::new(Mutex::new(stdout));

    walker.run(|| {
        let repo_path_abs = repo_path_abs.clone();
        let git_metrics = &git_metrics;
        let cache = &cache;
        let new_cache_files = Arc::clone(&new_cache_files);
        let stdout_mutex = Arc::clone(&stdout_mutex);
        let repo = Repository::open(&repo_path_abs).expect("Open repo in thread");

        Box::new(move |entry| {
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
                        
                        // Check cache
                        let current_oid = GitAnalyzer::get_file_oid(&repo, rel_path)
                            .unwrap_or(None)
                            .map(|o| o.to_string())
                            .unwrap_or_default();

                        let mut functions = if let Some((cached_oid, cached_funcs)) = cache.files.get(&rel_path_str) {
                            if cached_oid == &current_oid && !current_oid.is_empty() {
                                cached_funcs.clone()
                            } else {
                                match TypeScriptAnalyzer::parse_file(path.to_str().unwrap()) {
                                    Ok(f) => f,
                                    Err(_) => return ignore::WalkState::Continue,
                                }
                            }
                        } else {
                            match TypeScriptAnalyzer::parse_file(path.to_str().unwrap()) {
                                Ok(f) => f,
                                Err(_) => return ignore::WalkState::Continue,
                            }
                        };

                        // Update with fresh churn metrics
                        if let Some(churn) = git_metrics.get(&rel_path_str) {
                            for func in &mut functions {
                                func.times_modified = churn.times_modified;
                                func.bug_fix_commits = churn.bug_fix_commits;
                                func.authors_count = churn.authors_count;
                                func.churn_score = churn.churn_score;
                                func.file = Cow::Owned(rel_path_str.clone());
                            }
                        } else {
                            for func in &mut functions {
                                func.file = Cow::Owned(rel_path_str.clone());
                            }
                        }

                        // Stream output (NDJSON)
                        {
                            let mut out = stdout_mutex.lock().unwrap();
                            for func in &functions {
                                if let Ok(json) = serde_json::to_string(func) {
                                    let _ = writeln!(out, "{}", json);
                                }
                            }
                            let _ = out.flush();
                        }

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

    // Finalize cache
    let new_files = Arc::try_unwrap(new_cache_files).unwrap().into_inner().unwrap();
    cache.files = new_files;
    let _ = cache_manager.save(&cache);

    Ok(())
}
