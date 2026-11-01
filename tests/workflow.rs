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

fn conditioned(name: &str, args: &[&str], mode: &str, value: &str) -> Value {
    json!({"name": name, "program": fixture(), "args": args,
        "success_condition": {"program": fixture(), "args": ["condition", name, "0", mode, value]}})
}

#[test]
fn condition_can_reject_zero_exit_using_complete_output() {
    let dir = tempdir().unwrap();
    let first = conditioned(
        "cargo test",
        &["record", "journal", "test failure", "0"],
        "output-excludes",
        "failure",
    );
    let result = run_value(request(
        dir.path(),
        vec![
            first,
            command("skipped", &["record", "journal", "next", "0"]),
        ],
    ));
    assert_eq!(result.status, Status::Failed);
    assert_eq!(result.failed_command_index, Some(0));
    assert_eq!(result.commands.len(), 1);
    let command = &result.commands[0];
    assert_eq!(command.exit_code, Some(0));
    assert_eq!(command.status, Status::Failed);
    let condition = command.condition.as_ref().unwrap();
    assert_eq!(condition.status, Status::Failed);
    assert_eq!(condition.exit_code, Some(7));
    assert!(
        fs::read_to_string(condition.stderr_path.as_ref().unwrap())
            .unwrap()
            .contains("rejected")
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("journal")).unwrap(),
        "test failure\n"
    );
}

#[test]
fn condition_can_accept_nonzero_exit_and_continue_using_state() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("new-directory");
    let first = conditioned(
        "search",
        &["record", "journal", "directory missing", "1"],
        "directory-absent",
        target.to_str().unwrap(),
    );
    let second = command("create", &["create-directory", target.to_str().unwrap()]);
    let result = run_value(request(dir.path(), vec![first, second]));
    assert_eq!(result.status, Status::Passed);
    assert_eq!(result.commands[0].exit_code, Some(1));
    assert_eq!(result.commands[0].status, Status::Passed);
    assert_eq!(
        result.commands[0].condition.as_ref().unwrap().exit_code,
        Some(0)
    );
    assert!(target.is_dir());
}

#[test]
fn condition_reads_complete_output_beyond_the_result_preview() {
    let dir = tempdir().unwrap();
    let first = conditioned(
        "report",
        &["report"],
        "output-contains",
        "failure details at end",
    );
    let result = run_value(request(
        dir.path(),
        vec![
            first,
            command("next", &["record", "journal", "continued", "0"]),
        ],
    ));
    assert_eq!(result.status, Status::Passed);
    assert_eq!(result.commands[0].exit_code, Some(2));
    assert!(result.commands[0].stdout_truncated);
    assert!(!result.commands[0].stdout.contains("failure details at end"));
    assert_eq!(
        result.commands[0].condition.as_ref().unwrap().status,
        Status::Passed
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("journal")).unwrap(),
        "continued\n"
    );
}

#[test]
fn condition_launch_failure_blocks_and_does_not_continue() {
    let dir = tempdir().unwrap();
    let mut first = command("check", &["record", "journal", "done", "0"]);
    first["success_condition"] = json!({"program": "missing-conductor-condition-executable"});
    let result = run_value(request(
        dir.path(),
        vec![
            first,
            command("skipped", &["record", "journal", "next", "0"]),
        ],
    ));
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.commands[0].exit_code, Some(0));
    assert_eq!(
        result.commands[0].condition.as_ref().unwrap().status,
        Status::Blocked
    );
    assert_eq!(result.commands.len(), 1);
}

