# conductor

A deterministic Rust command runner. Conductor runs an explicit list of commands
in order and stops at the first failure. It does not interpret command output,
generate changes, retry commands, or choose the next action.

Executor, Review, formatters, build tools, tests, and Git are external programs
chosen by the caller. None of the Executor or Review libraries are linked into
Conductor.

## Contract

```sh
conductor run --request workflow.json
conductor run --request -
```

`-` reads the request JSON from stdin. Example `workflow.json`:

```json
{
  "schema": "conductor.request/v2",
  "working_directory": "/absolute/path/to/project",
  "commands": [
    {"name": "apply", "program": "executor", "args": ["apply", "/absolute/path/to/executor-input.json"]},
    {"name": "compile", "program": "cargo", "args": ["build", "--locked"]},
    {"name": "tests", "program": "cargo", "args": ["test", "--locked", "specific_test"]},
    {"name": "review", "program": "review-cli", "args": ["run", "--request", "/absolute/path/to/review-input.json"]},
    {"name": "commit", "program": "./tools/checkpoint-verified-task", "args": []}
  ]
}
```

The final command illustrates a **caller-provided** checkpoint program, not a
program supplied by Conductor. It can verify the reviewed tree and create a Git
commit. A plain `git commit` command can also be used if the caller has already
prepared the intended index and accepts the ordinary Git hooks and behaviour.
If review exits nonzero, neither kind of commit command runs.

The caller prepares each tool's request files before invocation, using that
tool's own schema. Review's request specifies its repository, base/head refs,
and requirements; its CLI flags control provider/model and review budgets.
Conductor does not build those requests or supply defaults for them.

### Execution rules

- `working_directory` is a required, existing absolute directory. It need not be
  a Git repository. It is resolved once before execution and is the working
  directory for every child.
- `commands` is required. An empty list succeeds without launching anything.
- Each command requires a nonblank `name` and `program`; `args` defaults to `[]`.
  Names are diagnostic labels, have no special meaning, and need not be unique.
- The schema, directory, and every command are validated before the first child
  starts. Unknown fields and NUL in programs/arguments are rejected. Executable
  availability is checked when that command is launched, so an earlier command
  can create an executable used later.
- Programs run directly with literal argv, without a shell or interpolation.
  Bare names use inherited `PATH`; explicit relative executable paths such as
  `./tools/checkpoint` resolve from `working_directory`. Argument paths are
  interpreted by the child, normally relative to that directory.
- Children inherit Conductor's environment and receive closed stdin. Supply
  tool input through files/arguments, or explicitly call a wrapper program if
  another input mechanism is needed. This is sequencing, not a stdout pipeline.
- Only exit code **0** permits the next command to run. Any nonzero exit,
  termination by signal, or launch/wait error stops the sequence. No later
  commands run, and no earlier side effects are rolled back.
- There is no automatic retry, recovery replay, failure branch, timeout,
  cancellation protocol, or workspace isolation. Commands are trusted and may
  have arbitrary side effects. The caller owns recovery and source identity
  checks, including ensuring a commit contains exactly the reviewed tree.

### Result and exit codes

Stdout contains one JSON result (`conductor.result/v2`). Child stdout/stderr are
captured inside it, so child logs do not corrupt the result envelope.

| Result status | Conductor exit | Meaning |
| --- | --- | --- |
| `PASSED` | 0 | Every command exited 0, or the list was empty. |
| `FAILED` | 1 | A child exited nonzero or was terminated by a signal. |
| `BLOCKED` | 2 | Invalid/unreadable input, or a command could not be run. |

Conductor's exit code is independent of the child's code. For example, a review
command exiting 2 produces `FAILED`, Conductor exit 1, and a recorded child
`exit_code` of 2. Conductor does not interpret a review decision embedded in JSON;
tools or wrappers must express failure through their process exit status.
Clap handles help/version and CLI usage errors separately from workflow results.

Example stopped result (no record is produced for the skipped commit command):

```json
{
  "schema": "conductor.result/v2",
  "status": "FAILED",
  "phase": "commands",
  "commands": [
    {
      "index": 0,
      "name": "review",
      "program": "review-cli",
      "args": ["run", "--request", "review-input.json"],
      "status": "FAILED",
      "exit_code": 2,
      "stdout": "",
      "stderr": "changes requested",
      "stdout_truncated": false,
      "stderr_truncated": false,
      "error": "Command 'review' failed (exit status: 2)"
    }
  ],
  "failed_command_index": 0,
  "error": "Command 'review' failed (exit status: 2)"
}
```

`phase` is `input` for validation/read/parse errors, `commands` for a stopped
sequence, and `completed` for success. `failed_command_index` is zero-based and
present only when an attempted command fails. `commands` contains each attempted
command in order, including one that could not start. Launch errors have status
`BLOCKED`, null `exit_code`, and an error message; signal termination has status
`FAILED` and null `exit_code`. Earlier successful command results are retained.

Each output stream is decoded as lossy UTF-8 and limited in the result to 16,384
characters, with a separate truncation flag. Full process output is still
captured in memory before truncation; this is not a process-memory bound. Tools
should write full machine-readable reports/receipts to files when needed rather
than relying on potentially truncated stdout.

## Migration from v1

This is a breaking change, released as 0.2.0 with **v2 request/result schemas**.
Legacy requests are rejected before any commands run. There is no compatibility
mode that implicitly calls Executor or Review.

| Previous built-in behaviour | Caller responsibility in v2 |
| --- | --- |
| `repository_path` | Set `working_directory`. |
| `execution` through the Executor library | Write Executor input and add its CLI invocation. |
| Affected-file formatter rules | Add formatter invocations or a wrapper selecting the intended files. |
| `checks` | Put those commands in the desired order in `commands`. |
| `review` through the Review library | Write Review input and add its CLI invocation, including desired limits. |
| Scaled review budgets and `.conductor` ledger setup | Choose Review flags/ledger location and Git exclusions in the caller. |
| Typed execution/formatting/review results | Read each tool's output or report file; Conductor records generic process results. |
| `NEEDS_DESIGN` versus `BLOCKED` domain classification | Interpret tool-specific outcomes outside Conductor. |

Existing Pi/OpenCode adapters expecting v1 must be migrated before upgrading
their installed Conductor binary. In particular, callers must retain tool
evidence and source-tree validation required by their task/checkpoint protocol.
An arbitrary post-review command is not a replacement for those validations.
This repository changes only Conductor; adapter migration is separate work.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Integration tests compile a small portable Rust subprocess fixture with `rustc`
and exercise real command ordering, failure propagation, CLI JSON, working
directory/argv handling, and output capture. CI runs on Linux and Windows.
