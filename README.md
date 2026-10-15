# conductor

A deterministic Rust coordinator for a code-writing Design agent. It consumes the **exact executor input authored by Design**, applies operations through the `executor` library, optionally formats affected files, runs verification commands, then invokes the `review-app` library. Conductor does not ask a model to interpret executor input, retry changes, or make design decisions.

## Contract

`conductor run --request workflow.json` or `conductor run --request -` (stdin).

Example `workflow.json`:

```json
{
  "schema": "conductor.request/v1",
  "repository_path": "/absolute/path/to/project",
  "execution": {
    "operations": [
      {"type": "create_file", "path": "example.txt", "content": "hello\n"},
      {"type": "edit_file", "path": "example.txt", "edits": [{"oldText": "hello", "newText": "Hello"}]}
    ]
  },
  "checks": [
    {"name": "tests", "program": "cargo", "args": ["test", "--locked"]}
  ],
  "review": {
    "requirements": "Add example.txt containing Hello",
    "base_ref": "HEAD",
    "model": "opencode/claude-sonnet-4.5"
  }
}
```

`execution` is the executor library's input schema, passed through without translation. Operations may interleave exact-text edits and file/directory operations. Supported types are `create_file`, `edit_file`, `replace_file`, `delete_file`, `move_file`, `create_directory`, `delete_directory`, and `move_directory`. Each check is an **argv array**, not a shell command. Commands execute sequentially from the repository root. The reviewer sees `base_ref` versus `:working`, including uncommitted changes. Design should use a dedicated working tree to avoid mixing unrelated edits into the review.

Output is one JSON object (`conductor.result/v1`) with `status`, `phase`, executor outcome, per-file formatter results, completed check results, review result, and optional error:

- `PASSED` (exit 0): all operations, selected formatters and checks succeeded, and review approved.
- `NEEDS_DESIGN` (exit 2): an operation failed, a formatter failed or emitted invalid output, a check failed, or review requested changes. Prior file changes are **not rolled back**.
- `BLOCKED` (exit 3): invalid input, unavailable formatter/check command, formatting filesystem or target-change errors, or indeterminate/unavailable review. No AI retry is attempted.

Failures always return to Design. Conductor never rewrites or regenerates operations.

### Exact-text edits and patch migration

`edit_file` applies literal `oldText`/`newText` replacements to an existing regular UTF-8 file. Every `oldText` must be non-empty and match exactly once, and edit ranges must not overlap. All edits in one operation resolve against the same original contents at the start of that operation, after earlier operations have completed. No whitespace or line-ending normalization occurs. Match validation completes before that operation writes the file; earlier successful operations are not rolled back on failure.

**Breaking input change:** the former `patch` operation, including both inline and file-source forms, is no longer accepted. Conductor does not import or translate unified diffs. Replace localized patches with `edit_file`, use `replace_file` for hash-guarded whole-file replacement, and use explicit file/directory operations for structural changes. A legacy patch anywhere in a request fails deserialization before execution: the CLI returns `BLOCKED` with `phase: "input"`, and no operations run. The `conductor.request/v1` and `conductor.result/v1` envelope names are unchanged.

### Affected-file reporting

The nested `execution` outcome now always includes `affected_files`, for example:

```json
{
  "success": true,
  "operations_applied": 2,
  "affected_files": ["example.txt"]
}
```

Only a fully successful execution batch populates this list. Paths are sorted, unique, repository-relative, use `/` separators, and identify surviving affected regular files at their final locations. Files targeted by identical-content edits and files relocated by directory moves are included; deleted files, directories, symlinks, and unrelated pre-existing working-tree changes are not. Empty batches and every execution failure, including partial application, report `[]`; earlier changes remain applied. The list is not a Git baseline diff or a partial-execution recovery ledger. Later check or review failures do not erase the outcome of a successful execution.

Conductor exposes Executor's metadata unchanged, including when a later formatting phase fails. Consumers that reject unknown output fields must allow `execution.affected_files`. Rebuild and replace installed Conductor binaries before sending `edit_file` requests; changing this repository's dependency alone does not upgrade an installed binary.

### Formatting affected files

An optional `formatters` array in `conductor.request/v1` enables formatting after the entire Executor batch succeeds and before checks and review:

```json
"formatters": [
  {
    "name": "rust",
    "extensions": ["rs"],
    "program": "rustfmt",
    "args": ["--edition", "2024", "--emit", "stdout", "--config-path", "{parent}"]
  },
  {
    "name": "prettier",
    "extensions": ["js", "ts", "tsx"],
    "program": "prettier",
    "args": ["--stdin-filepath", "{path}"]
  }
]
```

Rules are explicit, not discovered by Conductor. Missing or empty `formatters` leaves existing behavior unchanged. Each rule requires a nonblank name/program and a nonempty list of plain extensions without dots, separators or whitespace; `args` defaults to `[]`. Extensions match the final filename suffix case-insensitively. Duplicate extension assignments, including within a rule, are rejected during input validation before any operations run. Unknown rule fields are rejected. Unmatched files and rules with no matching affected files cause no formatter invocation. Rules run in declaration order and their matching paths in sorted, deduplicated order.

Only surviving paths from the successful `execution.affected_files` are candidates. Conductor does not discover targets from Git status or the review baseline. For each file, it sends the complete UTF-8 source on stdin and treats complete stdout as the replacement source. No filename is automatically appended. Commands use argv, not a shell, and run from the repository root. Relative executable paths such as `./tools/formatter` are resolved from that root; bare executable names use PATH. Whole arguments `{path}` and `{parent}` expand to the repository-relative target and its parent directory (`.` for root files). Embedded placeholders such as `--file={path}` remain literal. These arguments supply configuration/parser context, not permission to rewrite extra files. Configure the Rust edition explicitly or through rustfmt configuration; Conductor does not infer it from Cargo manifests.

Formatters must emit only formatted source on stdout, not check-mode output, diffs or logs. Source is never truncated; diagnostics are bounded. Nonzero exit, invalid UTF-8 stdout or empty stdout for nonempty input returns `NEEDS_DESIGN` at phase `formatting` without replacing that file. Empty input may produce empty output. Unchanged output causes no rewrite. Conductor revalidates safe regular-file paths (including symlink/reparse-point protections) and compares current contents with the original input before committing. Changed output is written to a temporary sibling and replaces only the target, preserving permissions and avoiding writes through hard links. This is per-file replacement, not a batch transaction or a crash-durability guarantee.

Launch, source-read, replacement and target-change errors return `BLOCKED` at phase `formatting`. Any failure stops later formatting, checks and review, while preserving earlier execution and formatting changes. There is no rollback or retry. Executor's affected-file metadata is not rewritten; checks and review see the formatted working tree.

The result always includes a `formatters` array, empty when no files were attempted. Each attempted file records `name`, `path`, `program`, expanded `args`, `exit_code` (null when unavailable), `changed`, and bounded `stderr`. Formatted source is not duplicated in results. Output consumers rejecting unknown fields must allow this new array. The v1 envelope names are unchanged; Rust consumers constructing `WorkflowRequest`/`WorkflowResult` literals must accommodate their new fields.

Formatter executables and plugins remain trusted, not sandboxed: stdin/stdout invocation limits Conductor's own writes, not arbitrary command side effects. Target checks below the resolved repository root are optimistic, not filesystem locking. Ordinary target renames, removals or content changes detected before replacement return a formatting error; Conductor does not search for moved targets or recreate them. The repository root and its ancestor directories must remain stable throughout the workflow. Concurrent root/ancestor relocation, redirection or replacement (including a different directory with identical target contents) is explicitly out of scope; no directory-identity tracking or adversarial filesystem isolation is provided. Output is captured in memory and there are no process timeouts or output-size limits. Pi/OpenCode adapters do not yet expose formatter rules; this feature is available through the library and CLI workflow request. Adapter changes and installation are separate work.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -- run --request workflow.json
```

Formatting tests compile a portable Rust subprocess fixture with `rustc` and exercise stdin-mode `rustfmt`; install the rustfmt component (as CI does).

Dependencies are the reusable Rust libraries from [executor](https://github.com/jared-two-foxes/executor) and [review](https://github.com/jared-two-foxes/review); neither CLI is spawned. The reviewer uses the credentials/provider configuration supported by `review-app`.

## Scope of the first version

The first version provides one execution/check/review pass and a test seam for injecting a deterministic reviewer. It does not implement process timeouts, cancellation, workspace isolation, automated retries, OpenCode/Pi adapters, or a persistent workflow ledger. Run untrusted verification commands only in a suitably isolated environment. A subsequent change should add per-check timeouts and cancellation before unattended use.