fn request(directory: &Path, commands: Vec<Value>) -> Value {
    json!({
        "schema": "conductor.request/v2",
        "working_directory": directory,
        "output_directory": directory,
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
            fs::read(record.stdout_path.as_ref().unwrap()).unwrap(),
            record.stdout.as_bytes()
        );
        assert_eq!(
            fs::read(record.stderr_path.as_ref().unwrap()).unwrap(),
            record.stderr.as_bytes()
        );
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
    for path in [
        &result.commands[1].stdout_path,
        &result.commands[1].stderr_path,
    ] {
        assert!(fs::read(path.as_ref().unwrap()).unwrap().is_empty());
    }
    assert!(
        !result
            .output_directory
            .as_ref()
            .unwrap()
            .join("000002.stdout")
            .exists()
    );
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
    assert!(result.output_directory.is_none());
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
    assert_eq!(
        fs::read(record.stdout_path.as_ref().unwrap()).unwrap(),
        "é".repeat(17_000).as_bytes()
    );
    assert_eq!(
        fs::read(record.stderr_path.as_ref().unwrap()).unwrap(),
        vec![0xff; 17_000]
    );
}

#[test]
fn cli_retains_large_structured_output_and_failure_tail_after_exit() {
    let dir = tempdir().unwrap();
    let output = cli_stdin(&request(
        dir.path(),
        vec![
            command("review", &["report"]),
            command("commit", &["record", "journal", "must-not-run", "0"]),
        ],
    ));
    assert_eq!(output.status.code(), Some(1));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "FAILED");
    assert_eq!(result["failed_command_index"], 0);
    let records = result["commands"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record["exit_code"], 2);
    assert_eq!(record["stdout_truncated"], true);
    assert_eq!(record["stderr_truncated"], true);
    assert!(!record["stderr"].as_str().unwrap().contains("FAILURE"));
    let stdout = Path::new(record["stdout_path"].as_str().unwrap());
    let stderr = Path::new(record["stderr_path"].as_str().unwrap());
    assert!(stdout.is_absolute() && stderr.is_absolute());
    // Read only after the CLI process has exited: full JSON remains parseable.
    let report: Value = serde_json::from_slice(&fs::read(stdout).unwrap()).unwrap();
    assert_eq!(report["padding"].as_str().unwrap().len(), 100_000);
    assert_eq!(report["findings"][0]["message"], "failure details at end");
    assert!(
        fs::read_to_string(stderr)
            .unwrap()
            .ends_with("FAILURE: expected 42, got 7\n")
    );
    assert!(!dir.path().join("journal").exists());
    assert!(!stdout.parent().unwrap().join("000001.stdout").exists());
}

#[test]
fn previews_respect_character_boundaries_and_detect_exact_limit() {
    let dir = tempdir().unwrap();
    for character in ["a", "é", "😀"] {
        for count in [16_383, 16_384, 16_385] {
            let result = run_value(request(
                dir.path(),
                vec![command(
                    "repeat",
                    &["repeat", character, &count.to_string()],
                )],
            ));
            assert_eq!(result.status, Status::Passed);
            let record = &result.commands[0];
            assert_eq!(record.stdout, character.repeat(count.min(16_384)));
            assert_eq!(record.stdout_truncated, count > 16_384);
            assert_eq!(
                fs::read(record.stdout_path.as_ref().unwrap()).unwrap(),
                character.repeat(count).as_bytes()
            );
            assert!(record.stderr.is_empty() && !record.stderr_truncated);
        }
    }
}

#[test]
fn output_directories_are_unique_and_names_do_not_control_paths() {
    let dir = tempdir().unwrap();
    let logs = tempdir().unwrap();
    let mut value = request(
        dir.path(),
        vec![
            command("../same", &["echo", "first"]),
            command("../same", &["echo", "second"]),
        ],
    );
    value["output_directory"] = json!(logs.path());
    let first = run_value(value.clone());
    let second = run_value(value);
    assert_eq!(first.status, Status::Passed);
    assert_eq!(second.status, Status::Passed);
    assert_ne!(first.output_directory, second.output_directory);
    for result in [&first, &second] {
        let run_dir = result.output_directory.as_ref().unwrap();
        assert!(run_dir.starts_with(logs.path().canonicalize().unwrap()));
        assert_eq!(fs::read_dir(run_dir).unwrap().count(), 4);
        for (index, record) in result.commands.iter().enumerate() {
            assert_eq!(
                record.stdout_path.as_ref().unwrap(),
                &run_dir.join(format!("{index:06}.stdout"))
            );
            assert_eq!(
                record.stderr_path.as_ref().unwrap(),
                &run_dir.join(format!("{index:06}.stderr"))
            );
        }
    }
    assert!(
        fs::read_to_string(first.commands[0].stdout_path.as_ref().unwrap())
            .unwrap()
            .ends_with("first\n")
    );
}

#[test]
fn omitted_output_directory_keeps_logs_outside_the_working_directory() {
    let dir = tempdir().unwrap();
    let mut value = request(dir.path(), vec![command("empty", &["stdin"])]);
    value.as_object_mut().unwrap().remove("output_directory");
    let result = run_value(value);
    assert_eq!(result.status, Status::Passed);
    let logs = result.output_directory.unwrap();
    assert!(logs.is_absolute() && logs.is_dir());
    assert!(!logs.starts_with(dir.path().canonicalize().unwrap()));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    fs::remove_dir_all(logs).unwrap();
}

