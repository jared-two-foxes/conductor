use conductor::{Status, WorkflowRequest, run};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use tempfile::{TempDir, tempdir};

fn fixture() -> &'static Path {
    static FIXTURE: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    &FIXTURE
        .get_or_init(|| {
            let dir = tempdir().unwrap();
            let executable = dir
                .path()
                .join(format!("command{}", std::env::consts::EXE_SUFFIX));
            let output = Command::new("rustc")
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/command.rs"))
                .arg("-o")
                .arg(&executable)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            (dir, executable)
        })
        .1
}

fn command(name: &str, args: &[&str]) -> Value {
    json!({"name": name, "program": fixture(), "args": args})
}

fn request(directory: &Path, commands: Vec<Value>) -> Value {
    json!({
        "schema": "conductor.request/v2",
        "working_directory": directory,
        "commands": commands
    })
}

fn run_value(value: Value) -> conductor::WorkflowResult {
    run(&serde_json::from_value::<WorkflowRequest>(value).unwrap())
}

fn cli_stdin(value: &Value) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_conductor"))
        .args(["run", "--request", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(value).unwrap())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn runs_arbitrary_commands_in_order_and_captures_diagnostics() {
    let dir = tempdir().unwrap();
    // Labels are opaque to Conductor, including the executor/review/commit labels.
    let labels = ["apply", "format", "compile", "test", "review", "commit"];
    let result = run_value(request(
        dir.path(),
        labels
            .iter()
            .map(|label| command(label, &["record", "journal", label, "0"]))
            .collect(),
    ));
    assert_eq!(result.status, Status::Passed);
    assert_eq!(result.phase, "completed");
    assert_eq!(result.failed_command_index, None);
    assert_eq!(
        fs::read_to_string(dir.path().join("journal")).unwrap(),
        labels.join("\n") + "\n"
    );
    for (index, record) in result.commands.iter().enumerate() {
        assert_eq!(record.index, index);
        assert_eq!(record.status, Status::Passed);
        assert_eq!(record.exit_code, Some(0));
        assert_eq!(
            record.stdout.trim_end(),
            format!("stdout: {}", labels[index])
        );
        assert_eq!(
            record.stderr.trim_end(),
            format!("stderr: {}", labels[index])
        );
    }
}

#[test]
fn failure_at_every_position_stops_later_side_effects_without_rollback() {
    for failed in 0..6 {
        let dir = tempdir().unwrap();
        let commands = (0..6)
            .map(|index| {
                command(
                    &format!("step-{index}"),
                    &[
                        "record",
                        "journal",
                        &index.to_string(),
                        if index == failed { "7" } else { "0" },
                    ],
                )
            })
            .collect();
        let result = run_value(request(dir.path(), commands));
        assert_eq!(result.status, Status::Failed);
        assert_eq!(result.phase, "commands");
        assert_eq!(result.failed_command_index, Some(failed));
        assert_eq!(result.commands.len(), failed + 1);
        assert_eq!(result.commands.last().unwrap().exit_code, Some(7));
        let expected = (0..=failed).map(|i| format!("{i}\n")).collect::<String>();
        assert_eq!(
            fs::read_to_string(dir.path().join("journal")).unwrap(),
            expected
        );
    }
}

#[test]
fn unavailable_command_is_recorded_and_stops_the_sequence() {
    let dir = tempdir().unwrap();
    let result = run_value(request(
        dir.path(),
        vec![
            command("first", &["record", "journal", "first", "0"]),
            json!({"name": "missing", "program": dir.path().join("does-not-exist")}),
            command("last", &["record", "journal", "last", "0"]),
        ],
    ));
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.failed_command_index, Some(1));
    assert_eq!(result.commands.len(), 2);
    assert_eq!(result.commands[1].exit_code, None);
    assert!(
        result.commands[1]
            .error
            .as_ref()
            .unwrap()
            .contains("missing")
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("journal")).unwrap(),
        "first\n"
    );
}

#[test]
fn invalid_later_command_prevents_any_execution() {
    let dir = tempdir().unwrap();
    for invalid in [
        json!({"name": " ", "program": "unused"}),
        json!({"name": "bad", "program": " "}),
        json!({"name": "bad", "program": "invalid\0program"}),
        json!({"name": "bad", "program": "unused", "args": ["invalid\0arg"]}),
    ] {
        let result = run_value(request(
            dir.path(),
            vec![
                command("first", &["record", "journal", "first", "0"]),
                invalid,
            ],
        ));
        assert_eq!(result.status, Status::Blocked);
        assert_eq!(result.phase, "input");
        assert!(result.commands.is_empty());
        assert!(!dir.path().join("journal").exists());
    }
}

#[test]
fn requires_an_existing_absolute_directory_but_not_a_git_repository() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("file"), "content").unwrap();
    for path in [
        PathBuf::from("relative"),
        dir.path().join("missing"),
        dir.path().join("file"),
    ] {
        let result = run_value(request(&path, vec![command("unused", &["stdin"])]));
        assert_eq!(result.status, Status::Blocked);
        assert_eq!(result.phase, "input");
    }
    let result = run_value(request(dir.path(), vec![]));
    assert_eq!(result.status, Status::Passed);
    assert!(result.commands.is_empty());
}

