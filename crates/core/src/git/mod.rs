use crate::cache::{GitCacheEntry, GitCacheMetadata, LineChange, GIT_ALGORITHM_VERSION};
use crate::metrics::ChurnMetrics;
use anyhow::{anyhow, Result};
use git2::{Commit, Delta, DiffDelta, DiffFindOptions, DiffHunk, DiffOptions, Oid, Repository};
use rayon::prelude::*;
use regex::Regex;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_128;

pub struct GitAnalyzer;

const COMMITS_PER_WORKER_CHUNK: usize = 32;
const COMMITS_PER_BATCH: usize = 4096;
const FULL_FILE_RANGE_END_LINE: u32 = 1_000_000_000;

#[derive(Clone)]
pub struct BugFixPatterns {
    patterns: Vec<Regex>,
    cache_key: String,
}

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
    files: HashMap<String, Vec<ChangedRange>>,
    renames: Vec<(String, String)>,
}

#[derive(Clone)]
struct ChangedRange {
    start_line: u32,
    end_line: u32,
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
        bug_fix_patterns: &BugFixPatterns,
    ) -> Result<GitMetricsResult> {
        let mut result = GitMetricsResult {
            cache: HashMap::new(),
            metadata: GitCacheMetadata {
                repository_path: repository_path.to_string_lossy().to_string(),
                branch: branch.to_string(),
                head_oid: head_oid.to_string(),
                algorithm_version: GIT_ALGORITHM_VERSION,
                bug_fix_patterns_hash: bug_fix_patterns.cache_key().to_string(),
            },
            cache_reset: false,
            processed_commits: 0,
            partial: false,
            warnings: Vec::new(),
        };

        let mut last_commit_oid = last_commit_oid;
        if !Self::is_git_cache_compatible(
            git_metadata.as_ref(),
            repository_path,
            branch,
            bug_fix_patterns,
        ) {
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
                let batch_metrics =
                    Self::process_commit_batch(&repo_path, &pending, bug_fix_patterns);
                Self::merge_batch_metrics(&mut new_metrics, batch_metrics);
                pending.clear();
            }
        }

        if !pending.is_empty() {
            let batch_metrics = Self::process_commit_batch(&repo_path, &pending, bug_fix_patterns);
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
            authors: cache_entry.authors.iter().cloned().collect(),
            churn_score,
        }
    }

    pub fn compute_churn_metrics_for_range(
        cache_entry: &GitCacheEntry,
        start_line: u32,
        end_line: u32,
    ) -> ChurnMetrics {
        if cache_entry.line_changes.is_empty() {
            return Self::compute_churn_metrics(cache_entry);
        }

        let mut commits = HashSet::new();
        let mut bug_fix_commits = HashSet::new();
        let mut authors = HashSet::new();

        for change in &cache_entry.line_changes {
            if ranges_overlap(start_line, end_line, change.start_line, change.end_line) {
                commits.insert(change.commit.clone());
                if change.is_bug_fix {
                    bug_fix_commits.insert(change.commit.clone());
                }
                authors.insert(change.author.clone());
            }
        }

        if commits.is_empty() {
            return ChurnMetrics {
                times_modified: 0,
                bug_fix_commits: 0,
                authors_count: 0,
                authors: Vec::new(),
                churn_score: 0.0,
            };
        }

        let authors_count = authors.len().max(1);
        let churn_score = (commits.len() as f64 + (bug_fix_commits.len() as f64 * 2.0))
            * (authors_count as f64 + 1.0).log10();

        ChurnMetrics {
            times_modified: commits.len(),
            bug_fix_commits: bug_fix_commits.len(),
            authors_count,
            authors: authors.into_iter().collect(),
            churn_score,
        }
    }

    fn is_git_cache_compatible(
        metadata: Option<&GitCacheMetadata>,
        repository_path: &Path,
        branch: &str,
        bug_fix_patterns: &BugFixPatterns,
    ) -> bool {
        let Some(metadata) = metadata else {
            return false;
        };

        metadata.algorithm_version == GIT_ALGORITHM_VERSION
            && metadata.repository_path == repository_path.to_string_lossy()
            && metadata.branch == branch
            && !metadata.head_oid.is_empty()
            && metadata.bug_fix_patterns_hash == bug_fix_patterns.cache_key()
    }

    fn get_modified_files_optimized(repo: &Repository, commit: &Commit) -> Result<ModifiedFiles> {
        let mut files = HashMap::new();
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
                        files.insert(
                            path_str.to_string(),
                            vec![ChangedRange {
                                start_line: 1,
                                end_line: FULL_FILE_RANGE_END_LINE,
                            }],
                        );
                    }
                }
                git2::TreeWalkResult::Ok
            })?;
        }

        Ok(ModifiedFiles { files, renames })
    }

    fn collect_diff_paths(
        repo: &Repository,
        old_tree: Option<&git2::Tree>,
        new_tree: Option<&git2::Tree>,
        files: &mut HashMap<String, Vec<ChangedRange>>,
        renames: &mut Vec<(String, String)>,
    ) -> Result<()> {
        let mut opts = DiffOptions::new();
        opts.pathspec("*");
        opts.context_lines(0);

        let mut diff = repo.diff_tree_to_tree(old_tree, new_tree, Some(&mut opts))?;
        let mut find_opts = DiffFindOptions::new();
        find_opts.renames(true);
        diff.find_similar(Some(&mut find_opts))?;

        let collected_files = RefCell::new(HashMap::<String, Vec<ChangedRange>>::new());
        let collected_renames = RefCell::new(Vec::<(String, String)>::new());
        diff.foreach(
            &mut |delta, _| {
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
                        collected_renames.borrow_mut().push((old_path, new_path));
                    }
                }

                if let Some(path) = new_path {
                    collected_files.borrow_mut().entry(path).or_default();
                }
                true
            },
            None,
            Some(&mut |delta, hunk| {
                let Some(path) = hunk_path(delta) else {
                    return true;
                };
                collected_files
                    .borrow_mut()
                    .entry(path)
                    .or_default()
                    .push(changed_range_from_hunk(hunk));
                true
            }),
            None,
        )?;
        files.extend(collected_files.into_inner());
        renames.extend(collected_renames.into_inner());

        Ok(())
    }

    fn process_commit_batch(
        repo_path: &Path,
        oids: &[Oid],
        bug_fix_patterns: &BugFixPatterns,
    ) -> GitBatchMetrics {
        oids.par_chunks(COMMITS_PER_WORKER_CHUNK)
            .map(|chunk| Self::process_commit_chunk(repo_path, chunk, bug_fix_patterns))
            .reduce(GitBatchMetrics::default, |mut acc, metrics| {
                Self::merge_batch_metrics(&mut acc, metrics);
                acc
            })
    }

    fn process_commit_chunk(
        repo_path: &Path,
        oids: &[Oid],
        bug_fix_patterns: &BugFixPatterns,
    ) -> GitBatchMetrics {
        match Self::with_thread_local_repo(repo_path, |repo| {
            let mut metrics = GitBatchMetrics::default();
            for oid in oids {
                match Self::process_commit(repo, *oid, bug_fix_patterns) {
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

    fn process_commit(
        repo: &Repository,
        oid: Oid,
        bug_fix_patterns: &BugFixPatterns,
    ) -> Result<GitBatchMetrics> {
        let commit = repo.find_commit(oid)?;
        let author = Self::author_identity(&commit);
        let is_bug_fix = Self::is_bug_fix(&commit, bug_fix_patterns);
        let modified_files = Self::get_modified_files_optimized(repo, &commit)?;

        let mut metrics = HashMap::new();
        let commit_id = oid.to_string();
        for (file_path, mut ranges) in modified_files.files {
            let entry: &mut GitCacheEntry = metrics.entry(file_path).or_default();
            entry.times_modified += 1;
            if is_bug_fix {
                entry.bug_fix_commits += 1;
            }
            entry.authors.insert(author.clone());
            if ranges.is_empty() {
                ranges.push(ChangedRange {
                    start_line: 1,
                    end_line: FULL_FILE_RANGE_END_LINE,
                });
            }
            for range in ranges {
                entry.line_changes.push(LineChange {
                    commit: commit_id.clone(),
                    start_line: range.start_line,
                    end_line: range.end_line,
                    is_bug_fix,
                    author: author.clone(),
                });
            }
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
            target_entry.line_changes.extend(source_entry.line_changes);
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
            target_entry.line_changes.extend(entry.line_changes);
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

    fn is_bug_fix(commit: &Commit, bug_fix_patterns: &BugFixPatterns) -> bool {
        commit
            .message()
            .is_some_and(|message| bug_fix_patterns.is_match(message))
    }
}

impl BugFixPatterns {
    pub fn from_patterns(patterns: &[String]) -> Result<Self> {
        if patterns.is_empty() {
            return Ok(Self::default());
        }
        let mut compiled = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            compiled.push(Regex::new(pattern)?);
        }
        Ok(Self {
            patterns: compiled,
            cache_key: patterns_cache_key(&patterns.join("\n")),
        })
    }

    pub fn cache_key(&self) -> &str {
        &self.cache_key
    }

    fn is_match(&self, message: &str) -> bool {
        self.patterns
            .iter()
            .any(|pattern| pattern.is_match(message))
    }
}

impl Default for BugFixPatterns {
    fn default() -> Self {
        let patterns = [
            r"(?i)\bfix(?:e[sd])?\b",
            r"(?i)\bbug(?:s)?\b",
            r"(?i)\bissue(?:s)?\b",
            r"(?i)\bclos(?:e[sd]?|ing)\b",
            r"(?i)\bresolv(?:e[sd]?|ing)\b",
        ];
        let compiled = patterns
            .iter()
            .map(|pattern| Regex::new(pattern).expect("default bug-fix pattern should compile"))
            .collect();
        Self {
            patterns: compiled,
            cache_key: patterns_cache_key(&patterns.join("\n")),
        }
    }
}

fn patterns_cache_key(patterns: &str) -> String {
    format!("{:032x}", xxh3_128(patterns.as_bytes()))
}

fn hunk_path(delta: DiffDelta) -> Option<String> {
    delta
        .new_file()
        .path()
        .or_else(|| delta.old_file().path())
        .and_then(|path| path.to_str())
        .map(str::to_string)
}

fn changed_range_from_hunk(hunk: DiffHunk) -> ChangedRange {
    let start_line = hunk.new_start().max(1);
    let line_count = hunk.new_lines().max(1);
    ChangedRange {
        start_line,
        end_line: start_line.saturating_add(line_count).saturating_sub(1),
    }
}

fn ranges_overlap(left_start: u32, left_end: u32, right_start: u32, right_end: u32) -> bool {
    left_start <= right_end && right_start <= left_end
}
