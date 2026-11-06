# GroundControl: implementation notes

Status of the implementation against [`groundcontrol-spec.md`](groundcontrol-spec.md),
the decisions taken where the spec left room, and what is still open.

## Checklist (spec §12 MVP)

### Architecture (revision 3: no daemon)

- [x] `groundcontrol` is the TUI with an in-process engine (`engine.rs`): config loading, validation (§7.4) with line and column, hot reload that keeps the last good config, git polling, live updates by polling the store and log files, and ending runs whose worker is gone (`runs::reap`)
- [x] Runs execute in `groundcontrol run-exec <id>` in the tmux window `gc-<project>:run-<id>` (`runs/worker.rs`); services in `groundcontrol svc-exec` in `gc-<project>:svc-<service>` (`services/worker.rs`)
- [x] tmux control with argv commands and exact `=session:=window` targets; ids escaped for tmux (`tmux.rs`)
- [x] `flock` locks per project run and per service, released by the kernel when the holder exits (`lock.rs`)
- [x] SQLite store opened by every process (busy timeout set first, WAL); schema v2 adds `runs.cancel_requested` and the worker-owned service columns, with a migration that is safe when two processes open the store together (`store.rs`)
- [x] Tool check resolves on `PATH` in-process; tmux checked at TUI start and on the Health screen; Docker only while the Health screen is open
- [x] No socket, no protocol envelope, no `launchd`, no `osascript`, `ps`, `lsof` or `sysctl`

### Pipelines (§6, §8)

- [x] Steps run as `/bin/zsh -c` in a new session (own process group, no controlling terminal), stdin `/dev/null`, stdout and stderr merged in order
- [x] Environment precedence (§6.1), including `INPUT_*`, `OUTPUT_*`, `GC_*`, `FORCE_COLOR`, `CLICOLOR_FORCE`, `TERM` and `COLUMNS=120`
- [x] Typed inputs validated before the run (pattern, choice, bool); they reach commands only as environment variables
- [x] Per-step timeout (default 15m); cancel sends `SIGTERM` to the group, then `SIGKILL` after 10 s
- [x] One run per project, enforced by the project's run lock; a second trigger is rejected, never queued
- [x] `approve:` steps:
  - pause with `awaiting_approval` and ring the tmux bell; the worker polls the store for the decision
  - expire after 30 minutes with `cancelled (approval expired)`
  - the first answer wins; later answers get `already_decided`
  - a rejection gives the distinct status `rejected`
  - protected approvals and protected runs are refused without the confirm flag
- [x] Step outputs through `$GC_OUTPUT`, stored with the run, shown in the run view, and passed on as `OUTPUT_<KEY>`
- [x] `service:` steps start and stop services through the same `services::control` as the TUI
- [x] Dry run: resolved commands, working directory, env var *names*, inputs, approvals, and the blockers a real run would hit
- [x] Repository guard (`origin` normalised across SSH and HTTPS forms), checked before every run and service start
- [x] Dirty and unpushed snapshot at run start; dirty runs save `git diff HEAD` plus the list of untracked files
- [x] Sensitive projects:
  - logs and diffs go to `<repo>/.groundcontrol/`
  - the run is refused unless `git check-ignore` confirms that folder is ignored
  - no output is stored centrally
- [x] Secret masking of `env_file` values (6 or more characters), streaming-safe across chunk boundaries (`mask.rs`, unit-tested)

### Services (§6.5)

- [x] HTTP-200 and TCP-port health checks; the 30 s start deadline; without a check, alive counts as healthy
- [x] Starting a service starts its dependencies first and waits for them to be healthy. Groups start in dependency order and stop in reverse
- [x] `restart: on-failure`: back-off from 1 s doubling to 60 s; after 5 crashes in 5 minutes the service is marked **crashed** and a notification fires. `restart: never`: exit 0 shows as stopped, non-zero as crashed
- [x] A port in use refuses the start with "port N is in use (not started by GroundControl)"
- [x] Rotated service logs; masked live log events for the dashboard pane
- [x] Stop through a stop-request flag the worker polls (works while the pane is in copy mode); `C-c` and closing the window stop it too
- [x] A service's worker is the only writer of its row while it holds the lock, and releases the lock after its process group is gone; each launch carries a token
- [x] Nothing is restored after a reboot or logout
- [x] Notifications are the tmux bell, from the worker's pane

### TUI (§10)

- [x] Chrome:
  - top bar with screen tabs, breadcrumbs, `!` attention markers, config status with reload time, and the clock
  - bottom key bar
- [x] Theme tokens in one place (`tui/theme.rs`); every status carries a word or symbol, never colour alone
- [x] Layout safety:
  - fixed or flexible columns measured in display cells (`unicode-width`), truncated with `…`
  - pane titles clipped to the pane
  - a "terminal too small" screen below 100×28
- [x] Screens: Dashboard, Project, Run dialog, Run view, History, Health, plus help, log/diff viewer, dry-run viewer and confirm prompts
- [x] Output pane:
  - `vt100` parser, 120 columns with horizontal scroll
  - follow mode and scrollback
  - when attaching, reads the log from the start, then follows live chunks; offsets prevent gaps and duplicates
- [x] Approval only in the run view; the dashboard banner offers only "review". A protected approval takes two keys; a rejection takes one
- [x] State folder or database cannot be opened: an explanatory screen that retries automatically

