# GroundControl redesign: no daemon, tmux-hosted

Status: implemented (build steps 1–6 below) · Date: 2026-11-06 · The spec,
[`groundcontrol-spec.md`](groundcontrol-spec.md) revision 3, now describes this
design. What remains: checking it against the security tool (build step 6).

## Why

Endpoint security tools flag the current build. Everything it does is
legitimate, but the combination matches common macOS malware heuristics:

| Behaviour today | Heuristic it matches |
|---|---|
| Hand-written LaunchAgent, `RunAtLoad` + `KeepAlive`, unsigned binary in `~/.local/bin` | Persistence (MITRE T1543.001) |
| Windowless background process repeatedly spawning `/bin/zsh -l -c <string>` | Unknown parent spawning shells |
| `osascript -e` from that process | Scripted UI and prompt abuse |
| `ps -o args`, `sysctl kern.boottime`, `lsof`, `command -v` for 9 tools, on a timer | Discovery and reconnaissance |
| Signalling process groups left over from a previous process | Process tampering |
| `.env` values read by the background process | Credential access |

The goal is not invisibility. It is to make every process have an obvious,
attributable, interactive parent: **Terminal → tmux → groundcontrol →
your command**, started by you, visible in a tmux window you can attach to.

These triggers are inferred. Check the rule name or MITRE technique in the
actual alert before and after the change. If it turns out to be a
reputation or unsigned-binary rule, this redesign won't be enough on its own,
and Developer ID signing becomes necessary.

## What stays the same

- `groundcontrol.yaml` keeps its schema. Steps and services are still shell
  strings (`&&`, `"$INPUT_PR"`, the `TODO(§13.x)` placeholders all keep
  working).
- Pipelines, inputs, approvals, outputs, dry runs, guards, dirty/unpushed
  snapshots, sensitive-project log locations, masking, history and every TUI
  screen.
- "Quitting the TUI never stops a run or a service." tmux now provides that
  guarantee instead of launchd.

## Architecture

One binary, four entry points. Only the first is typed by you. The other
three are started by GroundControl as the command of a tmux window.

| Command | Runs where | Lifetime | Job |
|---|---|---|---|
| `groundcontrol` | Your terminal or a tmux popup | While open | TUI: dashboard, launcher, viewer, approvals |
| `groundcontrol run-exec <run-id>` | tmux window `gc-<project>:run-<id>` | One pipeline run | Executes the steps, writes state and logs |
| `groundcontrol svc-exec <project> <service> <launch>` | tmux window `gc-<project>:svc-<service>` | While the service is up | Runs one service: env file, masking, logs, health, restart policy |
| `groundcontrol cli …` (later) | Anywhere | One command | Scriptable `start`, `stop`, `run`, `status` |

There is no socket, no protocol server and no background process of
GroundControl's own. **The SQLite store and log files are the shared
state.** The TUI reads them; the workers write them.

```
you ─▶ Terminal ─▶ tmux ─┬─ popup: groundcontrol (TUI) ──reads/writes──┐
                         │                                             ▼
                         ├─ gc-finance:run-412 ─ groundcontrol run-exec ─▶ state.db, logs
                         │                         └─ zsh -c "pnpm … deploy"
                         └─ gc-finance:api-local ─ groundcontrol svc-exec ─▶ state.db, logs
                                                   └─ zsh -c "pnpm … wrangler dev"
```

### tmux layout

- Default tmux server (no `-L` socket), so the windows show up in your normal
  `tmux ls` and `choose-tree`.
- One detached session per project, `gc-<project>`, created on first use with
  `tmux new-session -d`. Windows: `svc-<service>` per running service and
  `run-<id>` per active run. The prefixes keep the two apart whatever a
  service is called. Ids are escaped for tmux (`_` → `__`, `.` → `_d`).
- Windows set `remain-on-exit on`, so a crashed service's last output stays
  visible until it's restarted or closed.
- The TUI gets a key (`t`) that attaches or switches to the selected
  service's or run's window. This was already in Phase 2.
- tmux becomes a hard requirement. The TUI checks for it at startup and
  explains what to do if it's missing.

### Shell invocation

- Workers run steps and services as `/bin/zsh -c <command>`, **not** `-l`.
  tmux windows already inherit your interactive environment, so the
  `.zprofile` PATH requirement in the README goes away.
- Each step and service shell starts in its own session (`setsid`), so it
  has no controlling terminal even though its worker runs in a pane: it
  can't read the pane's size or prompt on `/dev/tty`. Stdin stays
  `/dev/null`, as before.
- Everything GroundControl runs for itself uses argv with no shell: `git`,
  `tmux`, and `docker info` on the Health screen.

### Pipeline runs

1. The TUI validates inputs and runs preflight (guards, dirty snapshot, dry
   run). This is the same code as `runner::start` today.
2. It inserts the run row, then `tmux new-window -d -t gc-<project>
   -n run-<id> 'groundcontrol run-exec <id>'`.
3. `run-exec` takes the project lock (below), reads the run and its config
   snapshot from SQLite, and executes `runner::execute` as it does today. It
   prints the step output to its own pane (masked) and to the step log file.
