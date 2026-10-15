//! Formatting assumes the repository root and its ancestors remain stable.
//! Guards below that resolved root detect unsafe, missing or changed targets.

use crate::{Status, bounded};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, Metadata, Permissions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use tempfile::Builder;

/// An explicit stdin/stdout formatter for a non-overlapping set of extensions.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Formatter {
    pub name: String,
    pub extensions: Vec<String>,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// One attempted file. Stdout is source data, so it is not duplicated here.
#[derive(Serialize)]
pub struct FormatterResult {
    pub name: String,
    pub path: String,
    pub program: String,
    pub args: Vec<String>,
    pub exit_code: Option<i32>,
    pub changed: bool,
    pub stderr: String,
}

pub(crate) struct Failure {
    pub status: Status,
    pub error: String,
}

impl Failure {
    fn blocked(error: impl Into<String>) -> Self {
        Self {
            status: Status::Blocked,
            error: error.into(),
        }
    }

    fn needs_design(error: impl Into<String>) -> Self {
        Self {
            status: Status::NeedsDesign,
            error: error.into(),
        }
    }
}

pub(crate) fn validate(rules: &[Formatter]) -> Result<(), String> {
    let mut extensions = HashSet::new();
    for rule in rules {
        if rule.name.trim().is_empty() || rule.program.trim().is_empty() {
            return Err("Formatter name and program must not be empty".into());
        }
        if rule.extensions.is_empty() {
            return Err(format!("Formatter '{}' must have extensions", rule.name));
        }
        for extension in &rule.extensions {
            if extension.is_empty()
                || extension.contains(['.', '/', char::from(92), ':'])
                || extension.chars().any(char::is_whitespace)
            {
                return Err(format!("Invalid formatter extension '{extension}'"));
            }
            if !extensions.insert(extension.to_lowercase()) {
                return Err(format!("Duplicate formatter extension '{extension}'"));
            }
        }
    }
    Ok(())
}

pub(crate) fn run(
    repository: &Path,
    rules: &[Formatter],
    files: &[String],
    results: &mut Vec<FormatterResult>,
) -> Result<(), Failure> {
    if rules.is_empty() || files.is_empty() {
        return Ok(());
    }
    let root = fs::canonicalize(repository)
        .map_err(|error| Failure::blocked(format!("Cannot resolve formatting root: {error}")))?;
    let mut files = files.to_vec();
    files.sort();
    files.dedup();
    for rule in rules {
        for path in &files {
            let matches = Path::new(path)
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|extension| {
                    rule.extensions
                        .iter()
                        .any(|wanted| wanted.to_lowercase() == extension.to_lowercase())
                });
            if !matches {
                continue;
            }
            let parent = Path::new(path)
                .parent()
                .filter(|value| !value.as_os_str().is_empty())
                .and_then(|value| value.to_str())
                .unwrap_or(".");
            let args = rule
                .args
                .iter()
                .map(|arg| match arg.as_str() {
                    "{path}" => path.clone(),
                    "{parent}" => parent.to_owned(),
                    _ => arg.clone(),
                })
                .collect();
            let mut result = FormatterResult {
                name: rule.name.clone(),
                path: path.clone(),
                program: rule.program.clone(),
                args,
                exit_code: None,
                changed: false,
                stderr: String::new(),
            };
            let outcome = format_file(&root, rule, &mut result);
            results.push(result);
            outcome.map_err(|failure| Failure {
                status: failure.status,
                error: format!("Formatter '{}' for '{}': {}", rule.name, path, failure.error),
            })?;
        }
    }
    Ok(())
}

fn format_file(root: &Path, rule: &Formatter, result: &mut FormatterResult) -> Result<(), Failure> {
    let target = checked_target(root, &result.path).map_err(Failure::blocked)?;
    let original = fs::read(&target)
        .map_err(|error| Failure::blocked(format!("Cannot read target: {error}")))?;
    std::str::from_utf8(&original)
        .map_err(|error| Failure::blocked(format!("Target is not UTF-8: {error}")))?;
    let permissions = fs::metadata(&target)
        .map_err(|error| Failure::blocked(format!("Cannot inspect permissions: {error}")))?
        .permissions();
    let (output, written) = transform(root, rule, &result.args, &original)
        .map_err(|error| Failure::blocked(format!("Cannot run formatter: {error}")))?;
    result.exit_code = output.status.code();
    result.stderr = bounded(&output.stderr);
    if !output.status.success() {
        return Err(Failure::needs_design("Formatter exited unsuccessfully"));
    }
    std::str::from_utf8(&output.stdout)
        .map_err(|error| Failure::needs_design(format!("Formatter stdout is not UTF-8: {error}")))?;
    if output.stdout.is_empty() && !original.is_empty() {
        return Err(Failure::needs_design(
            "Formatter stdout is empty for nonempty input",
        ));
    }
    written.map_err(|error| Failure::blocked(format!("Cannot supply formatter stdin: {error}")))?;
    result.changed = replace_if_unchanged(root, &result.path, &original, &output.stdout, permissions)
        .map_err(Failure::blocked)?;
    Ok(())
}

