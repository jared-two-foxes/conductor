use conductor::{ReviewDecision, ReviewEvaluation, Status, WorkflowRequest, run_with_reviewer};
use git2::{Repository, Signature};
use serde_json::{Value, json};
use std::fs::{self, FileTimes, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, UNIX_EPOCH};
use tempfile::{TempDir, tempdir};

struct Fixture {
    _directory: TempDir,
    executable: PathBuf,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let directory = tempdir().unwrap();
        let executable = directory.path().join(if cfg!(windows) {
            "formatter.exe"
        } else {
            "formatter"
        });
        let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formatter.rs");
        let output = Command::new(compiler)
            .arg("--edition=2024")
            .arg(source)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Fixture {
            _directory: directory,
            executable,
        }
    })
}

fn repository() -> TempDir {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    fs::write(dir.path().join("unrelated.rs"), "untouched").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("unrelated.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let signature = Signature::now("Formatting tests", "formatting@example.com").unwrap();
    repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
        .unwrap();
    dir
}

fn rule(mode: &str) -> Value {
    json!({
        "name": mode,
        "extensions": ["rs"],
        "program": fixture().executable,
        "args": [mode, "{path}"]
    })
}

fn request(root: &Path, operations: Value, formatters: Value, checks: Value) -> WorkflowRequest {
    serde_json::from_value(json!({
        "schema": "conductor.request/v1",
        "repository_path": root,
        "execution": {"operations": operations},
        "formatters": formatters,
        "checks": checks,
        "review": {"requirements": "Format only the affected files", "ledger": false}
    }))
    .unwrap()
}

fn create(path: &str, content: &str) -> Value {
    json!({"type": "create_file", "path": path, "content": content})
}

fn approved() -> ReviewEvaluation {
    ReviewEvaluation {
        decision: ReviewDecision::Approved,
        result: Some(json!({"status": "APPROVED"})),
        error: None,
    }
}

fn no_checks() -> Value {
    json!([])
}

#[test]
fn invalid_rules_are_rejected_before_execution() {
    let invalid = [
        json!([{"name": "", "extensions": ["rs"], "program": "missing"}]),
        json!([{"name": "rule", "extensions": ["rs"], "program": " "}]),
        json!([{"name": "rule", "extensions": [], "program": "missing"}]),
        json!([{"name": "rule", "extensions": [""], "program": "missing"}]),
        json!([{"name": "rule", "extensions": [".rs"], "program": "missing"}]),
        json!([{"name": "rule", "extensions": ["r s"], "program": "missing"}]),
        json!([{"name": "rule", "extensions": ["rs", "RS"], "program": "missing"}]),
        json!([
            {"name": "one", "extensions": ["rs"], "program": "missing"},
            {"name": "two", "extensions": ["RS"], "program": "missing"}
        ]),
    ];
    for rules in invalid {
        let dir = repository();
        let input = request(
            dir.path(),
            json!([create("file.rs", "source")]),
            rules,
            no_checks(),
        );
        let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
        assert_eq!(result.status, Status::Blocked);
        assert_eq!(result.phase, "input");
        assert!(result.execution.is_none());
        assert!(result.formatters.is_empty());
        assert!(!dir.path().join("file.rs").exists());
    }
}

#[test]
fn omitted_empty_and_unmatched_rules_preserve_existing_behavior() {
    for rules in [
        json!([]),
        json!([{"name": "skip", "extensions": ["js"], "program": "missing-formatter-9ee1"}]),
    ] {
        let dir = repository();
        let input = request(
            dir.path(),
            json!([create("file.rs", "source")]),
            rules,
            no_checks(),
        );
        let result = run_with_reviewer(&input, |_, _| approved());
        assert_eq!(result.status, Status::Passed);
        assert!(result.formatters.is_empty());
        assert_eq!(
            fs::read_to_string(dir.path().join("file.rs")).unwrap(),
            "source"
        );
    }
    let dir = repository();
    let input: WorkflowRequest = serde_json::from_value(json!({
        "schema": "conductor.request/v1", "repository_path": dir.path(),
        "execution": {"operations": [create("file.rs", "source")]},
        "review": {"requirements": "Legacy request", "ledger": false}
    }))
    .unwrap();
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    assert_eq!(
        serde_json::to_value(result).unwrap()["formatters"],
        json!([])
    );
}

#[test]
fn rules_and_paths_are_ordered_and_only_surviving_affected_files_are_formatted() {
    let dir = repository();
    fs::write(dir.path().join("unrelated.rs"), "unrelated dirty file").unwrap();
    fs::write(dir.path().join("untracked.rs"), "untracked").unwrap();
    let mut text_rule = rule("upper");
    text_rule["name"] = json!("text");
    text_rule["extensions"] = json!(["txt"]);
    let input = request(
        dir.path(),
        json!([
            create("z.rs", "z"), create("a.RS", "a"), create("first.txt", "text"),
            create("deleted.rs", "gone"), create("ignored.md", "untouched"),
            {"type": "move_file", "from": "z.rs", "to": "b.rs"},
            {"type": "edit_file", "path": "a.RS", "edits": [{"oldText": "a", "newText": "a"}]},
            {"type": "delete_file", "path": "deleted.rs"}
        ]),
        json!([text_rule, rule("upper")]),
        no_checks(),
    );
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    let paths: Vec<_> = result
        .formatters
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(paths, vec!["first.txt", "a.RS", "b.rs"]);
    assert!(result.formatters.iter().all(|entry| entry.changed));
    assert_eq!(fs::read_to_string(dir.path().join("a.RS")).unwrap(), "A");
    assert_eq!(fs::read_to_string(dir.path().join("b.rs")).unwrap(), "Z");
    for (path, expected) in [
        ("unrelated.rs", "unrelated dirty file"),
        ("untracked.rs", "untracked"),
        ("ignored.md", "untouched"),
    ] {
        assert_eq!(fs::read_to_string(dir.path().join(path)).unwrap(), expected);
    }
    assert!(!dir.path().join("deleted.rs").exists());
    assert!(!dir.path().join("z.rs").exists());
    assert_eq!(
        result.execution.unwrap().affected_files,
        vec!["a.RS", "b.rs", "first.txt", "ignored.md"]
    );
}

#[test]
fn placeholders_supply_context_without_shell_or_filename_appending() {
    for path in ["nested dir/file.rs", "root.rs"] {
        let dir = repository();
        fs::write(dir.path().join("root.marker"), "marker").unwrap();
        fs::create_dir(dir.path().join("nested dir")).unwrap();
        let mut context = rule("context");
        context["args"] = json!(["context", "{path}", "{parent}", "flag={path}"]);
        let input = request(
            dir.path(),
            json!([create(path, "body")]),
            json!([context]),
            no_checks(),
        );
        let result = run_with_reviewer(&input, |_, _| approved());
        assert_eq!(result.status, Status::Passed);
        let parent = if path == "root.rs" { "." } else { "nested dir" };
        assert_eq!(
            fs::read_to_string(dir.path().join(path)).unwrap(),
            format!("body|{path}|{parent}|flag={{path}}|true")
        );
        assert_eq!(
            result.formatters[0].args,
            vec!["context", path, parent, "flag={path}"]
        );
    }
}

#[test]
fn relative_executables_and_default_arguments_work() {
    let dir = repository();
    let name = if cfg!(windows) {
        "formatter.exe"
    } else {
        "formatter"
    };
    fs::copy(&fixture().executable, dir.path().join(name)).unwrap();
    let input = request(
        dir.path(),
        json!([create("file.rs", "source")]),
        json!([{
            "name": "identity", "extensions": ["rs"], "program": format!("./{name}")
        }]),
        no_checks(),
    );
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    assert!(!result.formatters[0].changed);
    assert!(result.formatters[0].args.is_empty());
}

#[test]
fn checks_and_review_observe_formatted_contents() {
    let dir = repository();
    fs::write(dir.path().join("expected.txt"), "FORMATTED").unwrap();
    let input = request(
        dir.path(),
        json!([create("file.rs", "formatted")]),
        json!([rule("upper")]),
        json!([{
            "name": "formatted contents", "program": "git",
            "args": ["diff", "--no-index", "--", "expected.txt", "file.rs"]
        }]),
    );
    let result = run_with_reviewer(&input, |_, _| {
        assert_eq!(
            fs::read_to_string(dir.path().join("file.rs")).unwrap(),
            "FORMATTED"
        );
        approved()
    });
    assert_eq!(result.status, Status::Passed);
    assert_eq!(result.checks[0].exit_code, Some(0));
    assert!(result.formatters[0].changed);
}

#[test]
fn execution_failure_skips_formatting() {
    let dir = repository();
    let input = request(
        dir.path(),
        json!([
            create("file.rs", "source"), {"type": "delete_file", "path": "missing"}
        ]),
        json!([{"name": "missing", "extensions": ["rs"], "program": "missing-formatter-9ee1"}]),
        no_checks(),
    );
    let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
    assert_eq!(result.status, Status::NeedsDesign);
    assert_eq!(result.phase, "execution");
    assert!(result.formatters.is_empty());
}

#[test]
fn formatter_failure_retains_prior_writes_and_skips_later_phases() {
    let dir = repository();
    let input = request(
        dir.path(),
        json!([
            create("c.rs", "c"), create("b.rs", "b"), create("a.rs", "a")
        ]),
        json!([rule("select-fail")]),
        json!([{"name": "must not run", "program": "git", "args": ["status"]}]),
    );
    let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
    assert_eq!(result.status, Status::NeedsDesign);
    assert_eq!(result.phase, "formatting");
    assert!(result.checks.is_empty());
    assert!(result.review.is_none());
    assert_eq!(result.formatters.len(), 2);
    assert_eq!(result.formatters[0].exit_code, Some(0));
    assert!(result.formatters[0].changed);
    assert_eq!(result.formatters[1].exit_code, Some(42));
    assert!(!result.formatters[1].changed);
    assert!(result.formatters[1].stderr.contains("fixture formatter failed"));
    for (path, expected) in [("a.rs", "A"), ("b.rs", "b"), ("c.rs", "c")] {
        assert_eq!(fs::read_to_string(dir.path().join(path)).unwrap(), expected);
    }
    assert_eq!(
        result.execution.unwrap().affected_files,
        vec!["a.rs", "b.rs", "c.rs"]
    );
}

#[test]
fn launch_failure_is_blocked_without_replacement_or_review() {
    let dir = repository();
    let input = request(
        dir.path(),
        json!([create("file.rs", "source")]),
        json!([{
            "name": "missing", "extensions": ["rs"], "program": "missing-formatter-9ee1"
        }]),
        no_checks(),
    );
    let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.phase, "formatting");
    assert_eq!(result.formatters.len(), 1);
    assert_eq!(result.formatters[0].exit_code, None);
    assert!(!result.formatters[0].changed);
    assert!(result.error.unwrap().contains("missing"));
    assert_eq!(
        fs::read_to_string(dir.path().join("file.rs")).unwrap(),
        "source"
    );
}

