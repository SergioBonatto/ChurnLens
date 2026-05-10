use churnlens::analyze_repository;
use churnlens::metrics::AnalysisStatus;
use git2::{Repository, Signature};
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-12,
        "expected {expected}, got {actual}"
    );
}

fn shutdown() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn init_repo(repo_path: &Path) -> Repository {
    Repository::init(repo_path).expect("repo should be initialized")
}

fn commit_all(repo: &Repository, message: &str) {
    let mut index = repo.index().expect("index should be available");
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .expect("files should be staged");
    index.write().expect("index should be written");
    let tree_id = index.write_tree().expect("tree should be written");
    let tree = repo.find_tree(tree_id).expect("tree should exist");
    let signature =
        Signature::now("Test User", "test@example.com").expect("signature should build");
    let parents = match repo.head() {
        Ok(head) => {
            let parent = head.peel_to_commit().expect("head should peel to commit");
            vec![parent]
        }
        Err(_) => Vec::new(),
    };
    let parent_refs = parents.iter().collect::<Vec<_>>();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parent_refs,
    )
    .expect("commit should be created");
}

fn write_source(repo_path: &Path, source: &str) {
    let source_dir = repo_path.join("src");
    fs::create_dir_all(&source_dir).expect("source dir should be created");
    fs::write(source_dir.join("index.ts"), source).expect("source file should be written");
}

#[test]
fn analyzes_small_git_repository() {
    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let repo_path = temp_dir.path();
    write_source(
        repo_path,
        r#"
        function a() {
            if (x) {}
        }

        const b = () => {};
        "#,
    );

    let repo = init_repo(repo_path);
    commit_all(&repo, "initial commit");

    let report =
        analyze_repository(repo_path, "file", None, shutdown()).expect("repository should analyze");

    assert_eq!(report.schema_version, "0.2.0");
    assert_eq!(report.quality.status, AnalysisStatus::Complete);
    assert!(report.quality.warnings.is_empty());
    assert!(report.quality.skipped_files.is_empty());
    assert!(report.quality.git.available);
    assert_eq!(report.summary.total_functions, 2);
    assert_eq!(report.functions.len(), 2);
    assert!(report.analysis.commit.len() >= 40);

    let mut expected = report.functions.clone();
    expected.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.name.cmp(&b.name))
            .then(a.line.cmp(&b.line))
    });
    let actual = report.functions.clone();
    assert_eq!(
        expected
            .iter()
            .map(|function| &function.id)
            .collect::<Vec<_>>(),
        actual
            .iter()
            .map(|function| &function.id)
            .collect::<Vec<_>>()
    );

    assert!(report
        .functions
        .iter()
        .all(|function| function.risk.is_some()));

    let function_a = report
        .functions
        .iter()
        .find(|function| function.name == "a")
        .expect("function a should exist");
    assert_eq!(function_a.cyclomatic_complexity, 2);
    assert_eq!(function_a.cognitive_complexity, 1);
    assert_eq!(function_a.nesting_depth, 1);
    assert!(function_a.lines_of_code >= 3);
    assert!(function_a.lines_of_code <= 5);
    let normalized_a = function_a
        .normalized
        .as_ref()
        .expect("function a should have normalized metrics");
    assert_close(normalized_a.cyclomatic, 1.0);
    assert_close(normalized_a.cognitive, 1.0);
    assert_close(normalized_a.loc, 1.0);
    let percentile_a = function_a
        .percentile
        .as_ref()
        .expect("function a should have percentile metrics");
    assert_close(percentile_a.risk, 100.0);
    assert_close(percentile_a.cognitive, 100.0);

    let function_b = report
        .functions
        .iter()
        .find(|function| function.name == "b")
        .expect("function b should exist");
    assert_eq!(function_b.cyclomatic_complexity, 1);
    assert_eq!(function_b.cognitive_complexity, 0);
    assert_eq!(function_b.nesting_depth, 0);
    assert!(function_b.lines_of_code >= 1);
    assert!(function_b.lines_of_code <= 2);
    let normalized_b = function_b
        .normalized
        .as_ref()
        .expect("function b should have normalized metrics");
    assert_close(
        normalized_b.cyclomatic,
        std::f64::consts::LN_2 / 3.0_f64.ln(),
    );
    assert_close(normalized_b.cognitive, 0.0);
    assert_close(normalized_b.loc, 0.5);
    let percentile_b = function_b
        .percentile
        .as_ref()
        .expect("function b should have percentile metrics");
    assert_close(percentile_b.risk, 0.0);
    assert_close(percentile_b.cognitive, 0.0);

    let json = serde_json::to_string(&report).expect("report should serialize");
    assert!(json.contains("\"total_functions\":2"));
    assert!(json.contains("\"quality\""));
    assert!(json.contains("\"functions\""));
    assert!(json.contains("\"cyclomatic_complexity\""));
    assert!(json.contains("\"timestamp\""));

    let repeated_report =
        analyze_repository(repo_path, "file", None, shutdown()).expect("repository should analyze");
    assert_eq!(
        report.analysis.timestamp,
        repeated_report.analysis.timestamp
    );
    assert_ne!(report.analysis.timestamp, "1970-01-01T00:00:00+00:00");
    assert!(repeated_report.quality.cache.ast_hits > 0);
}

