# ground-control

GroundControl is a personal, local-only developer platform for one Mac. It is a
disciplined wrapper around the terminal. It runs the CLIs you are already logged
in to (`git`, `gh`, `wrangler`, `pnpm`, `cargo`, `docker`) as ordinary shell
steps. On top of that it adds a cross-project status view, a full history of runs
and deploys, approval steps for risky actions, and one place to start and stop
local test stacks.

- Specification: [`docs/groundcontrol-spec.md`](docs/groundcontrol-spec.md) (the source of truth)
- Implementation status, decisions and open items: [`docs/implementation.md`](docs/implementation.md)
- Design references: [`design/`](design/)

Nothing runs in the background on its own and nothing starts at login. One Rust binary plays two roles:

| Command | What it is |
|---|---|
| `groundcontrol` | The TUI, the only interface. Open it when you need it. |
| `groundcontrol svc-exec …`, `groundcontrol run-exec …` | Workers. GroundControl starts one per service and per pipeline run, each as the command of its own tmux window in a session per project (`gc-<project>`). You never type these. |

tmux keeps the workers alive, so quitting the TUI never stops a run or a
service, and you can attach to any of them (`tmux ls`). Every process has a
visible parent: Terminal → tmux → `groundcontrol` → your command. GroundControl
opens no socket or network listener, installs no background agent, holds no
credentials and needs no root. Why it is built this way:
[`docs/architecture.md`](docs/architecture.md).

## Install (macOS)

Prerequisites: Rust (stable) and tmux.

```bash
make install
```

This builds `~/.local/bin/groundcontrol` and links this repository's
`groundcontrol.yaml` into `~/Library/Application Support/GroundControl/`, so the
TUI finds it from any directory. It registers nothing to start at login.

To update: `git pull && make install`. Running workers keep the binary they
started with; new windows use the new one.

Open the TUI from any terminal with `groundcontrol`. A tmux popup works well:

```
bind g display-popup -E -w 90% -h 85% groundcontrol
```

`make uninstall` removes both and keeps history and logs.

Steps run under `/bin/zsh -c` inside tmux, so they inherit your interactive
shell's environment (PATH, `brew shellenv`, version managers). Each step and
service starts in its own session with stdin from `/dev/null` and no controlling
terminal, so nothing can stop and wait for input.

## Configuration

`groundcontrol.yaml` at the root of this repository is the only configuration.
It is edited by hand; the TUI never writes it. An open TUI reloads it on save,
and each worker uses the config as it was when it started. An invalid file is
rejected, the previous valid config stays active, and the errors
(with line and column) appear on the Health screen (`3`).

The schema is spec §7.1, plus one optional pipeline field, `warning:`. It holds a
destructive-action warning that the run dialog shows in red; see `restore-d1` and
`rollback-worker`. Values the spec still leaves open (§13) are written as
commands that fail loudly with a `TODO(§13.x)` message, so nothing runs against
a guessed target.

Validation (§7.4) rejects these as errors:

- unknown keys and unresolved references
- protected pipelines without an `approve:` step
- `push --force`, `push -f`, `reset --hard`, `clean -f` and `branch -D`
- `--remote` in a service `start` command
- `depends_on` cycles
- string inputs without a `pattern`, and choice inputs without `options`

It warns about literal values in secret-looking keys, and disables any project
whose path is missing.

## Using the TUI

| Key | Action |
|---|---|
| `1` `2` `3` | Dashboard, History, Health |
| `tab` / `shift+tab` | Next or previous pane |
| `↑↓` / `j k` | Move the selection |
| `enter` | Open the selection (on the dashboard approval banner: review the run) |
| `esc` | Back |
| `?` | Help, with the keys for every screen |
| `q` | Quit the TUI (runs and services keep going) |

- **Dashboard.** Approval banner, projects (branch, tree, ahead/behind), services and groups of the selected project, recent runs, and the selected service's last 50 log lines. `s` starts or stops, `r` restarts, `l` opens the full log. A running service is the tmux window `gc-<project>:svc-<service>`; `C-c` there stops it too.
- **Project.** `enter` opens the run dialog, `d` does a dry run, `e` switches environment. On the services pane, `s`, `r` and `l` work as on the dashboard.
- **Run dialog.** Shows a live pattern check for each input, dirty and unpushed warnings, destructive warnings and the step outline. `enter` starts the run; `ctrl+d` does a dry run. For protected runs, this dialog is the confirmation.
- **Run view.** Steps, outputs and live terminal output (colours, `\r` progress, wide characters). `f` toggles follow, `y` and `Y` copy the command or output, `c` cancels the run. `a` approves (a protected run asks for a second `y`) and `x` rejects. The run is also the tmux window `gc-<project>:run-<id>`. When it needs approval or fails, every attached tmux client shows a message in its status line; with no tmux client attached, nobody is told until a TUI opens.
- **History.** `/` filters (`project:… pipeline:… env:… status:…`), `s` cycles the status filter, `ctrl+u` clears the filter, `enter` opens a run and `D` shows a run's saved diff.
- **Health.** Tool check (resolved on `PATH` without running anything), tmux and Docker reachability (Docker only while this screen is open), config errors with line and column, reload times, the version, the state folder and the config path.

## Development

```bash
make test
```

```bash
make lint
```

`make test` runs the unit tests and the end-to-end tests, which start real
workers in a private tmux server and skip themselves when tmux is missing.
`make lint` runs rustfmt and clippy with `-D warnings`.

It has only been tested on macOS. It is meant to build on Linux too, where the
clipboard falls back from `pbcopy` to `wl-copy` or `xclip`. To try it without
installing, point it at a scratch state folder from inside tmux:

```bash
GC_STATE_DIR=/tmp/gc GC_CONFIG=$PWD/groundcontrol.yaml cargo run
```

On Linux, `/bin/zsh` must be installed, because steps always run under it.

### Layout

```
src/
  config/        groundcontrol.yaml: typed config, parsing with positions, validation (§7)
  yaml.rs        YAML tree with line/column positions (yaml-rust2 event parser)
  protocol.rs    requests, responses, events and data types (§10.6)
  engine.rs      the TUI's in-process engine: requests, polling, config watch
  ctx.rs         what the TUI and workers share: tmux server, binary, state, config
  runs/          recording runs, approvals and cancel; the run-exec worker
  services/      start/stop in dependency order, health, restart policy; the svc-exec worker
  tmux.rs        tmux sessions and windows (argv commands, exact targets)
  lock.rs        flock run and service locks
  store.rs       SQLite store (§11)
  process.rs     spawning steps in their own session and process group
  logfile.rs     rotated log files
  mask.rs        streaming secret masker (§9.3)
  envfile.rs     .env parsing
  git.rs         status parsing, repository guard, snapshots and diffs
  paths.rs       state folder, config, logs, locks (§11)
  tui/           app state, screens, theme tokens, vt100 output pane
tests/           tmux, services, runs and engine end-to-end tests
```