#[test]
fn invalid_output_directory_is_rejected_without_side_effects() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("file"), "content").unwrap();
    for parent in [
        PathBuf::from("relative"),
        dir.path().join("missing"),
        dir.path().join("file"),
    ] {
        let mut value = request(
            dir.path(),
            vec![command("unused", &["record", "journal", "unused", "0"])],
        );
        value["output_directory"] = json!(parent);
        let result = run_value(value);
        assert_eq!(result.status, Status::Blocked);
        assert_eq!(result.phase, "input");
        assert!(result.commands.is_empty() && result.output_directory.is_none());
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn unavailable_default_log_directory_blocks_before_any_command_runs() {
    let dir = tempdir().unwrap();
    let request_file = dir.path().join("request.json");
    let mut value = request(
        dir.path(),
        vec![command("unused", &["record", "journal", "unused", "0"])],
    );
    value.as_object_mut().unwrap().remove("output_directory");
    fs::write(&request_file, serde_json::to_vec(&value).unwrap()).unwrap();
    let missing = dir.path().join("missing");
    let output = Command::new(env!("CARGO_BIN_EXE_conductor"))
        .args(["run", "--request"])
        .arg(request_file)
        .env("TMPDIR", &missing)
        .env("TMP", &missing)
        .env("TEMP", &missing)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "BLOCKED");
    assert_eq!(result["phase"], "output");
    assert_eq!(result["commands"], json!([]));
    assert!(!dir.path().join("journal").exists());
}

#[test]
fn log_file_creation_failure_retains_prior_results_and_never_overwrites() {
    let dir = tempdir().unwrap();
    let logs = tempdir().unwrap();
    let mut value = request(
        dir.path(),
        vec![
            command("first", &["block-next-log", logs.path().to_str().unwrap()]),
            command("blocked", &["record", "journal", "must-not-run", "0"]),
            command("skipped", &["record", "journal", "must-not-run", "0"]),
        ],
    );
    value["output_directory"] = json!(logs.path());
    let result = run_value(value);
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.phase, "commands");
    assert_eq!(result.failed_command_index, Some(1));
    assert_eq!(result.commands.len(), 2);
    assert_eq!(result.commands[0].status, Status::Passed);
    assert_eq!(result.commands[1].exit_code, None);
    assert!(result.commands[1].stdout_path.as_ref().unwrap().is_file());
    assert!(result.commands[1].stderr_path.is_none());
    assert!(result.error.unwrap().contains("Cannot create stderr log"));
    assert_eq!(
        fs::read_to_string(result.output_directory.unwrap().join("000001.stderr")).unwrap(),
        "do not overwrite"
    );
    assert!(!dir.path().join("journal").exists());
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
    for condition in [
        json!({"program": ""}),
        json!({"program": "ok\u{0}bad"}),
        json!({"program": "ok", "args": ["bad\u{0}arg"]}),
        json!({"program": "ok", "unknown": true}),
    ] {
        let mut value = request(
            dir.path(),
            vec![command("unused", &["record", "journal", "unused", "0"])],
        );
        value["commands"][0]["success_condition"] = condition;
        invalid_requests.push(value);
    }
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
fn unreadable_log_stops_the_sequence_even_if_the_child_succeeded() {
    let dir = tempdir().unwrap();
    let logs = tempdir().unwrap();
    let mut value = request(
        dir.path(),
        vec![
            command(
                "remove-log",
                &["remove-current-stdout", logs.path().to_str().unwrap()],
            ),
            command("skipped", &["record", "journal", "must-not-run", "0"]),
        ],
    );
    value["output_directory"] = json!(logs.path());
    let result = run_value(value);
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.failed_command_index, Some(0));
    assert_eq!(result.commands.len(), 1);
    assert_eq!(result.commands[0].exit_code, Some(0));
    assert_eq!(result.commands[0].status, Status::Blocked);
    assert!(result.error.unwrap().contains("Cannot read stdout log"));
    assert!(result.commands[0].stderr_path.as_ref().unwrap().is_file());
    assert!(!dir.path().join("journal").exists());
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