4. **Approvals** stay in the TUI run view, so the two-key confirm for
   protected runs is unchanged. `run-exec` writes the `awaiting_approval`
   row and polls it every 500 ms. The TUI writes the decision. The 30-minute
   expiry and first-answer-wins logic move from memory into SQLite (a
   conditional `UPDATE … WHERE status = 'waiting'`).
5. **Cancel**: the TUI sets `runs.cancel_requested`. `run-exec` polls it and
   runs the existing SIGTERM → 10 s → SIGKILL path on its **own child**. If
   the window doesn't exit within 15 s, the TUI runs `tmux kill-window`.
   GroundControl never signals a PID it didn't spawn itself.
6. When the run ends, the window closes after 5 s (configurable). Failed
   runs keep their window until you close it.

Interrupted runs (the window was killed or the Mac rebooted) are detected
when the project lock is free but the run row is still active. They're
marked `interrupted`, as `recover_interrupted` does today.

### Services

`svc-exec` is today's per-service supervisor slot, moved into a foreground
process:

- Loads `env_file`, spawns `zsh -c <start>` in its own process group, and
  writes the merged output through the masker to both the pane and the
  rotating log file. Your scrollback therefore never shows unmasked secrets.
  Sensitive projects keep their logs in `<repo>/.groundcontrol/`.
- Runs the health check (HTTP 200 or TCP port) every 5 s and writes the
  status to `service_state`.
- Handles `restart: on-failure` with the existing back-off, and the "crashed
  after 5 crashes in 5 minutes" rule (`services::policy`, a pure function).
  After giving up it rings the bell and exits. The dead pane
  (`remain-on-exit`) keeps the last output until the next start replaces it.
- Ownership: the worker takes the service lock before writing anything, is
  the only writer of the service's row while it runs, and releases the lock
  last, after the process group is gone and the final row is written. A
  worker that loses the lock race closes its own pane, so window names stay
  unique. Each launch carries a token, so a starter never mistakes an older
  row for its own launch.
- Stop: the TUI sets `service_state.stop_requested`, which the worker polls.
  `send-keys C-c` isn't used for this, because tmux keeps keys for itself
  while a pane is in copy mode. C-c in the window still works by hand.
  `tmux kill-window` is the fallback after 15 s: the worker handles SIGHUP
  (and SIGTERM) the same way and still ends the child's process group.
- `depends_on` and groups: `services::control(ctx, config, project, target,
  action)`, called from the TUI or from a `service:` step in `run-exec`,
  opens the dependency windows in order and waits for each one to report
  `healthy` in SQLite before starting the next. It stops them in reverse
  order.
- A port already in use refuses the start with "port N is in use (not
  started by GroundControl)". The `lsof` call that named the PID is removed.

**Known limitation:** if a worker is SIGKILLed or panics, its child's process
group keeps running. The status then shows "worker exited unexpectedly", and
the next start of a service with a health port refuses with "port N is in use
(not started by GroundControl)", which is misleading in this case. Stop the
leftover process by hand.

**Removed on purpose:** the restore after a daemon restart, the
`kern.boottime` comparison, adopting and killing orphans (`looks_like_ours`,
`terminate_orphan_group`), and stored PIDs as the source of truth. After a
reboot nothing comes back until you start it. That is the point.

### Locking and concurrency

Several TUIs can be open at once (two popups, for example), so in-memory
maps can't enforce "one run per project" any more.

- `run-exec` holds `flock(<state>/locks/<project>/run.lock)` for its whole
  lifetime. `svc-exec` holds `<state>/locks/<project>/services/<service>.lock`.
  Separate folders keep project `a-b` from colliding with project `a`'s
  service `b`.
- The kernel releases a lock when its process dies, so there are no stale
  locks and no PID checks. "Is it alive?" is a non-blocking `flock` probe.
- The TUI tries the lock before inserting a run. `run-exec` takes it again
  and exits with `already running` if another process beat it.
- SQLite stays in WAL mode. Each process opens its own connection with a
  `busy_timeout`.

### Live updates in the TUI

The socket push events are replaced with polling:

- `service_state`, `runs`, `steps` and `approvals`: polled every 250 ms with
  one cheap query on an `updated_at` index.
- Output: tail the log file from the stored offset. The run view's
  offset-based attach logic already does this.
- Git status: polled by the TUI every 10 s while it's open, plus on focus and
  after a run. The commands are argv `git`, as today.

### Notifications

`osascript` is removed. Instead:

- The TUI's attention markers (`!`) and approval banner are unchanged.
- Workers ring the tmux bell (`\a` in their pane). With `monitor-bell on`,
  tmux marks the window in your status line and can show a message.
- Not built yet: an optional `notify: terminal-notifier` config setting, off
  by default, for desktop notifications through that signed Homebrew app,
  so that your unsigned binary never scripts the UI. Add it only if the
  tmux bell turns out not to be enough.

### Health screen

- Tool check: resolve each tool on `PATH` in-process (stat each directory).
  No subprocess.
