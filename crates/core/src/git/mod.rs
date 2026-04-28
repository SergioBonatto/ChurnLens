use crate::metrics::ChurnMetrics;
use crate::cache::GitCacheEntry;
use anyhow::Result;
use git2::{Commit, Repository, Oid, DiffOptions};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use aho_corasick::AhoCorasick;
use std::cell::RefCell;

pub struct GitAnalyzer;

thread_local! {
    static REPO_HANDLE: RefCell<Option<Repository>> = RefCell::new(None);
}

impl GitAnalyzer {
    pub fn get_all_file_metrics(
        repo: &Repository,
        mut git_cache: HashMap<String, GitCacheEntry>,
        last_commit_oid: Option<String>,
    ) -> Result<HashMap<String, GitCacheEntry>> {
        let mut revwalk = repo.revwalk()?;
        revwalk.set_sorting(git2::Sort::TOPOLOGICAL)?;
        revwalk.push_head()?;

        if let Some(oid_str) = last_commit_oid {
            if let Ok(oid) = Oid::from_str(&oid_str) {
                let _ = revwalk.hide(oid);
            }
        }

        let patterns = &["fix", "bug", "issue", "close", "resolve"];
        let ac = AhoCorasick::new(patterns).expect("Valid patterns");

        for oid in revwalk {
            let oid = oid?;
            let commit = repo.find_commit(oid)?;
            let author = commit.author().name().unwrap_or("Unknown").to_string();
            let is_bug_fix = Self::is_bug_fix(&commit, &ac);

            // ANALISE DE MERGE: Para métricas de churn, comparamos com o primeiro pai (mainline)
            // para evitar dupla contagem de modificações vindas de branches laterais.
            let modified_files = Self::get_modified_files_optimized(repo, &commit)?;

            for file_path in modified_files {
                let entry = git_cache.entry(file_path).or_default();
                entry.times_modified += 1;
                if is_bug_fix {
                    entry.bug_fix_commits += 1;
                }
                entry.authors.insert(author.clone());
            }
        }

        Ok(git_cache)
    }

    pub fn compute_churn_metrics(cache_entry: &GitCacheEntry) -> ChurnMetrics {
        let authors_count = cache_entry.authors.len().max(1);
        let churn_score = (cache_entry.times_modified as f64 + (cache_entry.bug_fix_commits as f64 * 2.0)) 
            * (authors_count as f64 + 1.0).log10();

        ChurnMetrics {
            times_modified: cache_entry.times_modified,
            bug_fix_commits: cache_entry.bug_fix_commits,
            authors_count,
            churn_score,
        }
    }

    fn get_modified_files_optimized(repo: &Repository, commit: &Commit) -> Result<HashSet<String>> {
        let mut files = HashSet::new();
        let current_tree = commit.tree()?;

        if commit.parent_count() > 0 {
            // Comparamos com o primeiro pai para rastrear a evolução linear
            let parent = commit.parent(0)?;
            let parent_tree = parent.tree()?;
            
            let mut opts = DiffOptions::new();
            opts.pathspec("*");
            opts.context_lines(0);

            let diff = repo.diff_tree_to_tree(Some(&parent_tree), Some(&current_tree), Some(&mut opts))?;
            
            for delta in diff.deltas() {
                if let Some(path) = delta.new_file().path() {
                    if let Some(path_str) = path.to_str() {
                        files.insert(path_str.to_string());
                    }
                }
            }
        } else {
            // Commit inicial: todos os arquivos são novos
            current_tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
                if let Some(name) = entry.name() {
                    let path = Path::new(root).join(name);
                    if let Some(path_str) = path.to_str() {
                        files.insert(path_str.to_string());
                    }
                }
                git2::TreeWalkResult::Ok
            })?;
        }

        Ok(files)
    }

    pub fn get_file_oid_tls(repo_path: &Path, rel_path: &Path) -> Result<Option<Oid>> {
        REPO_HANDLE.with(|handle| {
            let mut opt = handle.borrow_mut();
            if opt.is_none() {
                *opt = Some(Repository::open(repo_path)?);
            }
            let repo = opt.as_ref().unwrap();
            
            // Otimização: peel_to_commit uma única vez por HEAD
            let head = repo.head()?.peel_to_commit()?;
            let tree = head.tree()?;
            
            match tree.get_path(rel_path) {
                Ok(entry) => Ok(Some(entry.id())),
                Err(_) => Ok(None),
            }
        })
    }

    fn is_bug_fix(commit: &Commit, ac: &AhoCorasick) -> bool {
        if let Some(message) = commit.message() {
            let lower_msg = message.to_lowercase();
            ac.find(&lower_msg).is_some()
        } else {
            false
        }
    }
}
