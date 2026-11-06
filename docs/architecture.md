# GroundControl architecture

How GroundControl runs on the Mac, and why it is built this way. The spec,
[`groundcontrol-spec.md`](groundcontrol-spec.md), describes the behaviour; this
document describes the processes, where state lives, and the design rules
behind them.

## Design rules

1. **Every process has an obvious, interactive parent:** Terminal → tmux →
   `groundcontrol` → your command. You start it, and you can see it in a tmux
   window and attach to it. Nothing starts at login and nothing runs in the
   background on its own.
2. **GroundControl persists nothing.** No database, no log files, no history,
   no lock files. Its only file is your `groundcontrol.yaml`. Everything
   GroundControl knows lives in the tmux server's memory and goes away with
   it.

### Why there is no daemon

Earlier versions ran a `launchd` daemon. Endpoint security tools flagged it as
malicious software. Everything it did was legitimate, but the combination
matched common macOS malware heuristics: a hand-written LaunchAgent with
`RunAtLoad` and `KeepAlive` pointing at an unsigned binary (persistence), a
windowless process repeatedly spawning `zsh -l -c <string>`, `osascript`, and
`ps`, `lsof` and `sysctl` on a timer (discovery).

Those heuristics were inferred, not read from an alert. If alerts continue,
check the rule name or MITRE technique in the alert itself. If it is a
reputation or unsigned-binary rule, the remaining fix is Developer ID signing.

## Processes

One binary. Only the first command is typed by you; GroundControl starts the
other two as the command of a tmux window.

| Command | Runs where | Lifetime | Job |
|---|---|---|---|
| `groundcontrol` | Your terminal or a tmux popup | While open | The TUI and its engine (`engine.rs`) |
| `groundcontrol run-exec` | A `<project>/<pipeline>` window | One pipeline run | Executes the run (`runs/worker.rs`) |
| `groundcontrol svc-exec <project> <service>` | A `<project>/<service>` window | While the service is up | Runs one service (`services/worker.rs`) |

```
you ─▶ Terminal ─▶ tmux ─┬─ popup: groundcontrol (TUI + engine) ──reads/writes window options──┐
                         │                                                                      ▼
                         └─ session "ground-control"
                              ├─ finance/deploy    groundcontrol run-exec  ─▶ @gc_* options, pane
                              │                      └─ zsh -c "pnpm … deploy"
                              └─ finance/api       groundcontrol svc-exec  ─▶ @gc_* options, pane
                                                     └─ zsh -c "pnpm … wrangler dev"
```

## tmux

- Every run and service is a window of one session, **`ground-control`**, on
  the user's default tmux server. If the session exists, a new window joins
  it; if not, the session is created with that window.
- Windows are named `<project>/<service>` or `<project>/<pipeline>` and
  addressed by their tmux window id (`@12`). **A run's id is its window's id
  number.** Window ids restart from 0 when the tmux server restarts.
- Commands are passed to tmux as argv after `--`, so tmux executes them
  without a shell.
- Windows set `remain-on-exit`, so a crashed service or failed run keeps its
  last output on screen.
- The session's `history-limit` is raised to 50,000 lines, since a window's
  scrollback is the only copy of its output. The window that creates the
  session keeps tmux's default (usually 2,000 lines), because it exists before
  the limit can be raised. The session is created again whenever all its
  windows have closed, so this happens regularly.
- tmux is required. The Health screen shows whether it is reachable, and a
  start fails with tmux's error if it is not.

## Where state lives

All of it is in tmux:

- **Window options.** Each window carries `@gc_*` options:
  - what it is: `kind` (`svc` or `run`), `project`, `service` or `pipeline`
  - for a run: environment, inputs, branch, commit, dirty and unpushed flags
  - the live status the worker publishes
  - the requests the TUI writes back: `@gc_stop`, `@gc_cancel`, `@gc_decision`

  Values are percent-encoded where tmux would alter them: control
  characters, non-ASCII, and the `|` and `,` field separators. One
  `list-windows` call returns every window with all its options.
- **Pane scrollback.** It holds the output, masked by the worker before it
  is written. Before each step, a run's worker prints a header line
  (`── step 2/3: Deploy ──`); the engine splits the captured pane at those
  headers to show each step's output. When a header has scrolled out of the
  scrollback, the lines above the first header still visible go to the step
  before it.
- **Waking a worker.** When the TUI writes a request, it signals the
  window's `tmux wait-for` channel in the same tmux command. Each worker keeps
  one `tmux wait-for` child blocked on that channel for as long as nothing
  happens, so an idle worker starts no processes. tmux remembers a signal sent
  while nobody waits, so no request is lost.
- **Window liveness.** A live (not dead) window means its worker is running.
  This replaces locks: the worker keeps its window alive until its child's
  process group is gone.

