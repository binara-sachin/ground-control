# GroundControl: implementation notes

Status of the implementation against [`groundcontrol-spec.md`](groundcontrol-spec.md),
the decisions taken where the spec left room, and what is still open.

## Checklist (spec §12 MVP)

### Daemon

- [x] Config loading, validation (§7.4) with line and column, hot reload that keeps the last good config (`src/config/`, `daemon/mod.rs`)
- [x] Tool check through `/bin/zsh -l` (§6.6), plus Docker and tmux reachability (`daemon/system.rs`)
- [x] Unix socket at `<state>/gc.sock`: folder `0700`, socket `0600`, peer UID check on every connection, `hello` with protocol version (`daemon/server.rs`)
- [x] Newline-delimited JSON protocol with every request in §10.6. It adds `run.diff` and `service.log`, which the History and Dashboard screens need (`protocol.rs`)
- [x] SQLite store in WAL mode with the §11 tables (`daemon/store.rs`)
- [x] Retention: step logs and diffs pruned after 90 days; service and daemon logs rotated at 10 MB with 3 files kept

### Pipelines (§6, §8)

- [x] Steps run as `/bin/zsh -l -c`, stdin `/dev/null`, own process group, stdout and stderr merged in order
- [x] Environment precedence (§6.1), including `INPUT_*`, `OUTPUT_*`, `GC_*`, `FORCE_COLOR`, `CLICOLOR_FORCE`, `TERM` and `COLUMNS=120`
- [x] Typed inputs validated before the run (pattern, choice, bool); they reach commands only as environment variables
- [x] Per-step timeout (default 15m); cancel sends `SIGTERM` to the group, then `SIGKILL` after 10 s
- [x] One run per project; a second trigger is rejected, never queued
- [x] `approve:` steps:
  - pause with `awaiting_approval` and send a notification
  - expire after 30 minutes with `cancelled (approval expired)`
  - the first answer wins; later answers get `already_decided`
  - a rejection gives the distinct status `rejected`
  - the daemon refuses protected approvals and protected runs without the confirm flag
- [x] Step outputs through `$GC_OUTPUT`, stored with the run, shown in the run view, and passed on as `OUTPUT_<KEY>`
- [x] `service:` steps share the supervisor's per-service serialisation
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
- [x] A port in use refuses the start with "port N is in use by PID X (not managed)"
- [x] Rotated service logs; masked live log events for the dashboard pane
- [x] Daemon restart: services stop with the daemon and their desired state is persisted. On startup, `kern.boottime` is compared with the last save: on the same boot they are restarted, on a new boot nothing starts
- [x] Notifications through `osascript` for failed runs, waiting approvals and crashed services; a no-op off macOS

### TUI (§10)

- [x] Chrome:
  - top bar with screen tabs, breadcrumbs, `!` attention markers, daemon uptime, config status with reload time, and the clock
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
- [x] Daemon not running, or a protocol mismatch: an explanatory screen with a key that runs `launchctl kickstart` (`-k` on mismatch); it retries automatically and never runs the daemon in-process

### Install (§5.4)

- [x] `make install` builds `~/.local/bin/groundcontrol`, writes the `dev.binara.groundcontrol` plist (`RunAtLoad`, `KeepAlive`, `GC_CONFIG`) and loads it. `git pull && make install` restarts the daemon

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
| Leftover processes | If the daemon died without stopping services, the recorded process groups are stopped before the restore. This happens only on the same boot, and only when the group leader is still a `zsh -l -c` process. |
| Health URLs | Plain `http://` only, a minimal built-in HTTP/1.1 client (no TLS dependency). Port checks try IPv4 and IPv6 loopback, because Vite may bind only `::1`. |
| Protected runs | The run dialog is the confirmation dialog. The TUI sends `confirmed: true`, and the daemon refuses protected runs and protected approvals without it. |
| Minimum terminal size (§13.9) | 100×28. |
| Theme colours (§13.9) | Amber `#e5b567` accent and warning, green `#a6cc70`, red `#f07178`, muted `#838b99`, selection `#2c3a52`, background `#1b1f27`. |
| launchd label (§13.10) | `dev.binara.groundcontrol`. The plist sets `ExitTimeOut` to 40 s, so runs and services can stop gracefully. |
| Placeholder commands (§13.1) | Written as commands that print `TODO(§13.x)` and exit 1. Unknown health ports are commented out. |

## Verification

- `cargo test`:
  - 62 unit tests: config rules, YAML positions, masking at every split point, env files, git parsing, remote normalisation, store, process groups, log rotation, protocol round-trips, terminal rendering and run-view log sync
  - 4 end-to-end tests that drive the real `groundcontrol daemon` binary over its socket:
    - pipelines, inputs, masking, outputs, failure and skip, dry run, protected confirm, approval with first-answer-wins, reject, cancel, dirty diff, history filters, sensitive log location
    - the gitignore and remote guards
    - service health checks, a port in use, `restart: never`, on-failure restarts, group stop, service logs
    - config reload keeping the last good config
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are clean.
- Manual run of the TUI against a live daemon in tmux, on Linux, with demo repositories:
  - every screen
  - a protected deploy with approval and confirm, a migrate run whose bookmark output flows into the approval message, a reject
  - starting a service group, a crashing service giving up after 5 crashes
  - config hot reload with errors, a daemon restart restoring services on the same boot
  - the terminal-too-small screen

## Not verified, and known limitations

- **Not run on macOS yet.** These have only been reviewed, not executed:
  - `launchd` install and update, and `launchctl kickstart` from the TUI
  - `osascript` notifications and `pbcopy`
  - `sysctl kern.boottime` parsing against real output (it is unit-tested with a sample)
  - the socket path under `~/Library/Application Support`
- **Open spec items** (§13): placeholder commands (§13.1), D1 Time Travel syntax and JSON shape (§13.2), the wrangler prompt behaviour (§13.3), the production environment name (§13.4), the merge method (§13.5), repository paths (§13.6) and the Asgardeo bypass (§13.7). The config marks each of them with `TODO(§13.x)`.
- **Steps that leave background processes** holding the output pipe: the step still finishes when its main process exits; GroundControl stops reading after 2 s.
- **Text checks are seatbelts.** The blocked-command and `--remote` checks are textual, as the spec says. Masking does not catch encoded or transformed secrets.
- **Approvals across a daemon restart** are cancelled (`daemon restarted`), because the run that was waiting no longer exists.

## Suggested next steps

1. Run `make install` on the Mac and walk through the same manual checks: notifications, `pbcopy`, the restore after `make install`, and the tmux popup.
2. Fill in the §13 placeholders in `groundcontrol.yaml`, starting with the finance-analyzer seed command, ports and webapp deploy, and confirm the D1 bookmark extraction against wrangler 4.15x.
3. Phase 2 items (§12): a tmux window key, live-vs-HEAD per environment from history, and Cloudflare live-version checks.
