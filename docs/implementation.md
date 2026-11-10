# GroundControl: implementation notes

Status of the implementation against [`groundcontrol-spec.md`](groundcontrol-spec.md),
the decisions taken where the spec left room, and what is still open.

## Checklist (spec §12 MVP)

### Architecture ([`architecture.md`](architecture.md))

- [x] `groundcontrol` is the TUI with an in-process engine (`engine.rs`): config loading, validation (§7.4) with line and column, hot reload that keeps the last good config, git polling, and live updates from one `list-windows` per poll plus pane captures for what is on screen
- [x] Runs execute in `groundcontrol run-exec` in a `<project>/<pipeline>` window, services in `groundcontrol svc-exec <project> <service>` in a `<project>/<service>` window, all in the `ground-control` tmux session, created if it doesn't exist (`runs/worker.rs`, `services/worker.rs`, `tmux.rs`)
- [x] All state in tmux: `@gc_*` window options (percent-encoded where tmux would alter them) and pane scrollback. No database, log files, history or lock files. `window.rs` is the only code that knows the options' names and formats, and a test keeps it in step with spec §11
- [x] Workers wait on their window's `tmux wait-for` channel and are woken by the TUI's requests, so an idle worker starts no processes
- [x] A run's id is its window's id; a dead window with an active run shows as "worker exited unexpectedly"
- [x] One run per project and one worker per service, decided by the oldest live window
- [x] Tool check resolves on `PATH` in-process; tmux checked at TUI start and on the Health screen; Docker only while the Health screen is open
- [x] No socket, no `launchd`, no `osascript`, `ps`, `lsof` or `sysctl`; nothing starts at login

### Pipelines (§6, §8)

- [x] Steps run as `/bin/zsh -c` in a new session (own process group, no controlling terminal), stdin `/dev/null`, stdout and stderr merged in order
- [x] Environment precedence (§6.1), including `INPUT_*`, `OUTPUT_*`, `GC_*`, `FORCE_COLOR`, `CLICOLOR_FORCE`, `TERM` and `COLUMNS=120`
- [x] Typed inputs validated before the run (pattern, choice, bool); they reach commands only as environment variables
- [x] Per-step timeout (default 15m); cancel sends `SIGTERM` to the group, then `SIGKILL` after 10 s
- [x] One run per project; a second trigger is rejected, never queued
- [x] `approve:` steps:
  - pause with `awaiting_approval` and alert the attached tmux clients; the TUI writes `@gc_decision` and wakes the worker through its `tmux wait-for` channel
  - expire after 30 minutes with `cancelled (approval expired)`
  - the first answer wins; later answers get `already_decided`
  - a rejection gives the distinct status `rejected`
  - protected approvals and protected runs are refused without the confirm flag
- [x] Step outputs through `$GC_OUTPUT` (a temp file deleted when the run ends), published on the run's window, shown in the run view, and passed on as `OUTPUT_<KEY>`
- [x] `service:` steps start and stop services through the same `services::control` as the TUI
- [x] Dry run: resolved commands, working directory, env var *names*, inputs, approvals, and the blockers a real run would hit
- [x] Repository guard (`origin` normalised across SSH and HTTPS forms), checked before every run and service start
- [x] Dirty and unpushed snapshot at run start, shown in the run view
- [x] Secret masking of `env_file` values (6 or more characters) before output reaches the pane, streaming-safe across chunk boundaries (`mask.rs`, unit-tested)

### Services (§6.5)

- [x] HTTP-200 and TCP-port health checks; the 30 s start deadline; without a check, alive counts as healthy
- [x] Starting a service starts its dependencies first and waits for them to be healthy. Groups start in dependency order and stop in reverse
- [x] `restart: on-failure`: back-off from 1 s doubling to 60 s; after 5 crashes in 5 minutes the service is marked **crashed** and the attached tmux clients are alerted. `restart: never`: exit 0 shows as stopped, non-zero as crashed
- [x] A port in use refuses the start with "port N is in use (not started by GroundControl)"
- [x] Service output only in the window's scrollback; the dashboard shows its last 50 lines from a pane capture
- [x] Stop through the window's `@gc_stop` and a wake (works while the pane is in copy mode); `C-c` and closing the window stop it too
- [x] A worker keeps its window alive until its process group is gone; a worker that finds an older live window for the same service closes its own
- [x] Nothing is restored after a reboot or logout
- [x] Alerts are a status-line message to every attached tmux client plus the bell in the worker's pane

