//! Run caller-supplied commands in order, stopping at the first failure.
//! Conductor assigns no meaning to command names or output and has no knowledge
//! of source changes, verification, review, Git, or task lifecycle policy.

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Result as IoResult};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MAX_OUTPUT_CHARS: usize = 16_384;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRequest {
    pub schema: String,
    pub working_directory: PathBuf,
    /// Existing absolute parent for per-run logs. Defaults to the system temp directory.
    #[serde(default)]
    pub output_directory: Option<PathBuf>,
    pub commands: Vec<CommandSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub name: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional external evaluator. Its exit status, rather than the command's,
    /// determines whether the sequence continues.
    #[serde(default)]
    pub success_condition: Option<ConditionSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Clone, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Passed,
    Failed,
    Blocked,
}

impl Status {
    /// Runner exit codes, independent of any child command's exit code.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Passed => 0,
            Self::Failed => 1,
            Self::Blocked => 2,
        }
    }
}

#[derive(Serialize)]
pub struct CommandResult {
    pub index: usize,
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub status: Status,
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<ConditionResult>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct ConditionResult {
    pub program: String,
    pub args: Vec<String>,
    pub status: Status,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct WorkflowResult {
    pub schema: &'static str,
    pub status: Status,
    pub phase: &'static str,
    pub commands: Vec<CommandResult>,
    /// Unique run directory, retained after return for caller-managed cleanup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_directory: Option<PathBuf>,
    /// Zero-based index of the command that failed or could not be started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_command_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WorkflowResult {
    pub fn blocked(phase: &'static str, error: impl Into<String>) -> Self {
        Self {
            schema: "conductor.result/v2",
            status: Status::Blocked,
            phase,
            commands: Vec::new(),
            output_directory: None,
            failed_command_index: None,
            error: Some(error.into()),
        }
    }
}

/// Validate the whole request before starting any command. An empty list is a
/// successful no-op. Commands receive closed stdin and run without a shell.
pub fn run(request: &WorkflowRequest) -> WorkflowResult {
    let directory = match validate(request) {
        Ok(directory) => directory,
        Err(error) => return WorkflowResult::blocked("input", error),
    };
    let mut output = WorkflowResult {
        schema: "conductor.result/v2",
        status: Status::Passed,
        phase: "completed",
        commands: Vec::new(),
        output_directory: None,
        failed_command_index: None,
        error: None,
    };
    if request.commands.is_empty() {
        return output;
    }
    let logs = match create_run_directory(request.output_directory.as_deref()) {
        Ok(logs) => logs,
        Err(error) => return WorkflowResult::blocked("output", error),
    };
    output.output_directory = Some(logs.clone());
    for (index, command) in request.commands.iter().enumerate() {
        let mut record = CommandResult {
            index,
            name: command.name.clone(),
            program: command.program.clone(),
            args: command.args.clone(),
            status: Status::Blocked,
            exit_code: None,
            condition: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_path: None,
            stderr_path: None,
            error: None,
        };
        if let Err(error) = execute_command(&directory, &logs, command, &mut record) {
            record.status = Status::Blocked;
            record.error = Some(match record.error.take() {
                Some(previous) => format!("{previous}; {error}"),
                None => error,
            });
        }
        let succeeded = record.status == Status::Passed;
        if !succeeded {
            output.status = if record.status == Status::Failed {
                Status::Failed
            } else {
                Status::Blocked
            };
            output.phase = "commands";
            output.failed_command_index = Some(index);
            output.error = record.error.clone();
        }
        output.commands.push(record);
        if !succeeded {
            break;
        }
    }
    output
}

fn validate(request: &WorkflowRequest) -> Result<PathBuf, String> {
    if request.schema != "conductor.request/v2" {
        return Err("Unsupported request schema; expected conductor.request/v2".into());
    }
    if !request.working_directory.is_absolute() {
        return Err("working_directory must be an absolute directory path".into());
    }
    let directory = request
        .working_directory
        .canonicalize()
        .map_err(|error| format!("Cannot resolve working_directory: {error}"))?;
    if !directory.is_dir() {
        return Err("working_directory must be a directory".into());
    }
    if let Some(parent) = &request.output_directory
        && (!parent.is_absolute() || !parent.is_dir())
    {
        return Err("output_directory must be an existing absolute directory".into());
    }
    for (index, command) in request.commands.iter().enumerate() {
        if command.name.trim().is_empty() || command.program.trim().is_empty() {
            return Err(format!(
                "Command {index}: name and program must not be empty"
            ));
        }
        if command.program.contains('\0') || command.args.iter().any(|arg| arg.contains('\0')) {
            return Err(format!(
                "Command {index}: program and args must not contain NUL"
            ));
        }
        if let Some(condition) = &command.success_condition
            && (condition.program.trim().is_empty()
                || condition.program.contains('\0')
                || condition.args.iter().any(|arg| arg.contains('\0')))
        {
            return Err(format!(
                "Command {index}: invalid success_condition program or args"
            ));
        }
    }
    Ok(directory)
}

fn resolve_program(directory: &Path, program: &str) -> PathBuf {
    let path = Path::new(program);
    if !path.is_absolute() && path.components().count() > 1 {
        directory.join(path)
    } else {
        path.into()
    }
}

fn create_run_directory(parent: Option<&Path>) -> Result<PathBuf, String> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("conductor-");
    let dir = match parent {
        Some(parent) => builder.tempdir_in(parent),
        None => builder.tempdir(),
    }
    .map_err(|error| format!("Cannot create command output directory: {error}"))?;
    let path = dir
        .path()
        .canonicalize()
        .map_err(|error| format!("Cannot resolve command output directory: {error}"))?;
    // The caller owns retention/cleanup; do not delete logs when the run ends.
    let _ = dir.keep();
    Ok(path)
}

fn execute_command(
    directory: &Path,
    logs: &Path,
    command: &CommandSpec,
    record: &mut CommandResult,
) -> Result<(), String> {
    let stdout_path = logs.join(format!("{:06}.stdout", record.index));
    let stderr_path = logs.join(format!("{:06}.stderr", record.index));
    let stdout = File::create_new(&stdout_path).map_err(|error| {
        format!(
            "Cannot create stdout log {}: {error}",
            stdout_path.display()
        )
    })?;
    record.stdout_path = Some(stdout_path.clone());
    let stderr = File::create_new(&stderr_path).map_err(|error| {
        format!(
            "Cannot create stderr log {}: {error}",
            stderr_path.display()
        )
    })?;
    record.stderr_path = Some(stderr_path.clone());

    // Redirect to files, not pipes: output size must not determine memory use.
    // Resolve explicit relative programs before current_dir for portable behaviour.
    let result = Command::new(resolve_program(directory, &command.program))
        .args(&command.args)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .status();
    match result {
        Ok(status) => {
            record.exit_code = status.code();
            record.status = if status.success() {
                Status::Passed
            } else {
                Status::Failed
            };
            if !status.success() {
                record.error = Some(format!("Command '{}' failed ({status})", command.name));
            }
        }
        Err(error) => {
            record.error = Some(format!("Cannot run command '{}': {error}", command.name));
        }
    }
    (record.stdout, record.stdout_truncated) = preview(&stdout_path)
        .map_err(|error| format!("Cannot read stdout log {}: {error}", stdout_path.display()))?;
    (record.stderr, record.stderr_truncated) = preview(&stderr_path)
        .map_err(|error| format!("Cannot read stderr log {}: {error}", stderr_path.display()))?;
    if let Some(condition) = &command.success_condition
        && record.exit_code.is_some()
    {
        let evaluated = evaluate_condition(
            directory,
            logs,
            condition,
            record,
            &stdout_path,
            &stderr_path,
        );
        record.status = evaluated.status.clone();
        record.error = evaluated.error.clone();
        record.condition = Some(evaluated);
    }
    Ok(())
}

fn evaluate_condition(
    directory: &Path,
    logs: &Path,
    spec: &ConditionSpec,
    command: &CommandResult,
    stdout_path: &Path,
    stderr_path: &Path,
) -> ConditionResult {
    let mut result = ConditionResult {
        program: spec.program.clone(),
        args: spec.args.clone(),
        status: Status::Blocked,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        stdout_path: None,
        stderr_path: None,
        error: None,
    };
    let evaluated = (|| -> Result<(), String> {
        let out_path = logs.join(format!("{:06}.condition.stdout", command.index));
        let err_path = logs.join(format!("{:06}.condition.stderr", command.index));
        let out = File::create_new(&out_path)
            .map_err(|error| format!("Cannot create condition stdout log: {error}"))?;
        result.stdout_path = Some(out_path.clone());
        let err = File::create_new(&err_path)
            .map_err(|error| format!("Cannot create condition stderr log: {error}"))?;
        result.stderr_path = Some(err_path.clone());
        let status = Command::new(resolve_program(directory, &spec.program))
            .args(&spec.args)
            .current_dir(directory)
            .env(
                "CONDUCTOR_COMMAND_EXIT_CODE",
                command.exit_code.unwrap().to_string(),
            )
            .env("CONDUCTOR_COMMAND_STDOUT_PATH", stdout_path)
            .env("CONDUCTOR_COMMAND_STDERR_PATH", stderr_path)
            .env("CONDUCTOR_COMMAND_INDEX", command.index.to_string())
            .env("CONDUCTOR_COMMAND_NAME", &command.name)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .status()
            .map_err(|error| format!("Cannot run success condition: {error}"))?;
        result.exit_code = status.code();
        result.status = if status.success() {
            Status::Passed
        } else {
            Status::Failed
        };
        if !status.success() {
            result.error = Some(format!(
                "Success condition for '{}' failed ({status})",
                command.name
            ));
        }
        (result.stdout, result.stdout_truncated) = preview(&out_path)
            .map_err(|error| format!("Cannot read condition stdout log: {error}"))?;
        (result.stderr, result.stderr_truncated) = preview(&err_path)
            .map_err(|error| format!("Cannot read condition stderr log: {error}"))?;
        Ok(())
    })();
    if let Err(error) = evaluated {
        result.status = Status::Blocked;
        result.error = Some(error);
    }
    result
}

fn preview(path: &Path) -> IoResult<(String, bool)> {
    // A Unicode scalar occupies at most four UTF-8 bytes. Reading this much
    // guarantees enough decoded characters to decide truncation, even for
    // four-byte scalars, while preserving bounded memory for arbitrary logs.
    let mut bytes = Vec::new();
    File::open(path)?
        .take((4 * (MAX_OUTPUT_CHARS + 1)) as u64)
        .read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut chars = text.chars();
    let output = chars.by_ref().take(MAX_OUTPUT_CHARS).collect();
    Ok((output, chars.next().is_some()))
}