- Docker and tmux reachability: checked only while the Health screen is
  open, not on a timer.
- No daemon version or uptime. Show the TUI version, the tmux server version
  and the config reload status instead.

### Housekeeping

- Config reload: the TUI watches the file. Workers snapshot the config that
  was active when they started, so editing the file never changes a run in
  progress.
- Retention (90-day logs, 10 MB rotation): runs when the TUI starts, at most
  once a day.
- Install: `make install` only copies the binary to `~/.local/bin`. No plist,
  no `launchctl`. `make uninstall` removes the binary. Add a one-time
  cleanup target that boots out and deletes the old
  `dev.binara.groundcontrol` agent.

## Code changes

| Module | Change |
|---|---|
| `daemon/server.rs`, `client.rs` | Delete |
| `daemon/mod.rs` (`Daemon`, pollers, `run`) | Delete. Its config, git and retention parts become an in-process `Engine` used by the TUI |
| `daemon/runner.rs` | Split. Preflight, inputs and dry run stay in a library the TUI calls. `execute` becomes the body of `run-exec`. Approval and cancel switch from channels to SQLite polling |
| `daemon/supervisor.rs` | The slot logic becomes the body of `svc-exec`. `start_order` and the start/stop orchestration go to `services.rs`. `restore`, `shutdown`, `port_owner` and the orphan code are deleted |
| `daemon/process.rs` | Drop `-l`, `looks_like_ours`, `terminate_orphan_group` and `shell_output`. Keep `spawn_shell` and `terminate` |
| `daemon/system.rs` | Delete `boot_time` and `notify`. Rewrite the tool check in-process |
| `daemon/store.rs` | Move to `store.rs`. Add `runs.cancel_requested`, `updated_at` columns, and the conditional approval update. Done for services (v2): `healthy`, `message`, `launch`, `updated_at` and `stop_requested`. `pid`/`pgid` stay for display only; nothing signals them from outside the worker |
| New `tmux.rs` | `ensure_session`, `new_window`, `send_keys`, `kill_window`, `list_windows` (`#{window_name} #{pane_dead} #{pane_dead_status}`) |
| New `lock.rs` | `flock` acquire and probe |
| `protocol.rs` | Keep the data types (`RunSummary`, `ServiceStatus`, …). Drop the request/response envelope and `PROTOCOL_VERSION` |
| `tui/app.rs` | Replace the `Client` with `Engine` calls and a poll loop that emits the same `Msg` values. Delete the "daemon not running" screen and the `launchctl kickstart` key (line 706). Add the `t` attach key |
| `main.rs` | Add `run-exec` and `svc-exec`. Remove `daemon` |
| `Makefile`, `packaging/` | Remove the plist and launchctl targets |
| `tests/daemon_e2e.rs` | Becomes `tests/workers_e2e.rs`. It drives `run-exec` and `svc-exec` directly, plus a tmux-backed test that's skipped when tmux isn't installed |

Masking, env files, config, the YAML tree, git helpers and the screens keep
working with little or no change.

## Spec sections to revise

- §1 Summary and §5.1–5.2: two parts (daemon and client) become TUI plus
  tmux-hosted workers. Redraw the diagram.
- §5.4 Install and update: binary only, no launchd.
- §6.1 Steps: `/bin/zsh -c`, run inside tmux. Drop the `.zprofile` note.
- §6.2 Concurrency: `flock` per project and per service.
- §6.3 Approvals: decisions through SQLite. Same UI rules.
- §6.5 Services: `svc-exec` per window. Restore-after-restart removed.
- §6.6 Startup health: on-demand checks, no daemon uptime.
- §10.3 Screens: remove the daemon-not-running screen, add the attach key.
- §10.5 Notifications: tmux bell, optional terminal-notifier.
- §10.6 Client protocol: removed.
- §11 Storage: new columns, lock folder, no socket.

## Build order

1. `tmux.rs` and `lock.rs`, with tests.
2. `svc-exec`, moved from the supervisor slot, and `services::start` for
   dependency order. Port the service parts of the e2e test.
3. `run-exec`, with approvals and cancel through SQLite. Port the pipeline
   parts of the e2e test.
4. The TUI `Engine` and poll loop. Delete the client, server and daemon.
   **Cutover:** the installed LaunchAgent must be booted out before any
   worker runs against the real state folder. On a same-boot restart, the
   old daemon's `restore()` would SIGTERM a worker's child (`/bin/zsh -c …`
   passes `looks_like_ours`) and start the service itself, and
   `clear_desired_services` would overwrite rows that workers own. Until
   then, only the tests launch `svc-exec`, against temporary state.
5. Health screen, notifications, Makefile cleanup and the old-agent
   removal target.
6. Spec and README revision. Manual pass on the Mac with the security tool
   running, recording which alerts, if any, still fire.

## Optional, separate from this redesign

Move `deploy-*`, `migrate-*` and `merge-pr` to GitHub Actions and trigger them
with `gh workflow run`. Cloudflare credentials then stay out of the laptop
entirely, and GroundControl only records and links the workflow run.
