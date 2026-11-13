# ground-control

GroundControl is a personal, local-only developer platform for one Mac. It is a
disciplined wrapper around the terminal. It runs the CLIs you are already logged
in to (`git`, `gh`, `wrangler`, `pnpm`, `cargo`, `docker`) as ordinary shell
steps. On top of that it adds one view of everything that is running: a
cross-project status view, pipeline runs with approval steps for risky actions,
and one place to start and stop local test stacks. It keeps no history, logs or
other data: everything it shows lives in tmux and goes away with it.

- Specification: [`docs/groundcontrol-spec.md`](docs/groundcontrol-spec.md) (the source of truth)
- Implementation status, decisions and open items: [`docs/implementation.md`](docs/implementation.md)

Nothing runs in the background on its own and nothing starts at login. One Rust binary plays two roles:

| Command | What it is |
|---|---|
| `groundcontrol` | The TUI, the only interface. Open it when you need it. |
| `groundcontrol svc-exec …`, `groundcontrol run-exec` | Workers. GroundControl starts one per service and per pipeline run, each as the command of its own window in the tmux session `ground-control` (created if it doesn't exist). You never type these. |

tmux keeps the workers alive, so quitting the TUI never stops a run or a
service, and you can attach to any of them (`tmux attach -t ground-control`). Every process has a
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
`groundcontrol.yaml` into `~/.config/ground-control/`, so the TUI finds it from
any directory. It registers nothing to start at login.

To update: `git pull && make install`. Running workers keep the binary they
started with; new windows use the new one.

Open the TUI from any terminal with `groundcontrol`. A tmux popup works well:

```
bind g display-popup -E -B -w 90% -h 85% groundcontrol
```

`make uninstall` removes both.

Steps run under `/bin/zsh -c` inside tmux, so they inherit your interactive
shell's environment (PATH, `brew shellenv`, version managers). Each step and
service starts in its own session with stdin from `/dev/null` and no controlling
terminal, so nothing can stop and wait for input.

## Configuration

`groundcontrol.yaml` at the root of this repository is the only configuration.
It is edited by hand; the TUI never writes it. An open TUI reloads it on save,
and each worker uses the config as it was when it started. An invalid file is
rejected, the previous valid config stays active, and the errors
(with line and column) appear on the Health screen (`2`).

The schema is spec §7.1, plus one optional pipeline field, `warning:`. It holds a
destructive-action warning that the run dialog shows in red.

Validation (§7.3) rejects these as errors:

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
| `1` `2` | Dashboard, Health |
| `tab` / `shift+tab` | Next or previous pane |
| `↑↓` / `j k` | Move the selection |
| `enter` | Open the selection (on the dashboard approval banner: review the run) |
| `esc` | Back; also closes help, viewers, confirms and dialogs |
| `?` | Help for the current screen, with the keys that work everywhere |
| `q` | Quit the TUI (runs and services keep going); closes help, output viewers and confirms instead when one is open |
| `ctrl+c` | Quit from anywhere |
| mouse | Click selects a row and focuses a pane, and the top-bar tabs switch screens. Double-click opens the row, like `enter`. The wheel moves the selection in lists and scrolls output (sideways pans). Dialog fields and buttons and confirm buttons are clickable. |

The TUI captures the mouse, so selecting text with the terminal needs `shift`
(`option` in iTerm).

- **Dashboard.** Full-width bands from top to bottom, which is also the `tab` order: a one-line approval banner (only while an approval waits), projects (branch, tree, ahead/behind, services), the runs that still have a window, then the selected project's services and groups beside the selected service's output (its last 50 lines). The focused pane has a thick border. `enter` on a project opens it, on a run opens the run view, on a service shows all of its output. With the services pane focused, `s` starts or stops the selected service or group, `r` restarts it and `l` shows all of its output. A running service is the window `<project>/<service>` in the `ground-control` session; `C-c` there stops it too.
- **Project.** Pipelines and the selected pipeline's steps beside the project's services. `tab` switches between the pipelines and services panes. On pipelines, `enter` opens the run dialog, `d` does a dry run (it asks for missing inputs first) and `e` switches environment. On services, `s`, `r` and `l` work as on the dashboard, and `enter` shows all of the service's output.
- **Run dialog.** Environment, branch and commit, dirty and unpushed warnings, one field per input with a live pattern check, destructive warnings and the step outline. The error row and the buttons stay pinned to the bottom. `tab` and `↑↓` move between fields; the bottom bar shows the keys of the focused field. `enter` starts the run, `ctrl+d` does a dry run and `esc` cancels. For protected runs, this dialog is the confirmation.
- **Run view.** Steps beside an `outputs · inputs` pane, and the selected step's live terminal output (colours, `\r` progress, wide characters). `↑↓` select a step, `enter` jumps to its output's end, `f` toggles follow, `y` and `Y` copy the step's command or output, `c` cancels the run (after a confirm). While an approval waits, a banner shows it: `a` approves (a protected run asks for a second `y`) and `x` rejects. The run is also the window `<project>/<pipeline>` in the `ground-control` session, and its id is that window's id. A succeeded run's window closes after 5 seconds, which ends the run; a failed run's window stays until the project's next run. When a run needs approval or fails, every attached tmux client shows a message in its status line; with no tmux client attached, nobody is told until a TUI opens.
- **Health.** The version and when the TUI opened, a tool check that lists each tool once (tmux and Docker also say whether they answer; Docker only while this screen is open), and the config: file, load and reload times, and errors and warnings with line and column. `r` checks again.

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
installing, run it from inside tmux:

```bash
GC_CONFIG=$PWD/groundcontrol.yaml cargo run
```

On Linux, `/bin/zsh` must be installed, because steps always run under it.

### Layout

```
src/
  config/        groundcontrol.yaml: typed config, parsing with positions, validation (§7)
  yaml.rs        YAML tree with line/column positions (yaml-rust2 event parser)
  main.rs        the command line: the TUI, `svc-exec`, `run-exec`, `--version`
  lib.rs         the modules below
  model.rs       the data types the engine returns, its events, and the status values the window schema encodes (§10.6)
  engine.rs      the TUI's in-process engine: its calls, polling, config watch
  ctx.rs         what the TUI and workers share: tmux server, binary, config
  worker.rs      plumbing both workers share: pane writes, signals, waking on a request, alerts
  runs/          starting runs, approvals, cancel; the run-exec worker; pane.rs splits a run's pane into steps
  services/      start/stop in dependency order, health, restart policy; the svc-exec worker
  window.rs      the window schema: every @gc_* option of run and service windows, and its format
  tmux.rs        the ground-control session and its windows; moves @gc_* options without knowing them
  process.rs     spawning steps in their own session and process group
  mask.rs        streaming secret masker (§9.3)
  envfile.rs     .env parsing
  envlayers.rs   env layers: every process environment and approval message is built here (§6.1)
  git.rs         status parsing, repository guard, run snapshots
  paths.rs       where the config is
  tui/           app state (app.rs), theme tokens, vt100 output pane (term_view.rs), widgets, and
    mouse.rs       the clickable areas of the last frame
    clipboard.rs   copying to the system clipboard
    screens/       dashboard, project, run_view, dialog (run dialog), health, services (the services pane
                   both dashboard and project show), overlay (help, viewer, confirm)
    tests.rs       app-level tests: mouse input and overlays against a drawn frame
tests/           tmux, services, runs and engine end-to-end tests
```