#[test]
fn invalid_and_empty_stdout_do_not_replace_nonempty_source() {
    for mode in ["invalid", "empty", "fail"] {
        let dir = repository();
        let input = request(
            dir.path(),
            json!([create("file.rs", "source")]),
            json!([rule(mode)]),
            no_checks(),
        );
        let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
        assert_eq!(result.status, Status::NeedsDesign);
        assert_eq!(result.phase, "formatting");
        assert!(!result.formatters[0].changed);
        assert_eq!(
            fs::read_to_string(dir.path().join("file.rs")).unwrap(),
            "source"
        );
    }
    let dir = repository();
    let input = request(
        dir.path(),
        json!([create("file.rs", "")]),
        json!([rule("empty")]),
        no_checks(),
    );
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    assert!(!result.formatters[0].changed);
    assert!(fs::read(dir.path().join("file.rs")).unwrap().is_empty());
}

#[test]
fn invalid_output_takes_precedence_when_formatter_does_not_consume_large_stdin() {
    for (mode, status, reason) in [
        ("early-empty", Status::NeedsDesign, "empty"),
        ("early-invalid", Status::NeedsDesign, "UTF-8"),
        ("early-valid", Status::Blocked, "stdin"),
    ] {
        let dir = repository();
        let source = "s".repeat(524_288);
        let input = request(
            dir.path(),
            json!([create("file.rs", &source)]),
            json!([rule(mode)]),
            no_checks(),
        );
        let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
        assert_eq!(result.status, status);
        assert_eq!(result.phase, "formatting");
        assert_eq!(result.formatters[0].exit_code, Some(0));
        assert!(!result.formatters[0].changed);
        assert!(result.checks.is_empty());
        assert!(result.error.unwrap().contains(reason));
        assert_eq!(fs::read_to_string(dir.path().join("file.rs")).unwrap(), source);
    }
}