// Drain stdout/stderr concurrently with stdin writes to avoid pipe deadlocks.
fn transform(
    root: &Path,
    rule: &Formatter,
    args: &[String],
    source: &[u8],
) -> io::Result<(Output, io::Result<()>)> {
    let program = Path::new(&rule.program);
    let program = if program.is_relative() && program.components().count() > 1 {
        root.join(program)
    } else {
        program.to_path_buf()
    };
    let mut child = Command::new(program)
        .args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped formatter stdin");
    thread::scope(|scope| {
        let writer = match thread::Builder::new().spawn_scoped(scope, move || stdin.write_all(source)) {
            Ok(writer) => writer,
            Err(error) => {
                if let Err(cleanup) = child.kill() {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("Stdin writer creation failed: {error}; child termination failed: {cleanup}"),
                    ));
                }
                if let Err(cleanup) = child.wait() {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("Stdin writer creation failed: {error}; child reaping failed: {cleanup}"),
                    ));
                }
                return Err(error);
            }
        };
        let output = child.wait_with_output();
        let written = writer
            .join()
            .map_err(|_| io::Error::other("Formatter stdin writer panicked"))?;
        Ok((output?, written))
    })
}

fn replace_if_unchanged(
    root: &Path,
    relative: &str,
    original: &[u8],
    formatted: &[u8],
    permissions: Permissions,
) -> Result<bool, String> {
    let target = checked_target(root, relative)?;
    let current = fs::read(&target).map_err(|error| format!("Cannot reread target: {error}"))?;
    if current != original {
        return Err("Target contents changed during formatting".into());
    }
    if original == formatted {
        return Ok(false);
    }
    let parent = target.parent().ok_or("Target has no parent directory")?;
    let mut temporary = Builder::new()
        .prefix(".conductor-format-")
        .tempfile_in(parent)
        .map_err(|error| format!("Cannot create temporary sibling: {error}"))?;
    temporary
        .write_all(formatted)
        .map_err(|error| format!("Cannot write formatted source: {error}"))?;
    temporary
        .flush()
        .map_err(|error| format!("Cannot flush formatted source: {error}"))?;
    fs::set_permissions(temporary.path(), permissions)
        .map_err(|error| format!("Cannot preserve target permissions: {error}"))?;
    // Check again immediately before replacement; this is optimistic, not locking.
    let target = checked_target(root, relative)?;
    let current = fs::read(&target).map_err(|error| format!("Cannot reread target: {error}"))?;
    if current != original {
        return Err("Target contents changed during formatting".into());
    }
    temporary
        .persist(&target)
        .map_err(|error| format!("Cannot replace target: {}", error.error))?;
    Ok(true)
}

fn checked_target(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let unsafe_path = || format!("Unsafe formatting target '{relative}'");
    if relative.is_empty() || relative.contains(char::from(92)) {
        return Err(unsafe_path());
    }
    let mut target = root.to_path_buf();
    let mut components = Path::new(relative).components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(unsafe_path());
        };
        let name_text = name.to_string_lossy();
        if name_text.contains(':')
            || name_text.ends_with(['.', ' '])
            || name_text.eq_ignore_ascii_case(".git")
            || name_text.eq_ignore_ascii_case("git~1")
        {
            return Err(unsafe_path());
        }
        target.push(name);
        let metadata = fs::symlink_metadata(&target)
            .map_err(|error| format!("Cannot inspect formatting target '{relative}': {error}"))?;
        if is_link(&metadata)
            || (components.peek().is_some() && !metadata.is_dir())
            || (components.peek().is_none() && !metadata.is_file())
        {
            return Err(unsafe_path());
        }
    }
    if target == root {
        return Err(unsafe_path());
    }
    Ok(target)
}

fn is_link(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_checks_reject_traversal_metadata_and_non_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file.rs"), "source").unwrap();
        fs::create_dir(dir.path().join("directory")).unwrap();
        assert!(checked_target(dir.path(), "file.rs").is_ok());
        for target in [
            "",
            ".",
            "../file.rs",
            "/file.rs",
            ".git/config",
            "git~1/config",
            "directory",
            "missing.rs",
            "file.rs/child.rs",
            "file.rs:stream",
        ] {
            assert!(checked_target(dir.path(), target).is_err(), "{target}");
        }
    }

    #[test]
    fn unchanged_output_still_checks_for_target_changes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.rs");
        fs::write(&target, "original").unwrap();
        let permissions = fs::metadata(&target).unwrap().permissions();
        fs::write(&target, "external change").unwrap();
        let error = replace_if_unchanged(
            dir.path(),
            "file.rs",
            b"original",
            b"original",
            permissions,
        )
        .unwrap_err();
        assert!(error.contains("changed"));
        assert_eq!(fs::read_to_string(target).unwrap(), "external change");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn replacement_does_not_write_through_hard_links() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.rs");
        fs::write(&target, "original").unwrap();
        fs::hard_link(&target, dir.path().join("unrelated.rs")).unwrap();
        let permissions = fs::metadata(&target).unwrap().permissions();
        assert!(
            replace_if_unchanged(
                dir.path(),
                "file.rs",
                b"original",
                b"formatted",
                permissions
            )
            .unwrap()
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "formatted");
        assert_eq!(
            fs::read_to_string(dir.path().join("unrelated.rs")).unwrap(),
            "original"
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn target_checks_reject_symlink_targets_and_ancestors() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("file.rs"), "outside").unwrap();
        symlink(outside.path(), dir.path().join("linked-directory")).unwrap();
        symlink(
            outside.path().join("file.rs"),
            dir.path().join("linked.rs"),
        )
        .unwrap();
        assert!(checked_target(dir.path(), "linked.rs").is_err());
        assert!(checked_target(dir.path(), "linked-directory/file.rs").is_err());
    }
}