tmux plugins such as tmux-resurrect and tmux-continuum can save the session's window names and folders (and, if configured, pane contents) to disk. That is outside GroundControl's control; restored windows have no `@gc_*` options and are ignored.

The only file a run touches is its `$GC_OUTPUT` scratch file. It lives in the
system temp folder for the length of the run and is deleted when the run ends.

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

1. The TUI (`runs::start`) checks the environment and inputs, the guards
   (disabled project, repository remote) and that the project has no run in
   progress. It takes the dirty and unpushed snapshot, closes the project's
   earlier failed run windows, and opens the run's window with all of that in
   its options.
2. The worker reads its parameters from its window and runs the steps, using
   the config as it is when the worker starts. Output goes through the
   masker to the pane.
3. **Approvals.** The worker publishes the approval in `@gc_approval` and
   waits. The TUI writes the decision to `@gc_decision` once, wakes the
   worker, and refuses a second answer, so the first answer wins. Protected approvals
   need the confirm keypress. An approval expires after 30 minutes.
4. **Cancel.** The TUI sets `@gc_cancel` and wakes the worker. The wait
   carries on across the worker's output handling, so a step that prints
   constantly can't make it miss the wake. `C-c` in the
   window, SIGTERM and closing the window cancel too. The worker sends
   SIGTERM to the step's process group, then SIGKILL after 10 seconds. It
   never signals a process it did not start.
5. A succeeded run's window closes itself after 5 seconds, and the run is
   gone. A failed run's window stays for reading until the project's next run
   replaces it.
6. **One run per project.** A start is refused while a live window holds an
   active run of the project. If two starts race, the newer worker sees the
   older one and ends its own run.
7. A run whose worker died without publishing an end (its window is dead
   with the run still active) shows as cancelled, "worker exited
   unexpectedly".

## Services

- `services::control` starts a service or group in dependency order. It
  starts each service's dependencies first and waits for them to be healthy.
  Groups stop in reverse order. A `service:` step in a run uses the same
  function.
- The worker loads `env_file`, refuses to start if the health port is
  already in use ("port N is in use (not started by GroundControl)"), and
  writes the masked output to its pane.
- It runs the health check (HTTP 200 or TCP port) during start (30-second
  limit) and every 5 seconds after that. It applies `restart: on-failure`
  with back-off and the "crashed after 5 crashes in 5 minutes" rule
  (`services::policy`, a pure function).
- **One worker per service.** The oldest live window of a service owns it.
  A worker that finds an older live window closes its own.
- **Stop.** The TUI sets `@gc_stop` and wakes the worker. This works even
  while the pane is in tmux copy mode, where `C-c` would not reach the
  worker. `C-c` in the window works by hand. Closing the window (SIGHUP) is
  the fallback after 15 seconds; the worker still ends its process group.
- After a reboot or logout the tmux server is gone, and nothing starts until
  you start it.

**Known limitation.** If a worker is SIGKILLed or panics, its child's process
group keeps running. The status shows "worker exited unexpectedly", and the
next start of a service with a health port refuses because the port is in
use. Stop the leftover process by hand.

## The TUI

The engine runs inside the TUI while it is open:

- It answers the TUI's requests (`protocol::Request`) in process.
- Once a second it lists the session's windows (one `tmux` call) and turns
  changes into events: runs, steps, approvals, outputs, the busy run per
  project, and services. It also re-captures the panes of the run or service
  on screen and sends each step's or service's whole output when it changes.
  This polling, and the `tmux` processes it starts, happen only while a TUI
  is open.
- It polls git status every 30 seconds and after each run. It never fetches.
- It watches `groundcontrol.yaml` and reloads it, keeping the last good
  config when the new one is invalid.
- Tools are found on `PATH` without running anything. tmux is asked for its
  version when the TUI opens and while the Health screen is open. Docker is
  checked only while the Health screen is open.

The dashboard's runs pane lists the runs that still have a window. There is no
history screen.

## Alerts

When an approval is waiting, a run fails or a service gives up, the worker:

- shows a message in the status line of every attached tmux client
  (`display-message -c`), because a bell in the detached `ground-control`
  session is only flagged inside that session;
- rings the bell in its own pane.

With no tmux client attached, nobody is told until a TUI opens. GroundControl
never runs `osascript`. A possible addition is an optional, off-by-default
setting to send desktop notifications through `terminal-notifier`, a signed
Homebrew app.

## Install

`make install` copies the binary to `~/.local/bin` and links the
repository's `groundcontrol.yaml` into `~/.config/ground-control/`. It
registers nothing with `launchd`. `make uninstall` removes both.

## A possible next step

Move `deploy-*`, `migrate-*` and `merge-pr` to GitHub Actions and trigger them
with `gh workflow run`. Cloudflare credentials would then stay off the laptop.
