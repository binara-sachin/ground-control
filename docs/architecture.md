# GroundControl architecture

How GroundControl runs on the Mac, and why it is built this way. The spec,
[`groundcontrol-spec.md`](groundcontrol-spec.md), describes the behaviour; this
document describes the processes, the shared state and the design rules behind
them.

## Why there is no daemon

Earlier versions ran a `launchd` daemon. Endpoint security tools flagged it as
malicious software. Everything it did was legitimate, but the combination
matched common macOS malware heuristics: a hand-written LaunchAgent with
`RunAtLoad` and `KeepAlive` pointing at an unsigned binary (persistence), a
windowless process repeatedly spawning `zsh -l -c <string>`, `osascript`,
`ps`/`lsof`/`sysctl` on a timer (discovery), and signalling processes left over
from an earlier run.

The design rule is not to hide anything. **Every process has an obvious,
attributable, interactive parent: Terminal → tmux → `groundcontrol` → your
command.** You start it, and you can see it in a tmux window and attach to it.
Nothing starts at login and nothing runs in the background of its own.

Those heuristics were inferred, not read from an alert. If alerts continue,
check the rule name or MITRE technique in the alert itself. If it is a
reputation or unsigned-binary rule, the remaining fix is Developer ID signing.

## Processes

One binary. Only the first command is typed by you; GroundControl starts the
other two as the command of a tmux window.

| Command | Runs where | Lifetime | Job |
|---|---|---|---|
| `groundcontrol` | Your terminal or a tmux popup | While open | The TUI and its engine (`engine.rs`) |
| `groundcontrol run-exec <run-id>` | Window `gc-<project>:run-<id>` | One pipeline run | Executes the run (`runs/worker.rs`) |
| `groundcontrol svc-exec <project> <service> <launch>` | Window `gc-<project>:svc-<service>` | While the service is up | Runs one service (`services/worker.rs`) |

There is no socket and no network listener. **The SQLite store, the log files
and the lock files in the state folder are the shared state.** The workers
write them; the TUI reads them and writes requests (decisions, cancel and stop
requests) that the workers poll.

```
you ─▶ Terminal ─▶ tmux ─┬─ popup: groundcontrol (TUI + engine) ──reads/writes──┐
                         │                                                      ▼
                         ├─ gc-finance:run-412   groundcontrol run-exec ──▶ state.db, logs, locks
                         │                         └─ zsh -c "pnpm … deploy"
                         └─ gc-finance:svc-api   groundcontrol svc-exec ──▶ state.db, logs, locks
                                                   └─ zsh -c "pnpm … wrangler dev"
```

## tmux layout

- The user's default tmux server, so the windows show up in `tmux ls` and
  `choose-tree`. Inside a pane, `tmux` reaches that server through `$TMUX`.
- One detached session per project, `gc-<project>`, created on first use.
  Windows are `svc-<service>` and `run-<id>`; the prefixes keep a service from
  ever being mistaken for a run. Ids are escaped for tmux (`_` → `__`,
  `.` → `_d`), which keeps the mapping one-to-one.
- Commands are passed as argv after `--`, so tmux executes them without a
  shell. Targets always use exact matching (`=session:=window`).
- Windows set `remain-on-exit`, so a crashed service or failed run keeps its
  last output on screen.
- tmux is required. The Health screen shows whether it is reachable, and a
  start fails with tmux's error if it is not.

## Shell invocation

- Steps and services run as `/bin/zsh -c <command>`, not a login shell. tmux
  windows inherit the tmux server's environment, which is your interactive
  shell's.
- Each step and service starts in its own session (`setsid`): its own process
  group, which the worker signals as a whole, and no controlling terminal even
  though the worker runs in a pane. Stdin is `/dev/null`. Nothing can wait for
  input or prompt on `/dev/tty`.
- Everything GroundControl runs for itself uses argv with no shell: `git`,
  `tmux`, and `docker info` while the Health screen is open.

## Pipeline runs

1. The TUI (`runs::start`) validates the environment and inputs, checks the
   guards (disabled project, repository remote, gitignored `.groundcontrol/`
   for sensitive projects, a run already in progress), takes the dirty and
   unpushed snapshot, records the run, saves the diff of a dirty tree, and
   opens `run-<id>` running `groundcontrol run-exec <id>`.
2. The worker takes the project's run lock (below) and runs the steps with
   the config as it is when it starts. Output goes through the masker to the
   pane and to the step log, which the TUI follows.
3. **Approvals.** The worker records the approval and polls it every 500 ms.
   The TUI records the decision with a conditional update, so the first
   answer wins. Protected approvals need the confirm keypress. An approval
   expires after 30 minutes.
