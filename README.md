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
      {"type": "create_file", "path": "example.txt", "content": "hello\n"}
    ]
  },
  "checks": [
    {"name": "tests", "program": "cargo", "args": ["test", "--locked"]}
  ],
  "review": {
    "requirements": "Add example.txt containing hello",
    "base_ref": "HEAD",
    "model": "opencode/claude-sonnet-4.5"
  }
}
```

`execution` is the executor library's input schema, unchanged. Operations may interleave patches and file/directory operations. Each check is an **argv array**, not a shell command. Commands execute sequentially from the repository root. The reviewer sees `base_ref` versus `:working`, including uncommitted changes. Design should use a dedicated working tree to avoid mixing unrelated edits into the review.

Output is one JSON object (`conductor.result/v1`) with `status`, `phase`, executor outcome, completed check results, review result, and optional error:

- `PASSED` (exit 0): all operations and checks succeeded, and review approved.
- `NEEDS_DESIGN` (exit 2): an operation failed, a check failed, or review requested changes. Prior file changes are **not rolled back**.
- `BLOCKED` (exit 3): invalid input, missing verification tool, or indeterminate/unavailable review. No AI retry is attempted.

Failures always return to Design. Conductor never rewrites or regenerates operations.

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
