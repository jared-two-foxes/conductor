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
- Optional `output_directory` selects an existing absolute **parent** directory
  for command logs. Each nonempty run creates its own unique `conductor-*`
  subdirectory there. When omitted or null, the parent is the system temp
  directory, keeping logs outside the working tree by default. Empty runs and
  invalid requests create no log directory.
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
written directly to separate files, with bounded previews inside the result,
so child logs do not corrupt the result envelope or accumulate in memory.

| Result status | Conductor exit | Meaning |
| --- | --- | --- |
| `PASSED` | 0 | Every command exited 0, or the list was empty. |
| `FAILED` | 1 | A child exited nonzero or was terminated by a signal. |
| `BLOCKED` | 2 | Invalid/unreadable input, a command could not be run, or log setup/reading failed. |

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
  "output_directory": "/tmp/conductor-example",
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
      "stdout_path": "/tmp/conductor-example/000000.stdout",
      "stderr_path": "/tmp/conductor-example/000000.stderr",
      "error": "Command 'review' failed (exit status: 2)"
    }
  ],
  "failed_command_index": 0,
  "error": "Command 'review' failed (exit status: 2)"
}
```

`phase` is `input` for validation/read/parse errors, `output` if the run log
directory could not be prepared, `commands` for a stopped sequence, and
`completed` for success. `failed_command_index` is zero-based and
present only when an attempted command fails. `commands` contains each attempted
command in order, including one that could not start. Launch errors have status
`BLOCKED`, null `exit_code`, and an error message; signal termination has status
`FAILED` and null `exit_code`. Earlier successful command results are retained.

### Complete output and bounded previews

Every attempted command gets two log files, named by its zero-based index:
`000000.stdout`, `000000.stderr`, `000001.stdout`, etc. Labels never become
filenames. Files are created exclusively without overwriting existing files.
Successful and failed commands both retain their logs, including raw non-UTF-8
bytes. No files are created for skipped commands. Commands that cannot start
normally have two empty log files.

The result's `output_directory` is the absolute unique run directory, rather
than the requested parent. Each command's `stdout_path` and `stderr_path` are
absolute paths to its complete raw output. These fields are omitted if the
corresponding directory or file could not be created. Files remain after both
the library returns and the CLI process exits. Conductor does not automatically
remove them: callers own retention/cleanup, and default temp storage is subject
to the operating system's cleanup policy. Use a persistent parent when needed:

```json
"output_directory": "/absolute/path/to/existing/logs"
```

The existing `stdout` and `stderr` strings remain **previews**: the first 16,384
characters decoded as lossy UTF-8, with separate truncation flags. Conductor
reads only a bounded prefix from each file to produce these previews; it never
buffers complete command output in memory. Disk usage is proportional to output
and is not capped. Commands should finish their output writers before exiting;
Conductor waits for the direct child, not detached background processes.

For a failed test command, read `stderr_path` and/or `stdout_path` to retrieve
diagnostics beyond the preview. For a structured Review response, parse the
complete `stdout_path` file rather than a potentially truncated JSON preview.
For example, after loading the Conductor result in Python:

```python
from pathlib import Path
import json

failed = result["commands"][result["failed_command_index"]]
diagnostics = Path(failed["stderr_path"]).read_bytes()
# If this command emits JSON on stdout (for example, review-cli):
review = json.loads(Path(failed["stdout_path"]).read_bytes())
```

Log directory/file creation errors stop execution with `BLOCKED` before starting
the affected command. Preview read errors also stop the sequence with `BLOCKED`,
preserving the child's exit code if it already ran. Earlier command results and
any created log paths remain available; no later commands run. These are ordinary
local files, not an immutable evidence store or a crash-durability guarantee.

This is an additive extension of v2. Existing requests still work without
`output_directory`; output consumers that reject unknown fields must allow the
new top-level `output_directory` and command `stdout_path`/`stderr_path` fields.

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