#[test]
fn unchanged_output_preserves_modification_time() {
    let dir = repository();
    let source = dir.path().join("old.rs");
    fs::write(&source, "unchanged").unwrap();
    OpenOptions::new()
        .write(true)
        .open(&source)
        .unwrap()
        .set_times(
            FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000_000)),
        )
        .unwrap();
    let modified = fs::metadata(&source).unwrap().modified().unwrap();
    let input = request(
        dir.path(),
        json!([{"type": "move_file", "from": "old.rs", "to": "new.rs"}]),
        json!([rule("identity")]),
        no_checks(),
    );
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    assert!(!result.formatters[0].changed);
    assert_eq!(
        fs::metadata(dir.path().join("new.rs"))
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );
}

#[test]
fn replacement_preserves_permissions_and_cleans_temporary_siblings() {
    let dir = repository();
    let source = dir.path().join("old.rs");
    fs::write(&source, "lowercase").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
    }
    let original_permissions = fs::metadata(&source).unwrap().permissions();
    let input = request(
        dir.path(),
        json!([{"type": "move_file", "from": "old.rs", "to": "new.rs"}]),
        json!([rule("upper")]),
        no_checks(),
    );
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    assert!(result.formatters[0].changed);
    let permissions = fs::metadata(dir.path().join("new.rs")).unwrap().permissions();
    assert_eq!(permissions.readonly(), original_permissions.readonly());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(permissions.mode() & 0o777, 0o750);
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("new.rs")).unwrap(),
        "LOWERCASE"
    );
    assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".conductor-format-")
    }));
}

