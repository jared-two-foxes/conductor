use conductor::{ReviewDecision, ReviewEvaluation, Status, WorkflowRequest, run_with_reviewer};
use git2::Repository;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;

fn request(
    root: &std::path::Path,
    operations: serde_json::Value,
    checks: serde_json::Value,
) -> WorkflowRequest {
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
        assert_eq!(
            review.requirements.as_deref(),
            Some("Create the requested file")
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("created.txt")).unwrap(),
            "hello"
        );
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
    assert_eq!(
        fs::read_to_string(dir.path().join("created.txt")).unwrap(),
        "hello"
    );
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
    assert_eq!(
        result.error.as_deref(),
        Some("No review provider configured")
    );
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

#[test]
fn forwards_max_repeated_actions_and_completion_attempts_to_review_config() {
    let dir = tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    let input: WorkflowRequest = serde_json::from_value(json!({
        "schema": "conductor.request/v1",
        "repository_path": dir.path(),
        "execution": { "operations": [] },
        "checks": [],
        "review": {
            "requirements": "Create the requested file",
            "max_repeated_actions": 7,
            "max_completion_attempts": 5
        }
    }))
    .unwrap();
    let result = run_with_reviewer(&input, |_, config| {
        assert_eq!(config.max_repeated_actions, 7);
        assert_eq!(config.max_completion_attempts, 5);
        approved()
    });
    assert_eq!(result.status, Status::Passed);
}

#[test]
fn rejects_zero_max_repeated_actions() {
    let dir = tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    let input: WorkflowRequest = serde_json::from_value(json!({
        "schema": "conductor.request/v1",
        "repository_path": dir.path(),
        "execution": { "operations": [] },
        "checks": [],
        "review": {
            "requirements": "Create the requested file",
            "max_repeated_actions": 0
        }
    }))
    .unwrap();
    let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.phase, "input");
}

#[test]
fn rejects_zero_max_completion_attempts() {
    let dir = tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    let input: WorkflowRequest = serde_json::from_value(json!({
        "schema": "conductor.request/v1",
        "repository_path": dir.path(),
        "execution": { "operations": [] },
        "checks": [],
        "review": {
            "requirements": "Create the requested file",
            "max_completion_attempts": 0
        }
    }))
    .unwrap();
    let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.phase, "input");
}