### TUI (§10)

- [x] Chrome:
  - top bar with screen tabs, breadcrumbs, `!` attention markers, config status with reload time, and the clock
  - bottom key bar
- [x] Theme tokens in one place (`tui/theme.rs`); every status carries a word or symbol, never colour alone
- [x] Layout safety:
  - fixed or flexible columns measured in display cells (`unicode-width`), truncated with `…`
  - pane titles clipped to the pane
  - a "terminal too small" screen below 100×28
- [x] Screens: Dashboard, Project, Run dialog, Run view, Health, plus help, service output viewer, dry-run viewer and confirm prompts. No history screen
- [x] Output pane:
  - `vt100` parser, 120 columns with horizontal scroll
  - follow mode and scrollback
  - each step's output is cut from the run's pane capture at the worker's step headers and replaces the view, keeping the scroll position
- [x] Approval only in the run view; the dashboard banner offers only "review". A protected approval takes two keys; a rejection takes one
- [x] If the engine cannot start: an explanatory screen that retries automatically

### Install (§5.4)

- [x] `make install` builds `~/.local/bin/groundcontrol` and links `groundcontrol.yaml` into `~/.config/ground-control/`. Nothing starts at login

### Projects (§7.2)

- [x] `groundcontrol.yaml` holds tmux-ui-manager. It passes validation; a unit test keeps it that way

## Decisions where the spec left room

| Topic | Decision |
|---|---|
| YAML crate (§13.8) | `yaml-rust2`'s event parser builds a small tree that carries positions (`src/yaml.rs`). Typed config is built from it, so every schema and validation error has a line and column. It also catches duplicate keys and unknown keys, so typos don't pass silently. |
| Destructive warnings | The spec asks the run dialog to show them but gives no config field. An optional pipeline field `warning:` was added. |
| `env_file` path | Resolved relative to the step's or service's `cwd`. |
| "HEAD isn't on origin" | `git rev-list --count HEAD --not --remotes=origin` > 0. The count is kept on the run's window. |
| Run IDs | The number of the run's tmux window id (`@12` → `#12`). Ids restart when the tmux server does. |
| Service start semantics | Starting a single service starts its `depends_on` chain first. A process that exits during its first start is reported as a failed start, not fed into the restart loop. Health is re-checked every 5 s while a service runs (`unhealthy` is shown, not restarted). |
| Removed services | A worker uses the config it started with, so a running service that is removed from the config keeps running in its window. The TUI no longer lists it; stop it with `C-c` in its window. |
| Leftover processes | Never adopted or signalled. A worker that is SIGKILLed leaves its child running; the status says "worker exited unexpectedly" and the next start of a service with a health port reports the port as in use. |
| Health URLs | Plain `http://` only, a minimal built-in HTTP/1.1 client (no TLS dependency). Port checks try IPv4 and IPv6 loopback, because Vite may bind only `::1`. |
| Protected runs | The run dialog is the confirmation dialog. The TUI sends `confirmed: true`; protected runs and protected approvals are refused without it. |
| Minimum terminal size (§13.9) | 100×28. |
| Theme colours (§13.9) | Amber `#e5b567` accent and warning, green `#a6cc70`, red `#f07178`, muted `#838b99`, selection `#2c3a52`, background `#1b1f27`. |
| tmux layout | One session, `ground-control`. Windows are named `<project>/<service>` and `<project>/<pipeline>` for display; what a window is comes from its `@gc_kind`, `@gc_project` and `@gc_service`/`@gc_pipeline` options, and windows are addressed by id. |
| Finished runs | A succeeded run's window closes 5 s after it ends; a failed one stays until the project's next run. Nothing remains after that. |
| Placeholder commands (§13.1) | Written as commands that print `TODO(§13.x)` and exit 1. Unknown health ports are commented out. |

