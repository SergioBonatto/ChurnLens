use churnlens::analyze_repository;
use git2::{Repository, Signature};
use std::fs;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[test]
fn analyzes_small_git_repository() {
    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let repo_path = temp_dir.path();
    let source_dir = repo_path.join("src");
    fs::create_dir(&source_dir).expect("source dir should be created");
    fs::write(
        source_dir.join("index.ts"),
        r#"
        function a() {
            if (x) {}
        }

        const b = () => {};
        "#,
    )
    .expect("source file should be written");

    let repo = Repository::init(repo_path).expect("repo should be initialized");
    let mut index = repo.index().expect("index should be available");
    index
        .add_path(std::path::Path::new("src/index.ts"))
        .expect("source file should be staged");
    index.write().expect("index should be written");
    let tree_id = index.write_tree().expect("tree should be written");
    let tree = repo.find_tree(tree_id).expect("tree should exist");
    let signature =
        Signature::now("Test User", "test@example.com").expect("signature should build");
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "initial commit",
        &tree,
        &[],
    )
    .expect("commit should be created");

    let report = analyze_repository(
        repo_path,
        "churn_score",
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .expect("repository should be analyzed");

    assert_eq!(report.schema_version, "0.1.0");
    assert_eq!(report.summary.total_functions, 2);
    assert_eq!(report.functions.len(), 2);
    assert!(report.analysis.commit.len() >= 40);
    assert!(report
        .functions
        .iter()
        .all(|function| function.normalized.is_some()));
    assert!(report
        .functions
        .iter()
        .all(|function| function.risk.is_some()));
    assert!(report
        .functions
        .iter()
        .all(|function| function.percentile.is_some()));
}