#[test]
fn argv_is_literal_and_explicit_relative_executables_use_working_directory() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("tools with spaces")).unwrap();
    let relative = format!(
        "./tools with spaces/command{}",
        std::env::consts::EXE_SUFFIX
    );
    fs::copy(fixture(), dir.path().join(&relative)).unwrap();
    let literal_args = [
        "echo",
        "a b",
        "$(touch injected)",
        "; touch injected",
        "*",
        "",
    ];
    let result = run_value(request(
        dir.path(),
        vec![json!({
            "name": "literal argv", "program": relative, "args": literal_args
        })],
    ));
    assert_eq!(result.status, Status::Passed);
    let stdout = result.commands[0].stdout.replace("\r\n", "\n");
    let (directory, arguments) = stdout.split_once('\n').unwrap();
    assert_eq!(
        Path::new(directory).canonicalize().unwrap(),
        dir.path().canonicalize().unwrap()
    );
    assert_eq!(arguments, literal_args[1..].join("\n") + "\n");
    assert!(!dir.path().join("injected").exists());
}

#[test]
fn bare_executables_use_path_and_child_stdin_is_closed() {
    let dir = tempdir().unwrap();
    let value = request(
        dir.path(),
        vec![json!({
            "name": "from PATH", "program": fixture().file_name().unwrap().to_str().unwrap(), "args": ["stdin"]
        })],
    );
    let file = dir.path().join("request.json");
    fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_conductor"))
        .args(["run", "--request"])
        .arg(file)
        .env("PATH", fixture().parent().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["commands"][0]["exit_code"], 0);
}

#[test]
fn output_is_lossy_utf8_bounded_and_explicitly_marked_when_truncated() {
    let dir = tempdir().unwrap();
    let result = run_value(request(dir.path(), vec![command("large", &["large"])]));
    assert_eq!(result.status, Status::Passed);
    let record = &result.commands[0];
    assert_eq!(record.stdout.chars().count(), 16_384);
    assert_eq!(record.stderr.chars().count(), 16_384);
    assert!(record.stdout.chars().all(|c| c == 'é'));
    assert!(record.stderr.chars().all(|c| c == '\u{fffd}'));
    assert!(record.stdout_truncated && record.stderr_truncated);
}

#[test]
fn cli_reports_generic_failure_for_all_nonzero_child_exit_codes() {
    let dir = tempdir().unwrap();
    for code in ["1", "2", "3", "42"] {
        let output = cli_stdin(&request(
            dir.path(),
            vec![
                command("review", &["record", "journal", code, code]),
                command("commit", &["record", "journal", "must-not-run", "0"]),
            ],
        ));
        assert_eq!(output.status.code(), Some(1));
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["schema"], "conductor.result/v2");
        assert_eq!(result["status"], "FAILED");
        assert_eq!(
            result["commands"][0]["exit_code"],
            code.parse::<i32>().unwrap()
        );
        assert_eq!(result["commands"].as_array().unwrap().len(), 1);
    }
    assert!(
        !fs::read_to_string(dir.path().join("journal"))
            .unwrap()
            .contains("must-not-run")
    );
}

#[test]
fn cli_rejects_legacy_and_unknown_fields_before_any_command_runs() {
    let dir = tempdir().unwrap();
    let valid = request(
        dir.path(),
        vec![command("unused", &["record", "journal", "unused", "0"])],
    );
    let mut invalid_requests = vec![];
    let mut legacy_schema = valid.clone();
    legacy_schema["schema"] = json!("conductor.request/v1");
    invalid_requests.push(legacy_schema);
    for field in [
        "execution",
        "checks",
        "review",
        "formatters",
        "repository_path",
    ] {
        let mut value = valid.clone();
        value[field] = json!({});
        invalid_requests.push(value);
    }
    let mut unknown_command_field = valid;
    unknown_command_field["commands"][0]["on_failure"] = json!("continue");
    invalid_requests.push(unknown_command_field);
    for value in invalid_requests {
        let output = cli_stdin(&value);
        assert_eq!(output.status.code(), Some(2));
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["status"], "BLOCKED");
        assert_eq!(result["phase"], "input");
        assert_eq!(result["commands"], json!([]));
    }
    assert!(!dir.path().join("journal").exists());
}

#[test]
fn cli_reads_request_files_and_returns_json_for_read_or_parse_errors() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("request.json");
    for (bytes, expected_code, expected_status) in [
        (
            serde_json::to_vec(&request(dir.path(), vec![])).unwrap(),
            0,
            "PASSED",
        ),
        (b"invalid JSON".to_vec(), 2, "BLOCKED"),
    ] {
        fs::write(&file, bytes).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_conductor"))
            .args(["run", "--request"])
            .arg(&file)
            .output()
            .unwrap();
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["status"], expected_status);
        assert_eq!(output.status.code(), Some(expected_code));
        if expected_code != 0 {
            assert_eq!(result["phase"], "input");
        }
    }
    fs::remove_file(&file).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_conductor"))
        .args(["run", "--request"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("Cannot read request")
    );
}

#[cfg(unix)]
#[test]
fn signal_termination_is_failure_and_skips_later_commands() {
    let dir = tempdir().unwrap();
    let result = run_value(request(
        dir.path(),
        vec![
            json!({"name": "signal", "program": "/bin/sh", "args": ["-c", "kill -TERM $$"]}),
            command("later", &["record", "journal", "must-not-run", "0"]),
        ],
    ));
    assert_eq!(result.status, Status::Failed);
    assert_eq!(result.commands.len(), 1);
    assert_eq!(result.commands[0].exit_code, None);
    assert!(!dir.path().join("journal").exists());
}
