use conductor::{
    ReviewDecision, ReviewEvaluation, Status, WorkflowRequest, run_with_reviewer,
};
use git2::Repository;
use serde_json::json;
use std::fs;
use tempfile::tempdir;

fn request(root: &std::path::Path, operations: serde_json::Value, checks: serde_json::Value) -> WorkflowRequest {
    serde_json::from_value(json!({
        "schema": "conductor.request/v1",
        "repository_path": root,
        "execution": { "operations": operations },
        "checks": checks,
        "review": { "requirements": "Create the requested file" }
    }))
    .unwrap()
}

fn approved() -> ReviewEvaluation {
    ReviewEvaluation {
        decision: ReviewDecision::Approved,
        result: Some(json!({"status": "APPROVED"})),
        error: None,
    }
}

#[test]
fn applies_checks_and_reviews_in_order() {
    let dir = tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    let input = request(
        dir.path(),
        json!([{"type": "create_file", "path": "created.txt", "content": "hello"}]),
        json!([{"name": "git check", "program": "git", "args": ["status", "--porcelain"]}]),
    );
    let result = run_with_reviewer(&input, |review, _config| {
        assert_eq!(review.head_ref, ":working");
        assert_eq!(review.base_ref, "HEAD");
        assert_eq!(review.requirements.as_deref(), Some("Create the requested file"));
        assert_eq!(fs::read_to_string(dir.path().join("created.txt")).unwrap(), "hello");
        approved()
    });
    assert_eq!(result.status, Status::Passed);
    assert_eq!(result.phase, "review");
    assert_eq!(result.execution.unwrap().operations_applied, 1);
    assert_eq!(result.checks.len(), 1);
}

#[test]
fn executor_failure_returns_to_design_without_checks_or_review() {
    let dir = tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    let input = request(
        dir.path(),
        json!([
            {"type": "create_file", "path": "created.txt", "content": "hello"},
            {"type": "delete_file", "path": "missing.txt"}
        ]),
        json!([{"name": "check", "program": "git", "args": ["status"]}]),
    );
    let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
    assert_eq!(result.status, Status::NeedsDesign);
    assert_eq!(result.phase, "execution");
    assert_eq!(result.execution.unwrap().operations_applied, 1);
    assert!(result.checks.is_empty());
    assert_eq!(fs::read_to_string(dir.path().join("created.txt")).unwrap(), "hello");
}

#[test]
fn failing_check_returns_to_design_without_review() {
    let dir = tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    let input = request(
        dir.path(),
        json!([]),
        json!([{"name": "broken check", "program": "git", "args": ["not-a-git-command"]}]),
    );
    let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
    assert_eq!(result.status, Status::NeedsDesign);
    assert_eq!(result.phase, "verification");
    assert_eq!(result.checks.len(), 1);
    assert_ne!(result.checks[0].exit_code, Some(0));
}

#[test]
fn unavailable_check_is_blocked_without_review() {
    let dir = tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    let input = request(
        dir.path(),
        json!([]),
        json!([{"name": "missing", "program": "this-command-does-not-exist-8f4d2e"}]),
    );
    let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.phase, "verification");
}

#[test]
fn review_changes_requested_returns_to_design() {
    let dir = tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    let input = request(dir.path(), json!([]), json!([]));
    let result = run_with_reviewer(&input, |_, _| ReviewEvaluation {
        decision: ReviewDecision::ChangesRequested,
        result: Some(json!({"status": "CHANGES_REQUESTED", "findings": ["fix"]})),
        error: None,
    });
    assert_eq!(result.status, Status::NeedsDesign);
    assert_eq!(result.phase, "review");
    assert!(result.review.is_some());
}

#[test]
fn unavailable_review_is_blocked() {
    let dir = tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    let input = request(dir.path(), json!([]), json!([]));
    let result = run_with_reviewer(&input, |_, _| ReviewEvaluation {
        decision: ReviewDecision::Indeterminate,
        result: None,
        error: Some("No review provider configured".into()),
    });
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.error.as_deref(), Some("No review provider configured"));
}

#[test]
fn rejects_invalid_schema_before_applying_changes() {
    let dir = tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    let mut input = request(
        dir.path(),
        json!([{"type": "create_file", "path": "created.txt", "content": "hello"}]),
        json!([]),
    );
    input.schema = "wrong".into();
    let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
    assert_eq!(result.status, Status::Blocked);
    assert!(!dir.path().join("created.txt").exists());
}