4. **Cancel.** The TUI sets `runs.cancel_requested`, which the worker polls on
   its own schedule, so a step that prints constantly can't delay it. `C-c` in
   the window, SIGTERM and closing the window cancel too. The worker sends
   SIGTERM to the step's process group, then SIGKILL after 10 seconds. It
   never signals a process it did not start.
5. A succeeded run's window closes itself after 5 seconds. A failed run's
   window stays for reading until the project's next run replaces it.
6. **Interrupted runs.** The engine ends a run that is still active when its
   project's lock is free and its window is gone (`runs::reap`), with the
   reason "worker exited unexpectedly".

## Services

- `services::control` starts a service or group in dependency order. It
  starts each service's dependencies first and waits for them to be healthy.
  Groups stop in reverse order. A `service:` step in a run uses the same
  function.
- The worker loads `env_file`, refuses to start if the health port is
  already in use ("port N is in use (not started by GroundControl)"), and
  writes the masked output to the pane and the rotated log. Sensitive
  projects keep their logs in `<repo>/.groundcontrol/`.
- It runs the health check (HTTP 200 or TCP port) during start (30-second
  limit) and every 5 seconds after that. It applies `restart: on-failure`
  with back-off and the "crashed after 5 crashes in 5 minutes" rule
  (`services::policy`, a pure function).
- **Ownership.** The worker takes the service lock before it writes anything.
  While it holds the lock it is the only writer of the service's row, and it
  releases the lock last, after its process group is gone and the final row
  is written. A worker that loses the lock race closes its own pane, so window
  names stay unique. Each launch carries a token, so a starter never mistakes
  an older row for its own launch.
- **Stop.** The TUI sets `service_state.stop_requested`, which the worker
  polls. This works even while the pane is in tmux copy mode, where `C-c`
  would not reach the worker. `C-c` in the window works by hand. Closing the
  window (SIGHUP) is the fallback after 15 seconds; the worker still ends its
  process group.
- After a reboot or logout the tmux server is gone, and nothing starts until
  you start it.

**Known limitation.** If a worker is SIGKILLed or panics, its child's process
group keeps running. The status shows "worker exited unexpectedly", and the
next start of a service with a health port refuses because the port is in
use. Stop the leftover process by hand.

## Locking

- `run-exec` holds `<state>/locks/<project>/run.lock` for the whole run.
  `svc-exec` holds `<state>/locks/<project>/services/<service>.lock`. The
  separate folders keep project `a-b` from colliding with project `a`'s
  service `b`.
- The locks are `flock` locks. The kernel releases a lock when its holder
  exits, however it exits, so a lock never goes stale and no PID is ever
  checked. "Is it running?" is a non-blocking probe.
- Several TUIs can be open at once. The run lock is what makes "one run per
  project" hold between them.
- Every process opens its own SQLite connection, with the busy timeout set
  before WAL mode.

## The TUI

The engine runs inside the TUI while it is open:

- It answers the TUI's requests (`protocol::Request`) in process.
- It polls the store every 250 ms and turns changes into events: runs,
  steps, approvals, outputs, the busy run per project, and services (every
  500 ms).
- It follows the step logs of runs on screen from the start, and the
  followed service log from its current end. The run view drops output it
  already has.
- It polls git status every 30 seconds and after each run. It never fetches.
- It watches `groundcontrol.yaml` and reloads it, keeping the last good
  config when the new one is invalid.
- It ends interrupted runs every 5 seconds, and prunes logs and diffs older
  than 90 days once when it opens.
- Tools are found on `PATH` without running anything. tmux is asked for its
  version when the TUI opens and while the Health screen is open. Docker is
  checked only while the Health screen is open.

## Alerts

When an approval is waiting, a run fails or a service gives up, the worker:

- shows a message in the status line of every attached tmux client
  (`display-message -c`), because a bell in a detached `gc-*` session is only
  flagged inside that session;
- rings the bell in its own pane.

With no tmux client attached, nobody is told until a TUI opens. GroundControl
never runs `osascript`. A possible addition is an optional, off-by-default
setting to send desktop notifications through `terminal-notifier`, a signed
Homebrew app. It would be worth adding if approvals get missed while no tmux
client is attached.

## Install

`make install` copies the binary to `~/.local/bin` and links the
repository's `groundcontrol.yaml` into the state folder. It registers nothing
with `launchd`. `make uninstall` removes both and keeps history and logs.

## A possible next step

Move `deploy-*`, `migrate-*` and `merge-pr` to GitHub Actions and trigger them
with `gh workflow run`. Cloudflare credentials would then stay off the laptop,
and GroundControl would only record and link the workflow run.
