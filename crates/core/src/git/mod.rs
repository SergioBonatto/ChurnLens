use crate::metrics::ChurnMetrics;
use anyhow::Result;
use git2::{Commit, Repository, Oid};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use aho_corasick::AhoCorasick;

pub struct GitAnalyzer;

struct FileStats {
    times_modified: usize,
    bug_fix_commits: usize,
    authors: HashSet<String>,
}

impl GitAnalyzer {
    pub fn get_all_file_metrics(repo: &Repository) -> Result<HashMap<String, ChurnMetrics>> {
        let mut stats_map: HashMap<String, FileStats> = HashMap::new();
        let mut revwalk = repo.revwalk()?;
        revwalk.push_head()?;

        let patterns = &["fix", "bug", "issue", "close", "resolve"];
        let ac = AhoCorasick::new(patterns).expect("Valid patterns");

        for oid in revwalk {
            let oid = oid?;
            let commit = repo.find_commit(oid)?;
            let author = commit.author().name().unwrap_or("Unknown").to_string();
            let is_bug_fix = Self::is_bug_fix(&commit, &ac);

            let modified_files = Self::get_modified_files(repo, &commit)?;

            for file_path in modified_files {
                let stats = stats_map.entry(file_path).or_insert_with(|| FileStats {
                    times_modified: 0,
                    bug_fix_commits: 0,
                    authors: HashSet::new(),
                });

                stats.times_modified += 1;
                if is_bug_fix {
                    stats.bug_fix_commits += 1;
                }
                stats.authors.insert(author.clone());
            }
        }

        let result = stats_map
            .into_iter()
            .map(|(path, stats)| {
                let authors_count = stats.authors.len();
                let churn_score = if authors_count > 0 {
                    (stats.times_modified as f64 * stats.bug_fix_commits as f64)
                        / authors_count as f64
                } else {
                    stats.times_modified as f64
                };

                (
                    path,
                    ChurnMetrics {
                        times_modified: stats.times_modified,
                        bug_fix_commits: stats.bug_fix_commits,
                        authors_count,
                        churn_score,
                    },
                )
            })
            .collect();

        Ok(result)
    }

    pub fn get_file_oid(repo: &Repository, path: &Path) -> Result<Option<Oid>> {
        let head = repo.head()?.peel_to_commit()?;
        let tree = head.tree()?;
        
        match tree.get_path(path) {
            Ok(entry) => Ok(Some(entry.id())),
            Err(_) => Ok(None),
        }
    }

    fn is_bug_fix(commit: &Commit, ac: &AhoCorasick) -> bool {
        if let Some(message) = commit.message() {
            let lower_msg = message.to_lowercase();
            ac.find(&lower_msg).is_some()
        } else {
            false
        }
    }

    fn get_modified_files(repo: &Repository, commit: &Commit) -> Result<Vec<String>> {
        let tree = commit.tree()?;
        let mut files = Vec::new();

        if commit.parent_count() > 0 {
            for parent in commit.parents() {
                let parent_tree = parent.tree()?;
                let diff = repo.diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)?;
                
                for delta in diff.deltas() {
                    if let Some(path) = delta.new_file().path() {
                        if let Some(path_str) = path.to_str() {
                            files.push(path_str.to_string());
                        }
                    }
                }
            }
        } else {
            tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
                if let Some(name) = entry.name() {
                    let path = Path::new(root).join(name);
                    if let Some(path_str) = path.to_str() {
                        files.push(path_str.to_string());
                    }
                }
                git2::TreeWalkResult::Ok
            })?;
        }

        files.sort();
        files.dedup();
        Ok(files)
    }
}