### Install (§5.4)

- [x] `make install` builds `~/.local/bin/groundcontrol`, links `groundcontrol.yaml` into the state folder, and removes the old `dev.binara.groundcontrol` launchd agent if present. Nothing starts at login

### Projects (§7.2, §7.3)

- [x] `groundcontrol.yaml` holds finance-analyzer (including both rollback pipelines), git-internals (services only) and tmux-ui-manager. It passes validation; a unit test keeps it that way

## Decisions where the spec left room

| Topic | Decision |
|---|---|
| YAML crate (§13.8) | `yaml-rust2`'s event parser builds a small tree that carries positions (`src/yaml.rs`). Typed config is built from it, so every schema and validation error has a line and column. It also catches duplicate keys and unknown keys, so typos don't pass silently. |
| Destructive warnings | The spec asks the run dialog to show them but gives no config field. An optional pipeline field `warning:` was added. |
| `env_file` path | Resolved relative to the step's or service's `cwd`, which matches the Git Internals `.env` example. |
| "HEAD isn't on origin" | `git rev-list --count HEAD --not --remotes=origin` > 0. The count is stored with the run. |
| Run IDs | SQLite autoincrement integers, matching the mockups (`#412`). |
| Service start semantics | Starting a single service starts its `depends_on` chain first. A process that exits during its first start is reported as a failed start, not fed into the restart loop. Health is re-checked every 5 s while a service runs (`unhealthy` is shown, not restarted). |
| Removed services | When a config reload removes a service that is running, it is stopped. |
| Leftover processes | Never adopted or signalled. A worker that is SIGKILLed leaves its child running; the status says "worker exited unexpectedly" and the next start of a service with a health port reports the port as in use. |
| Health URLs | Plain `http://` only, a minimal built-in HTTP/1.1 client (no TLS dependency). Port checks try IPv4 and IPv6 loopback, because Vite may bind only `::1`. |
| Protected runs | The run dialog is the confirmation dialog. The TUI sends `confirmed: true`; protected runs and protected approvals are refused without it. |
| Minimum terminal size (§13.9) | 100×28. |
| Theme colours (§13.9) | Amber `#e5b567` accent and warning, green `#a6cc70`, red `#f07178`, muted `#838b99`, selection `#2c3a52`, background `#1b1f27`. |
| tmux names | Session `gc-<project>`; windows `svc-<service>` and `run-<id>`, so a service can never be mistaken for a run. `_` → `__` and `.` → `_d` in tmux names. |
| Placeholder commands (§13.1) | Written as commands that print `TODO(§13.x)` and exit 1. Unknown health ports are commented out. |

## Verification

- `cargo test` (on macOS with tmux 3.8, and on Linux):
  - unit tests: config rules, YAML positions, masking at every split point, env files, git parsing, remote normalisation, store (including concurrent migration, cancel and interrupt), locks, tmux escaping and parsing, the restart policy, health checks, process sessions and groups, log rotation, terminal rendering and run-view log sync
  - `tmux_e2e`: windows, argv commands, exact targets, dead panes, ids with `.` and `_`
  - `services_e2e`: port in use, health checks, failed start, on-failure restarts, group stop, masking in pane and log, stop in copy mode, `C-c`, closing the window, a duplicate worker closing itself
  - `runs_e2e`: inputs, env precedence, masking, outputs, dry run, failure and skip, protected confirm, approval with first-answer-wins, reject, cancel while a step prints constantly, closing the window, `service:` steps, dirty diff, history filter, sensitive log location, guards, reaping a run whose worker is gone
  - `engine_e2e`: projects, run and log events, approvals through the engine, service events and logs, health, config reload keeping the last good config, and the real TUI rendering its dashboard in tmux
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are clean.
- Manual pass on macOS: the TUI in a tmux session started a service from the dashboard, ran a pipeline from the run dialog and showed its output, survived quitting with the service still serving, and stopped the service.

## Not verified, and known limitations

- **The security tool.** Whether the endpoint-security alerts stop has not been checked: run `make install` and use GroundControl with the tool active, and note which alerts, if any, still fire.
- **Open spec items** (§13): placeholder commands (§13.1), D1 Time Travel syntax and JSON shape (§13.2), the wrangler prompt behaviour (§13.3), the production environment name (§13.4), the merge method (§13.5), repository paths (§13.6) and the Asgardeo bypass (§13.7). The config marks each of them with `TODO(§13.x)`.
- **Steps that leave background processes** holding the output pipe: the step still finishes when its main process exits; GroundControl stops reading after 2 s.
- **Text checks are seatbelts.** The blocked-command and `--remote` checks are textual, as the spec says. Masking does not catch encoded or transformed secrets.
- **A SIGKILLed worker** leaves its child process group running (see Leftover processes).
- **No desktop notifications.** The tmux bell is the only signal; an optional `terminal-notifier` setting is described but not built.

## Suggested next steps

1. Run `make install`, open `groundcontrol` from a tmux popup, and use it with the security tool running.
2. Fill in the §13 placeholders in `groundcontrol.yaml`, starting with the finance-analyzer seed command, ports and webapp deploy, and confirm the D1 bookmark extraction against wrangler 4.15x.
3. Phase 2 items (§12): a key that switches to a service's or run's tmux window, live-vs-HEAD per environment from history, and Cloudflare live-version checks.
