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

One Rust binary has two modes:

| Command | What it is |
|---|---|
| `groundcontrol daemon` | The daemon, run by `launchd` as a user agent. It owns every run and every service. |
| `groundcontrol` | The TUI. It is a client of the daemon; quitting it never stops a run or a service. |

The daemon listens only on a Unix socket (`0600`, with a peer UID check). It
opens no network listener, holds no credentials and needs no root.

## Install (macOS)

Prerequisites: Rust (stable). Move your PATH setup from `~/.zshrc` to
`~/.zprofile`, because steps run under `/bin/zsh -l -c`, which reads only
`.zprofile`. That includes PATH exports, `brew shellenv` and any Node or Rust
version managers.

```bash
make install
```

This does three things:

1. Builds `~/.local/bin/groundcontrol`.
2. Writes `~/Library/LaunchAgents/dev.binara.groundcontrol.plist`, which points `GC_CONFIG` at this repo's `groundcontrol.yaml`.
3. Loads and starts the agent.

To update: `git pull && make install`. This rebuilds the binary and restarts the
daemon. Services that were running come back, because it is the same boot.

Open the TUI from any terminal with `groundcontrol`. A tmux popup works well:

```
bind g display-popup -E -w 90% -h 85% groundcontrol
```

Other targets: `make restart`, `make logs` (tails the daemon log), `make uninstall` (keeps state).

## Configuration

`groundcontrol.yaml` at the root of this repository is the only configuration.
It is edited by hand; the TUI never writes it. The daemon reloads it on save. An
invalid file is rejected, the previous valid config stays active, and the errors
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

- **Dashboard.** Approval banner, projects (branch, tree, ahead/behind), services and groups of the selected project, recent runs, and the selected service's last 50 log lines. `s` starts or stops, `r` restarts, `l` opens the full log.
- **Project.** `enter` opens the run dialog, `d` does a dry run, `e` switches environment. On the services pane, `s`, `r` and `l` work as on the dashboard.
- **Run dialog.** Shows a live pattern check for each input, dirty and unpushed warnings, destructive warnings and the step outline. `enter` starts the run; `ctrl+d` does a dry run. For protected runs, this dialog is the confirmation.
- **Run view.** Steps, outputs and live terminal output (colours, `\r` progress, wide characters). `f` toggles follow, `y` and `Y` copy the command or output, `c` cancels the run. `a` approves (a protected run asks for a second `y`) and `x` rejects.
- **History.** `/` filters (`project:… pipeline:… env:… status:…`), `s` cycles the status filter, `ctrl+u` clears the filter, `enter` opens a run and `D` shows a run's saved diff.
- **Health.** Tool check, Docker and tmux reachability, config errors with line and column, reload times, and the daemon version and uptime.

## Development

```bash
make test
```

```bash
make lint
```

`make test` runs unit tests and end-to-end daemon tests; `make lint` runs rustfmt and clippy with `-D warnings`.

The code builds and its tests pass on Linux too; only `make install`
(launchd), notifications (`osascript`) and `pbcopy` are macOS-specific. To try
it without installing, point both modes at a scratch state folder:

```bash
GC_STATE_DIR=/tmp/gc GC_CONFIG=$PWD/groundcontrol.yaml cargo run -- daemon
```

```bash
GC_STATE_DIR=/tmp/gc cargo run
```

Set `GC_LOG_STDERR=1` to mirror the daemon log to stderr. On Linux, `/bin/zsh`
must be installed, because steps always run under it.

### Layout

```
src/
  config/        groundcontrol.yaml: typed config, parsing with positions, validation (§7)
  yaml.rs        YAML tree with line/column positions (yaml-rust2 event parser)
  protocol.rs    client protocol types (§10.6)
  client.rs      socket client shared by the TUI and the end-to-end tests
  mask.rs        streaming secret masker (§9.3)
  envfile.rs     .env parsing
  git.rs         status parsing, repository guard, snapshots and diffs
  paths.rs       state folder, socket, logs (§11)
  daemon/        store (SQLite), runner, supervisor, socket server, host integration
  tui/           app state, screens, theme tokens, vt100 output pane
tests/daemon_e2e.rs   drives the real daemon binary over its socket
```