## Verification

- `cargo test` (on macOS with tmux 3.8; not tested on Linux):
  - unit tests: config rules, YAML positions, masking at every split point, env files, git parsing, remote normalisation, tmux option encoding and window parsing, run-state encodings, service status from windows, splitting a pane into steps, the restart policy, health checks, process sessions and groups, terminal rendering and run-view output replacement
  - `tmux_e2e`: the `ground-control` session created once and reused, options set before the command runs, values with separators, newlines and non-ASCII, argv commands, dead panes
  - `services_e2e`: port in use, health checks, failed start, on-failure restarts, group stop, masking in the pane, stop in copy mode, `C-c`, closing the window, a duplicate worker closing itself
  - `runs_e2e`: inputs, env precedence, masking, outputs, dry run, failure and skip, a succeeded run's window closing itself, protected confirm, approval with first-answer-wins, reject, cancel while a step prints constantly, closing the window, `service:` steps, a dead worker, guards
  - `engine_e2e`: projects, run, step and output events, approvals through the engine, service events and output, health, config reload keeping the last good config, and the real TUI rendering its dashboard in tmux
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are clean.
- Manual pass on macOS: from the TUI, started a service (it opened in the `ground-control` session), ran a pipeline and saw its output, approved a gated run, saw the succeeded windows close themselves, and quit with the service still serving. GroundControl wrote no files.

## Not verified, and known limitations

- **The security tool.** Whether the endpoint-security alerts stop has not been checked: run `make install` and use GroundControl with the tool active, and note which alerts, if any, still fire.
- **Open spec items** (§13): the items that belonged to finance-analyzer and git-internals were dropped with those projects. The config has no `TODO(§13.x)` markers left.
- **Steps that leave background processes** holding the output pipe: the step still finishes when its main process exits; GroundControl stops reading after 2 s.
- **Text checks are seatbelts.** The blocked-command and `--remote` checks are textual, as the spec says. Masking does not catch encoded or transformed secrets.
- **A SIGKILLed worker** leaves its child process group running (see Leftover processes).
- **No desktop notifications.** Alerts reach attached tmux clients only; with none attached, a waiting approval goes unseen until a TUI opens (and expires after 30 minutes). Only the no-client path and the `#` escaping of the client message are tested; delivery to a really attached client has not been checked. An optional `terminal-notifier` setting is described but not built.
- **Output is bounded by scrollback.** A window keeps 50,000 lines; older output is gone. The window that creates the `ground-control` session keeps tmux's default (usually 2,000), and the session is recreated whenever all its windows have closed. A step whose header scrolled away still shows its remaining lines.
- **tmux plugins can persist what GroundControl doesn't.** tmux-resurrect and tmux-continuum save the `ground-control` session's window names and folders to disk, and can restore them as plain shells after a reboot. They don't save `@gc_*` options, so GroundControl ignores restored windows. With `@resurrect-capture-pane-contents` on, they would also save pane output.
- **No record after a run.** Once a run's window closes, nothing about it remains. Copy what you need, such as a restore point recorded by a migration, while the window is open.
- **Process activity.** Workers start no processes while idle: each keeps one blocked `tmux wait-for` child and is woken by the TUI's requests. Their state updates (a few per step or state change) each start one short `tmux` call. While a TUI is open, its engine starts one `tmux list-windows` per second, plus a `tmux capture-pane` per second for the run or service on screen, and re-sends a step's whole output when it changes (heavy only for a step that prints tens of thousands of lines).

## Suggested next steps

1. Run `make install`, open `groundcontrol` from a tmux popup, and use it with the security tool running.
2. Phase 2 items (§12): a key that switches to a service's or run's tmux window, live-vs-HEAD per environment read from the deploy targets, and Cloudflare live-version checks.
