use crate::cache::{GitCacheEntry, GitCacheMetadata, GIT_ALGORITHM_VERSION};
use crate::metrics::ChurnMetrics;
use anyhow::{anyhow, Result};
use git2::{Commit, Delta, DiffFindOptions, DiffOptions, Oid, Repository};
use rayon::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct GitAnalyzer;

const COMMITS_PER_WORKER_CHUNK: usize = 32;
const COMMITS_PER_BATCH: usize = 4096;

thread_local! {
     static REPO_HANDLE: RefCell<Option<ThreadLocalRepository>> = const { RefCell::new(None) };
}

struct ThreadLocalRepository {
    path: PathBuf,
    repo: Repository,
}

#[derive(Default)]
pub struct GitMetricsResult {
    pub cache: HashMap<String, GitCacheEntry>,
    pub metadata: GitCacheMetadata,
    pub cache_reset: bool,
    pub processed_commits: usize,
    pub partial: bool,
    pub warnings: Vec<String>,
}

#[derive(Default)]
struct GitBatchMetrics {
    files: HashMap<String, GitCacheEntry>,
    renames: Vec<(String, String)>,
    processed_commits: usize,
    partial: bool,
    warnings: Vec<String>,
}

struct ModifiedFiles {
    paths: HashSet<String>,
    renames: Vec<(String, String)>,
}

impl GitAnalyzer {
    pub fn get_all_file_metrics(
        repo: &Repository,
        mut git_cache: HashMap<String, GitCacheEntry>,
        git_metadata: Option<GitCacheMetadata>,
        last_commit_oid: Option<String>,
        repository_path: &Path,
        branch: &str,
        head_oid: &str,
    ) -> Result<GitMetricsResult> {
        let mut result = GitMetricsResult {
            cache: HashMap::new(),
            metadata: GitCacheMetadata {
                repository_path: repository_path.to_string_lossy().to_string(),
                branch: branch.to_string(),
                head_oid: head_oid.to_string(),
                algorithm_version: GIT_ALGORITHM_VERSION,
            },
            cache_reset: false,
            processed_commits: 0,
            partial: false,
            warnings: Vec::new(),
        };

        let mut last_commit_oid = last_commit_oid;
        if !Self::is_git_cache_compatible(git_metadata.as_ref(), repository_path, branch) {
            git_cache.clear();
            last_commit_oid = None;
            result.cache_reset = true;
        }
        if last_commit_oid.is_none() && !git_cache.is_empty() {
            git_cache.clear();
            result.cache_reset = true;
        }

        let mut revwalk = repo.revwalk()?;
        revwalk.set_sorting(git2::Sort::TOPOLOGICAL)?;
        revwalk.push_head()?;

        if let Some(oid_str) = last_commit_oid.as_deref() {
            let head = Oid::from_str(head_oid)?;
            match Oid::from_str(oid_str) {
                Ok(oid) if oid == head => {
                    result.cache = git_cache;
                    return Ok(result);
                }
                Ok(oid) if repo.graph_descendant_of(head, oid).unwrap_or(false) => {
                    if let Err(err) = revwalk.hide(oid) {
                        git_cache.clear();
                        result.cache_reset = true;
                        result.warnings.push(format!(
                            "Failed to hide cached Git commit {oid}: {err}. Git cache was rebuilt."
                        ));
                    }
                }
                Ok(oid) => {
                    git_cache.clear();
                    result.cache_reset = true;
                    result.warnings.push(format!(
                        "Cached Git commit {oid} is not an ancestor of HEAD. Git cache was rebuilt."
                    ));
                }
                Err(err) => {
                    git_cache.clear();
                    result.cache_reset = true;
                    result.warnings.push(format!(
                        "Cached Git commit '{oid_str}' is invalid: {err}. Git cache was rebuilt."
                    ));
                }
            }
        }

        let repo_path = repo.path().to_path_buf();
        let mut pending = Vec::with_capacity(COMMITS_PER_BATCH);
        let mut new_metrics = GitBatchMetrics::default();

        for oid in revwalk {
            pending.push(oid?);
            if pending.len() >= COMMITS_PER_BATCH {
                let batch_metrics = Self::process_commit_batch(&repo_path, &pending);
                Self::merge_batch_metrics(&mut new_metrics, batch_metrics);
                pending.clear();
            }
        }

        if !pending.is_empty() {
            let batch_metrics = Self::process_commit_batch(&repo_path, &pending);
            Self::merge_batch_metrics(&mut new_metrics, batch_metrics);
        }

        result.processed_commits = new_metrics.processed_commits;
        result.partial = new_metrics.partial;
        result.warnings.extend(new_metrics.warnings);

        Self::merge_git_cache(&mut git_cache, new_metrics.files);
        Self::apply_rename_aliases(&mut git_cache, &new_metrics.renames);
        result.cache = git_cache;

        Ok(result)
    }