#[test]
fn reparses_dirty_worktree_instead_of_reusing_head_oid_cache() {
    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let repo_path = temp_dir.path();
    write_source(repo_path, "function original() {}\n");
    let repo = init_repo(repo_path);
    commit_all(&repo, "initial commit");

    let first =
        analyze_repository(repo_path, "file", None, shutdown()).expect("first analysis succeeds");
    assert!(first
        .functions
        .iter()
        .any(|function| function.name == "original"));

    write_source(
        repo_path,
        r#"
        function changed() {
            if (x) {}
            if (y) {}
        }
        "#,
    );

    let second =
        analyze_repository(repo_path, "file", None, shutdown()).expect("second analysis succeeds");
    assert!(second
        .functions
        .iter()
        .any(|function| function.name == "changed"));
    assert!(!second
        .functions
        .iter()
        .any(|function| function.name == "original"));
    assert!(second.quality.cache.ast_misses > 0);
}

#[test]
fn reports_partial_analysis_for_parse_errors() {
    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let repo_path = temp_dir.path();
    write_source(repo_path, "function valid() {}\n");
    fs::write(
        repo_path.join("src").join("broken.ts"),
        "function broken( {",
    )
    .expect("broken file should be written");
    let repo = init_repo(repo_path);
    commit_all(&repo, "initial commit");

    let report =
        analyze_repository(repo_path, "file", None, shutdown()).expect("analysis should succeed");

    assert_eq!(report.quality.status, AnalysisStatus::Partial);
    assert_eq!(report.summary.total_functions, 1);
    assert!(report
        .quality
        .skipped_files
        .iter()
        .any(|file| file.path == "src/broken.ts"));
}

#[test]
fn honors_sort_and_limit_arguments() {
    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let repo_path = temp_dir.path();
    write_source(
        repo_path,
        r#"
        function simple() {}
        function complex() {
            if (a) {}
            if (b) {}
            if (c) {}
        }
        "#,
    );
    let repo = init_repo(repo_path);
    commit_all(&repo, "initial commit");

    let report =
        analyze_repository(repo_path, "cognitive", Some(1), shutdown()).expect("analysis succeeds");

    assert_eq!(report.functions.len(), 1);
    assert_eq!(report.functions[0].name, "complex");
}

#[test]
fn propagates_churn_across_renamed_files() {
    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let repo_path = temp_dir.path();
    let source_dir = repo_path.join("src");
    fs::create_dir_all(&source_dir).expect("source dir should be created");
    fs::write(source_dir.join("old.ts"), "function renamed() {}\n")
        .expect("old file should be written");
    let repo = init_repo(repo_path);
    commit_all(&repo, "initial commit");

    fs::rename(source_dir.join("old.ts"), source_dir.join("new.ts"))
        .expect("file should be renamed");
    commit_all(&repo, "rename file");

    let report =
        analyze_repository(repo_path, "file", None, shutdown()).expect("analysis should succeed");
    let function = report
        .functions
        .iter()
        .find(|function| function.file == "src/new.ts" && function.name == "renamed")
        .expect("renamed function should be reported under new path");

    assert!(function.times_modified >= 2);
}

#[test]
fn resets_git_cache_when_branch_changes() {
    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let repo_path = temp_dir.path();
    write_source(repo_path, "function branchSensitive() {}\n");
    let repo = init_repo(repo_path);
    commit_all(&repo, "initial commit");

    analyze_repository(repo_path, "file", None, shutdown()).expect("first analysis should succeed");

    let head_commit = repo
        .head()
        .expect("head should exist")
        .peel_to_commit()
        .expect("head should peel to commit");
    repo.branch("other", &head_commit, false)
        .expect("branch should be created");
    repo.set_head("refs/heads/other")
        .expect("head should switch to branch");
    repo.checkout_head(None).expect("checkout should succeed");

    let report =
        analyze_repository(repo_path, "file", None, shutdown()).expect("analysis should succeed");

    assert!(report.quality.git.cache_reset);
}