fn init_repo_with_commit(dir: &Path) {
    let repo = Repository::init(dir).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();
    fs::write(dir.join("initial.txt"), "initial\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("initial.txt")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();
}

fn create_file_operations(n: usize) -> serde_json::Value {
    let ops: Vec<serde_json::Value> = (0..n)
        .map(|i| json!({"type": "create_file", "path": format!("file_{i}.txt"), "content": "x"}))
        .collect();
    json!(ops)
}

#[test]
fn scales_review_limits_from_changed_file_count() {
    let dir = tempdir().unwrap();
    init_repo_with_commit(dir.path());
    let input = request(dir.path(), create_file_operations(5), json!([]));
    let result = run_with_reviewer(&input, |_, config| {
        // N=5: max(10, 8+5*2)=18, max(20, 12+5*3)=27, min(max(60, 30+5*8), 600)=70
        assert_eq!(config.max_turns, 18);
        assert_eq!(config.max_tool_calls, 27);
        assert_eq!(config.wall_clock_budget, Some(Duration::from_secs(70)));
        approved()
    });
    assert_eq!(result.status, Status::Passed);
    let budget = result.review_budget.expect("review budget should be set");
    assert_eq!(budget.changed_files, 5);
    assert_eq!(budget.max_turns, 18);
    assert_eq!(budget.max_tool_calls, 27);
    assert_eq!(budget.wall_clock_budget_secs, 70);
}

#[test]
fn explicit_limits_override_scaling() {
    let dir = tempdir().unwrap();
    init_repo_with_commit(dir.path());
    let input: WorkflowRequest = serde_json::from_value(json!({
        "schema": "conductor.request/v1",
        "repository_path": dir.path(),
        "execution": { "operations": create_file_operations(10) },
        "checks": [],
        "review": {
            "requirements": "Create the requested file",
            "max_turns": 5,
            "max_tool_calls": 15,
            "wall_clock_budget_secs": 30
        }
    }))
    .unwrap();
    let result = run_with_reviewer(&input, |_, config| {
        assert_eq!(config.max_turns, 5);
        assert_eq!(config.max_tool_calls, 15);
        assert_eq!(config.wall_clock_budget, Some(Duration::from_secs(30)));
        approved()
    });
    assert_eq!(result.status, Status::Passed);
    let budget = result.review_budget.expect("review budget should be set");
    assert_eq!(budget.changed_files, 10);
    assert_eq!(budget.max_turns, 5);
    assert_eq!(budget.max_tool_calls, 15);
    assert_eq!(budget.wall_clock_budget_secs, 30);
}

#[test]
fn caps_scaled_wall_clock_budget() {
    let dir = tempdir().unwrap();
    init_repo_with_commit(dir.path());
    let input = request(dir.path(), create_file_operations(72), json!([]));
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    let budget = result.review_budget.expect("review budget should be set");
    // N=72: 30+72*8=606, capped to 600
    assert_eq!(budget.changed_files, 72);
    assert_eq!(budget.wall_clock_budget_secs, 600);
}

#[test]
fn reports_review_budget_in_result() {
    let dir = tempdir().unwrap();
    init_repo_with_commit(dir.path());
    let input = request(dir.path(), create_file_operations(3), json!([]));
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    let budget = result.review_budget.expect("review budget should be set");
    // N=3: max(10, 8+3*2)=14, max(20, 12+3*3)=21, min(max(60, 30+3*8), 600)=60
    assert_eq!(budget.changed_files, 3);
    assert_eq!(budget.max_turns, 14);
    assert_eq!(budget.max_tool_calls, 21);
    assert_eq!(budget.wall_clock_budget_secs, 60);
}

#[test]
fn deduplicates_overlapping_tracked_and_untracked_paths() {
    let dir = tempdir().unwrap();
    init_repo_with_commit(dir.path());
    // Remove initial.txt from the index so it appears in both
    // git diff --name-only HEAD (as deleted from index) and
    // git ls-files --others (as untracked), then modify it.
    let rm_output = std::process::Command::new("git")
        .args(["rm", "--cached", "initial.txt"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        rm_output.status.success(),
        "git rm --cached failed: {}",
        String::from_utf8_lossy(&rm_output.stderr)
    );
    fs::write(dir.path().join("initial.txt"), "modified\n").unwrap();
    // Create one new file via executor.
    let input = request(dir.path(), create_file_operations(1), json!([]));
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    let budget = result.review_budget.expect("review budget should be set");
    // initial.txt appears in both git diff and git ls-files --others,
    // but should be counted once. Plus file_0.txt = 2 total.
    assert_eq!(budget.changed_files, 2);
}

#[test]
fn returns_zero_when_git_diff_fails() {
    let dir = tempdir().unwrap();
    init_repo_with_commit(dir.path());
    // Create untracked files via executor so git ls-files would report them,
    // but set an invalid base_ref so git diff fails.
    let input: WorkflowRequest = serde_json::from_value(json!({
        "schema": "conductor.request/v1",
        "repository_path": dir.path(),
        "execution": { "operations": create_file_operations(3) },
        "checks": [],
        "review": {
            "requirements": "Create the requested file",
            "base_ref": "nonexistent-ref-12345"
        }
    }))
    .unwrap();
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    let budget = result.review_budget.expect("review budget should be set");
    // git diff fails on invalid base_ref, so count_changed_files returns 0
    // even though untracked files exist.
    assert_eq!(budget.changed_files, 0);
}

#[test]
fn writes_ledger_by_default() {
    let dir = tempdir().unwrap();
    init_repo_with_commit(dir.path());
    let input = request(dir.path(), create_file_operations(1), json!([]));
    let result = run_with_reviewer(&input, |_, config| {
        let ledger_path = config
            .ledger_path
            .as_ref()
            .expect("ledger_path should be set");
        assert!(ledger_path.ends_with(".conductor/ledger.jsonl"));
        approved()
    });
    assert_eq!(result.status, Status::Passed);
    let ledger_path = result
        .ledger_path
        .as_ref()
        .expect("ledger_path should be in result");
    assert!(ledger_path.ends_with(".conductor/ledger.jsonl"));
    assert!(dir.path().join(".conductor").exists());
    let exclude = fs::read_to_string(dir.path().join(".git").join("info").join("exclude")).unwrap();
    assert!(exclude.lines().any(|l| l.trim() == ".conductor/"));
}

#[test]
fn disables_ledger_when_requested() {
    let dir = tempdir().unwrap();
    init_repo_with_commit(dir.path());
    let input: WorkflowRequest = serde_json::from_value(json!({
        "schema": "conductor.request/v1",
        "repository_path": dir.path(),
        "execution": { "operations": [] },
        "checks": [],
        "review": {
            "requirements": "Create the requested file",
            "ledger": false
        }
    }))
    .unwrap();
    let result = run_with_reviewer(&input, |_, config| {
        assert!(config.ledger_path.is_none());
        approved()
    });
    assert_eq!(result.status, Status::Passed);
    assert!(result.ledger_path.is_none());
}

#[test]
fn ledger_dir_excluded_from_changed_file_count() {
    let dir = tempdir().unwrap();
    init_repo_with_commit(dir.path());
    // First run creates .conductor/ and adds it to .git/info/exclude
    let input = request(dir.path(), create_file_operations(1), json!([]));
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    // Simulate the ledger file that the real review would write
    fs::write(dir.path().join(".conductor").join("ledger.jsonl"), "{}\n").unwrap();
    // Second run: .conductor/ledger.jsonl should NOT be counted
    let input2 = request(
        dir.path(),
        json!([{"type": "create_file", "path": "file_1.txt", "content": "y"}]),
        json!([]),
    );
    let result2 = run_with_reviewer(&input2, |_, _| approved());
    assert_eq!(result2.status, Status::Passed);
    let budget = result2.review_budget.expect("review budget should be set");
    // file_0.txt from first run + file_1.txt from second run = 2
    // .conductor/ledger.jsonl is excluded, so not counted
    assert_eq!(budget.changed_files, 2);
}

#[test]
fn blocks_when_ledger_dir_cannot_be_created() {
    let dir = tempdir().unwrap();
    init_repo_with_commit(dir.path());
    // Create a regular file at .conductor to block directory creation
    fs::write(dir.path().join(".conductor"), "blocking file").unwrap();
    let input = request(dir.path(), create_file_operations(1), json!([]));
    let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.phase, "review");
    assert!(
        result
            .error
            .as_deref()
            .unwrap()
            .contains("ledger directory")
    );
}