#[test]
fn changed_missing_and_non_regular_targets_are_blocked_even_for_identity_output() {
    for mode in ["mutate", "directory", "remove"] {
        let dir = repository();
        let input = request(
            dir.path(),
            json!([create("file.rs", "source")]),
            json!([rule(mode)]),
            no_checks(),
        );
        let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
        assert_eq!(result.status, Status::Blocked);
        assert_eq!(result.phase, "formatting");
        assert_eq!(result.formatters[0].exit_code, Some(0));
        assert!(!result.formatters[0].changed);
        assert!(result.checks.is_empty());
        if mode == "mutate" {
            assert_eq!(
                fs::read_to_string(dir.path().join("file.rs")).unwrap(),
                "external change"
            );
        }
        assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".conductor-format-")
        }));
    }
}

#[test]
fn target_renamed_during_formatting_is_reported_without_following_or_recreating_it() {
    let dir = repository();
    let input = request(
        dir.path(),
        json!([create("file.rs", "original source")]),
        json!([rule("rename")]),
        json!([{"name": "must not run", "program": "git", "args": ["status"]}]),
    );
    let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.phase, "formatting");
    assert_eq!(result.formatters[0].exit_code, Some(0));
    assert!(!result.formatters[0].changed);
    assert!(result.checks.is_empty());
    assert!(result.review.is_none());
    let error = result.error.unwrap();
    assert!(error.contains("file.rs"), "{error}");
    assert!(error.contains("Cannot inspect formatting target"), "{error}");
    assert!(!dir.path().join("file.rs").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("renamed.rs")).unwrap(),
        "original source"
    );
}

#[test]
fn non_utf8_input_is_blocked_before_launch() {
    let dir = repository();
    fs::write(dir.path().join("old.bin"), [255]).unwrap();
    let input = request(
        dir.path(),
        json!([{"type": "move_file", "from": "old.bin", "to": "bad.rs"}]),
        json!([rule("upper")]),
        no_checks(),
    );
    let result = run_with_reviewer(&input, |_, _| panic!("review must not run"));
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.phase, "formatting");
    assert_eq!(result.formatters[0].exit_code, None);
    assert!(result.error.unwrap().contains("UTF-8"));
    assert_eq!(fs::read(dir.path().join("bad.rs")).unwrap(), vec![255]);
}

#[test]
fn large_pipes_are_drained_without_truncating_source_and_stderr_is_bounded() {
    let dir = repository();
    let source = "s".repeat(524_288);
    let input = request(
        dir.path(),
        json!([create("file.rs", &source)]),
        json!([rule("flood")]),
        no_checks(),
    );
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    let formatted = fs::read_to_string(dir.path().join("file.rs")).unwrap();
    assert_eq!(formatted, "P".repeat(131_072) + &source);
    assert_eq!(result.formatters[0].stderr.len(), 16_384);
    let serialized = serde_json::to_value(&result).unwrap();
    assert!(serialized["formatters"][0].get("stdout").is_none());
    assert_eq!(serialized["formatters"][0]["changed"], true);
}

#[test]
fn rustfmt_stdin_uses_local_configuration_without_formatting_child_modules() {
    let dir = repository();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/rustfmt.toml"), "hard_tabs = true\n").unwrap();
    let child = "pub fn child( )->i32{1}\n";
    fs::write(dir.path().join("src/child.rs"), child).unwrap();
    let formatter = std::env::var("RUSTFMT").unwrap_or_else(|_| "rustfmt".into());
    let input = request(
        dir.path(),
        json!([create("src/lib.rs", "mod child;\npub fn sum(a:i32,b:i32)->i32{a+b}\n")]),
        json!([{
            "name": "rust", "extensions": ["rs"], "program": formatter,
            "args": ["--edition", "2024", "--emit", "stdout", "--config-path", "{parent}"]
        }]),
        no_checks(),
    );
    let result = run_with_reviewer(&input, |_, _| approved());
    assert_eq!(result.status, Status::Passed);
    let formatted = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(
        formatted.contains("pub fn sum(a: i32, b: i32) -> i32 {"),
        "{formatted}"
    );
    assert!(formatted.contains("\ta + b\n"), "{formatted}");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/child.rs")).unwrap(),
        child
    );
    assert_eq!(result.formatters.len(), 1);
    assert_eq!(result.formatters[0].path, "src/lib.rs");
}
