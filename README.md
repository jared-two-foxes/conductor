# conductor

A deterministic Rust coordinator for a code-writing Design agent. It consumes the **exact executor input authored by Design**, applies operations through the `executor` library, runs verification commands, then invokes the `review-app` library. Conductor does not ask a model to interpret executor input, retry changes, or make design decisions.

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

Output is one JSON object (`conductor.result/v1`) with `status`, `phase`, executor outcome, completed check results, review result, and optional error:

- `PASSED` (exit 0): all operations and checks succeeded, and review approved.
- `NEEDS_DESIGN` (exit 2): an operation failed, a check failed, or review requested changes. Prior file changes are **not rolled back**.
- `BLOCKED` (exit 3): invalid input, missing verification tool, or indeterminate/unavailable review. No AI retry is attempted.

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

Conductor exposes Executor's metadata unchanged; it does not format files in this version. Consumers that reject unknown output fields must allow `execution.affected_files`. Rebuild and replace installed Conductor binaries before sending `edit_file` requests; changing this repository's dependency alone does not upgrade an installed binary.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -- run --request workflow.json
```

Dependencies are the reusable Rust libraries from [executor](https://github.com/jared-two-foxes/executor) and [review](https://github.com/jared-two-foxes/review); neither CLI is spawned. The reviewer uses the credentials/provider configuration supported by `review-app`.

## Scope of the first version

The first version provides one execution/check/review pass and a test seam for injecting a deterministic reviewer. It does not implement process timeouts, cancellation, workspace isolation, automated retries, OpenCode/Pi adapters, or a persistent workflow ledger. Run untrusted verification commands only in a suitably isolated environment. A subsequent change should add per-check timeouts and cancellation before unattended use.
