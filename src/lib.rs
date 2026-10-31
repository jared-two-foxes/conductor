//! Run caller-supplied commands in order, stopping at the first failure.
//! Conductor assigns no meaning to command names or output and has no knowledge
//! of source changes, verification, review, Git, or task lifecycle policy.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_OUTPUT_CHARS: usize = 16_384;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRequest {
    pub schema: String,
    pub working_directory: PathBuf,
    pub commands: Vec<CommandSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub name: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Serialize, PartialEq, Eq, Debug)]
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
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct WorkflowResult {
    pub schema: &'static str,
    pub status: Status,
    pub phase: &'static str,
    pub commands: Vec<CommandResult>,
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
        failed_command_index: None,
        error: None,
    };
    for (index, command) in request.commands.iter().enumerate() {
        // Bare names use PATH. Explicit relative executable paths are resolved
        // before setting current_dir, avoiding platform-dependent resolution.
        let program = resolve_program(&directory, &command.program);
        let result = Command::new(program)
            .args(&command.args)
            .current_dir(&directory)
            .output();
        let mut record = CommandResult {
            index,
            name: command.name.clone(),
            program: command.program.clone(),
            args: command.args.clone(),
            status: Status::Blocked,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            error: None,
        };
        match result {
            Ok(result) => {
                record.exit_code = result.status.code();
                (record.stdout, record.stdout_truncated) = bounded(&result.stdout);
                (record.stderr, record.stderr_truncated) = bounded(&result.stderr);
                record.status = if result.status.success() {
                    Status::Passed
                } else {
                    record.error = Some(format!(
                        "Command '{}' failed ({})",
                        command.name, result.status
                    ));
                    Status::Failed
                };
            }
            Err(error) => {
                record.error = Some(format!("Cannot run command '{}': {error}", command.name));
            }
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

fn bounded(bytes: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars();
    let output = chars.by_ref().take(MAX_OUTPUT_CHARS).collect();
    (output, chars.next().is_some())
}