    pub fn compute_churn_metrics(cache_entry: &GitCacheEntry) -> ChurnMetrics {
        let authors_count = cache_entry.authors.len().max(1);
        let churn_score = (cache_entry.times_modified as f64
            + (cache_entry.bug_fix_commits as f64 * 2.0))
            * (authors_count as f64 + 1.0).log10();

        ChurnMetrics {
            times_modified: cache_entry.times_modified,
            bug_fix_commits: cache_entry.bug_fix_commits,
            authors_count,
            churn_score,
        }
    }

    fn is_git_cache_compatible(
        metadata: Option<&GitCacheMetadata>,
        repository_path: &Path,
        branch: &str,
    ) -> bool {
        let Some(metadata) = metadata else {
            return false;
        };

        metadata.algorithm_version == GIT_ALGORITHM_VERSION
            && metadata.repository_path == repository_path.to_string_lossy()
            && metadata.branch == branch
            && !metadata.head_oid.is_empty()
    }

    fn get_modified_files_optimized(repo: &Repository, commit: &Commit) -> Result<ModifiedFiles> {
        let mut files = HashSet::new();
        let mut renames = Vec::new();
        let current_tree = commit.tree()?;

        if commit.parent_count() > 0 {
            for parent_index in 0..commit.parent_count() {
                let parent = commit.parent(parent_index)?;
                let parent_tree = parent.tree()?;
                Self::collect_diff_paths(
                    repo,
                    Some(&parent_tree),
                    Some(&current_tree),
                    &mut files,
                    &mut renames,
                )?;
            }
        } else {
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

        Ok(ModifiedFiles {
            paths: files,
            renames,
        })
    }

    fn collect_diff_paths(
        repo: &Repository,
        old_tree: Option<&git2::Tree>,
        new_tree: Option<&git2::Tree>,
        files: &mut HashSet<String>,
        renames: &mut Vec<(String, String)>,
    ) -> Result<()> {
        let mut opts = DiffOptions::new();
        opts.pathspec("*");
        opts.context_lines(0);

        let mut diff = repo.diff_tree_to_tree(old_tree, new_tree, Some(&mut opts))?;
        let mut find_opts = DiffFindOptions::new();
        find_opts.renames(true);
        diff.find_similar(Some(&mut find_opts))?;

        for delta in diff.deltas() {
            let new_path = delta
                .new_file()
                .path()
                .and_then(|path| path.to_str())
                .map(str::to_string);

            if delta.status() == Delta::Renamed {
                if let (Some(old_path), Some(new_path)) = (
                    delta
                        .old_file()
                        .path()
                        .and_then(|path| path.to_str())
                        .map(str::to_string),
                    new_path.clone(),
                ) {
                    renames.push((old_path, new_path));
                }
            }

            if let Some(path) = new_path {
                files.insert(path);
            }
        }

        Ok(())
    }

    fn process_commit_batch(repo_path: &Path, oids: &[Oid]) -> GitBatchMetrics {
        oids.par_chunks(COMMITS_PER_WORKER_CHUNK)
            .map(|chunk| Self::process_commit_chunk(repo_path, chunk))
            .reduce(GitBatchMetrics::default, |mut acc, metrics| {
                Self::merge_batch_metrics(&mut acc, metrics);
                acc
            })
    }

    fn process_commit_chunk(repo_path: &Path, oids: &[Oid]) -> GitBatchMetrics {
        match Self::with_thread_local_repo(repo_path, |repo| {
            let mut metrics = GitBatchMetrics::default();
            for oid in oids {
                match Self::process_commit(repo, *oid) {
                    Ok(commit_metrics) => {
                        metrics.processed_commits += 1;
                        Self::merge_git_cache(&mut metrics.files, commit_metrics.files);
                        metrics.renames.extend(commit_metrics.renames);
                    }
                    Err(err) => {
                        metrics.partial = true;
                        metrics
                            .warnings
                            .push(format!("Failed to process commit {oid}: {err}"));
                    }
                }
            }
            metrics
        }) {
            Ok(metrics) => metrics,
            Err(err) => GitBatchMetrics {
                partial: true,
                warnings: vec![format!(
                    "Failed to open repository {}: {err}",
                    repo_path.display()
                )],
                ..GitBatchMetrics::default()
            },
        }
    }

    fn process_commit(repo: &Repository, oid: Oid) -> Result<GitBatchMetrics> {
        let commit = repo.find_commit(oid)?;
        let author = Self::author_identity(&commit);
        let is_bug_fix = Self::is_bug_fix(&commit);
        let modified_files = Self::get_modified_files_optimized(repo, &commit)?;

        let mut metrics = HashMap::new();
        for file_path in modified_files.paths {
            let entry: &mut GitCacheEntry = metrics.entry(file_path).or_default();
            entry.times_modified += 1;
            if is_bug_fix {
                entry.bug_fix_commits += 1;
            }
            entry.authors.insert(author.clone());
        }

        Ok(GitBatchMetrics {
            files: metrics,
            renames: modified_files.renames,
            ..GitBatchMetrics::default()
        })
    }

    fn merge_batch_metrics(target: &mut GitBatchMetrics, source: GitBatchMetrics) {
        Self::merge_git_cache(&mut target.files, source.files);
        target.renames.extend(source.renames);
        target.processed_commits += source.processed_commits;
        target.partial |= source.partial;
        target.warnings.extend(source.warnings);
    }

    fn merge_git_cache(
        target: &mut HashMap<String, GitCacheEntry>,
        source: HashMap<String, GitCacheEntry>,
    ) {
        for (file_path, source_entry) in source {
            let target_entry = target.entry(file_path).or_default();
            target_entry.times_modified += source_entry.times_modified;
            target_entry.bug_fix_commits += source_entry.bug_fix_commits;
            target_entry.authors.extend(source_entry.authors);
        }
    }

    fn apply_rename_aliases(
        cache: &mut HashMap<String, GitCacheEntry>,
        renames: &[(String, String)],
    ) {
        let mut aliases = HashMap::new();
        for (old_path, new_path) in renames {
            aliases.insert(old_path.clone(), new_path.clone());
        }

        let snapshot = cache.clone();
        for (path, entry) in snapshot {
            let final_path = Self::resolve_rename_alias(&path, &aliases);
            if final_path == path {
                continue;
            }

            let target_entry = cache.entry(final_path).or_default();
            target_entry.times_modified += entry.times_modified;
            target_entry.bug_fix_commits += entry.bug_fix_commits;
            target_entry.authors.extend(entry.authors);
        }
    }

    fn resolve_rename_alias(path: &str, aliases: &HashMap<String, String>) -> String {
        let mut current = path;
        let mut seen = HashSet::new();

        while let Some(next) = aliases.get(current) {
            if !seen.insert(current) {
                break;
            }
            current = next;
        }

        current.to_string()
    }

    pub fn get_file_oid_tls(repo_path: &Path, rel_path: &Path) -> Result<Option<Oid>> {
        REPO_HANDLE.with(|handle| -> Result<Option<Oid>> {
            let mut opt = handle.borrow_mut();
            let repo_path = repo_path.to_path_buf();
            if opt.as_ref().is_none_or(|cached| cached.path != repo_path) {
                *opt = Some(ThreadLocalRepository {
                    path: repo_path.clone(),
                    repo: Repository::open(&repo_path)?,
                });
            }
            let cached = opt
                .as_ref()
                .ok_or_else(|| anyhow!("Failed to initialize repository handle"))?;
            let repo = &cached.repo;

            let head = repo.head()?.peel_to_commit()?;
            let tree = head.tree()?;

            match tree.get_path(rel_path) {
                Ok(entry) => Ok(Some(entry.id())),
                Err(_) => Ok(None),
            }
        })
    }

    fn with_thread_local_repo<T>(
        repo_path: &Path,
        callback: impl FnOnce(&Repository) -> T,
    ) -> Result<T> {
        REPO_HANDLE.with(|handle| {
            let mut opt = handle.borrow_mut();
            let repo_path = repo_path.to_path_buf();
            if opt.as_ref().is_none_or(|cached| cached.path != repo_path) {
                *opt = Some(ThreadLocalRepository {
                    path: repo_path.clone(),
                    repo: Repository::open(&repo_path)?,
                });
            }

            let cached = opt
                .as_ref()
                .ok_or_else(|| anyhow!("Failed to initialize repository handle"))?;
            Ok(callback(&cached.repo))
        })
    }

    fn author_identity(commit: &Commit) -> String {
        let author = commit.author();
        if let Some(email) = author.email() {
            return email.to_string();
        }

        author.name().unwrap_or("Unknown").to_string()
    }

    fn is_bug_fix(commit: &Commit) -> bool {
        if let Some(message) = commit.message() {
            let lower_msg = message.to_lowercase();
            lower_msg
                .split(|ch: char| !ch.is_ascii_alphanumeric())
                .any(|word| {
                    matches!(
                        word,
                        "fix"
                            | "fixes"
                            | "fixed"
                            | "bug"
                            | "bugs"
                            | "issue"
                            | "issues"
                            | "close"
                            | "closes"
                            | "closed"
                            | "resolve"
                            | "resolves"
                            | "resolved"
                    )
                })
        } else {
            false
        }
    }
}
