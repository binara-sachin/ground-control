# GroundControl: Personal Local Developer Platform — Specification

> Status: **Draft for approval** · Owner: Sachin · Date: 2026-11-06 · Revision 4 (one view of what is running; nothing persisted)
> Processes, where state lives, and the reasons behind them: [`architecture.md`](architecture.md).
> Binary and config file use the lowercase form `groundcontrol`. Values written as `<...>` are commands or paths still to be filled in (see §13).

---

## 1. Summary

GroundControl is a single-user platform that runs only on Sachin's Mac. It manages the lifecycle of a small set of personal projects through a **terminal UI (TUI)**. It runs the CLIs already installed and logged in (`git`, `gh`, `wrangler`, `pnpm`, `cargo`, `docker`) as ordinary shell commands. It adds one view of everything that is running: a cross-project status view, pipeline runs with platform-owned approval steps for risky actions, and one place to start and stop local test stacks. It keeps **no history, logs or other data**: what it shows lives in tmux and goes away with it.

It is a disciplined wrapper around the terminal, not a CI server. One binary plays two roles:

- **The TUI** (`groundcontrol`) is the only interface. You open it when you need it; nothing starts at login.
- **Workers** (`groundcontrol svc-exec`, `groundcontrol run-exec`) run each service and each pipeline run in the foreground of its own **tmux window**, in the session `ground-control` (created if it doesn't exist). tmux, not GroundControl, keeps them alive, so quitting the TUI never stops a run or a service, and you can attach to any of them.

Every process therefore has a visible, interactive parent: Terminal → tmux → `groundcontrol` → your command. GroundControl holds no credentials, opens no network listener, installs no background agent, and needs no root privileges.

## 2. Goals

1. **Cross-project view.** One dashboard showing every registered project's git state (branch, uncommitted changes, ahead/behind) and the state of its services (running, stopped, crashed, health).
2. **Runs you can follow.** A pipeline run shows what is running, against which environment, at which commit, whether the tree had uncommitted or unpushed changes, each step's output, and the outcome, for as long as its window exists. There is no history.
3. **One place to start and stop local services.** Local test stacks start in dependency order with health checks, and stop from the same place.
4. **Guardrails for irreversible actions.** Production deploys, database migrations, and merges to `main` go through structural safety checks and explicit approval steps.

## 3. Non-goals

| Non-goal | Status |
|---|---|
| Hosted service, multi-user use, remote access | Never |
| Any network listener, including on localhost | Never; the TUI and the workers share state through tmux |
| History, logs, a database or any other stored data | Never; state lives in tmux window options and pane scrollback |
| Webhooks or any inbound network trigger | Never (breaks the no-inbound constraint) |
| Storing or managing credentials | Never; relies on existing CLI sessions and `.env` files |
| Web UI | Not planned; the TUI is the only interface |
| Scriptable subcommands (`groundcontrol run …`) | Not planned; the only subcommands are the workers' `svc-exec` and `run-exec` |
| Visual pipeline editor; the UI writing config | Never; the config is edited by hand |
| Embedded interactive terminal (PTY) | Not planned; conflicts with the approval-step model |
| Git-hook, file-watch, or scheduled triggers | Not planned; revisit only with a concrete use case |
| Auto-starting services at boot or login, or any background agent | Never; services are started on demand |
| Conditions, parallel steps, pipelines spanning projects | Not in MVP |
| Built-in providers (GitHub, Cloudflare API clients) | Not planned; everything is a shell step |
| Plugin system | Not planned |
| Linux or Windows support | Not planned; macOS only |

## 4. Constraints

- **Local only.** Nothing listens on any socket of GroundControl's own, not even on `127.0.0.1`. The TUI and the workers coordinate through tmux.
- **Nothing persisted.** GroundControl writes no files apart from a run's `$GC_OUTPUT` scratch file, which is deleted when the run ends.
- **Runs as Sachin's user.** No `sudo`, no `launchd` agent, nothing in `~/Library/LaunchAgents`.
- **Needs tmux.** Every run and service lives in a tmux window.
- **Reuses existing CLI sessions.** `gh auth`, `wrangler login`, Docker Desktop, and the SSH or credential-helper setup for git. GroundControl never reads or stores tokens except values loaded from a declared `env_file` (§9.2).
- **Shell environment.** Steps run through `/bin/zsh -c` inside a tmux window, so they inherit the environment of the tmux server, which is your interactive shell's. No `.zprofile` change is needed.
  - The Health screen's tool check (§6.6) reports any tool that can't be found.

## 5. Architecture

### 5.1 Components

**TUI** (`groundcontrol`, no arguments):

| Component | Responsibility |
|---|---|
| **Engine** | Runs inside the TUI while it is open. Loads and validates `groundcontrol.yaml` (§7.3), watches it, and keeps the previous good config when a new one is invalid, with line and column numbers. Opens run windows, starts and stops services in dependency order, and writes approval decisions and cancel and stop requests to the windows. Once a second it lists the `ground-control` windows and re-captures the panes on screen, and turns changes into live updates. Polls git status every 30 seconds and after each run; it never fetches. |
| **Screens** | Dashboard, Project, Run dialog, Run view, Health (§10). |
| **Output pane** | Feeds output captured from a tmux pane into a terminal-emulator parser and draws the resulting screen, so colours, `\r` progress lines and box-drawing tables render as they would in a terminal. |

**Workers**, each the command of a window in the `ground-control` tmux session:

| Worker | Window | Responsibility |
|---|---|---|
| `groundcontrol run-exec` | `<project>/<pipeline>` | Executes the run its window was opened for: steps, approval pauses (the TUI wakes it with the decision), outputs, timeouts, cancellation. Merges stdout and stderr, masks secrets, and writes the pane. Publishes its progress as the window's options. Closes its window 5 seconds after success; a failed run's window stays for reading until the project's next run. |
| `groundcontrol svc-exec <project> <service>` | `<project>/<service>` | Runs one service: `env_file`, masking, health checks, the crash-restart policy. Publishes its state as the window's options. Stops on a stop request from the TUI, `C-c` in the window, or the window being closed. |

**Shared state:** each window's `@gc_*` options and its pane scrollback, in the tmux server's memory (§11). Nothing is written to disk.

### 5.2 Diagram

```
 you ─▶ Terminal ─▶ tmux ─┬─ popup: groundcontrol (TUI + engine) ──reads/writes window options──┐
                          │                                                                      ▼
                          └─ session "ground-control"
                               ├─ myapp/deploy   groundcontrol run-exec ──▶ @gc_* options, pane
                               │                     └─ zsh -c "pnpm … deploy"
                               └─ myapp/api      groundcontrol svc-exec ──▶ @gc_* options, pane
                                                     └─ zsh -c "pnpm … wrangler dev"
                                     git · gh · wrangler · pnpm · cargo · docker compose
                                     (existing logins)
```

### 5.3 Tech stack

One Rust binary: the TUI and the two workers.

| Concern | Choice |
|---|---|
| Async runtime | `tokio` (process spawning, timers, health checks, polling) |
| Child processes | A new session per step or service (`setsid`): its own process group, signalled with `killpg`, and no controlling terminal even inside a tmux pane |
| Coordination and state | tmux: windows addressed by id, argv commands, `@gc_*` window options for state and requests, pane capture for output |
| TUI | `ratatui` on `crossterm` |
| Output pane | `vt100` parser (or a Ratatui widget built on it), plus `unicode-width` for display widths |
| Config | `yaml-rust2`'s event parser, which carries line and column positions; typed config is built from its tree |
| File watching | `notify` |
| Clipboard | `pbcopy` (the TUI always runs on the Mac) |

Rust was chosen over Go for reasons other than performance, which is not a factor at this scale. The owner already builds Ratatui TUIs. The terminal-in-a-pane libraries are more mature in Rust. Go's main advantage here, embedding and serving a web UI, no longer applies.

### 5.4 Install and update

- **Install:** `make install` builds the binary to `~/.local/bin/groundcontrol` and links the repository's `groundcontrol.yaml` into `~/.config/ground-control/`, so the TUI finds it from any directory. Nothing is registered to start at login.
- **Opening the TUI:** run `groundcontrol` in any terminal. The recommended setup is a tmux popup binding in `~/.tmux.conf`, for example `bind g display-popup -E -B -w 90% -h 85% groundcontrol`.
- **Update:** `git pull && make install`. Running workers keep the binary they started with; new windows use the new one.
- **Config location:** `GC_CONFIG` if set, else `~/.config/ground-control/groundcontrol.yaml` (the link), else `./groundcontrol.yaml`. The TUI passes the resolved path to every worker window.
- **GroundControl never redeploys itself from its own UI.**

## 6. Execution model

### 6.1 Steps

- Each step runs as `/bin/zsh -c "<command>"`, in the project `path`, or in `path/<cwd>` when the step sets `cwd`, from the run's worker in its tmux window.
- **Stdin is `/dev/null`, and there is no controlling terminal** (each step starts in a new session). A command that waits for input fails immediately instead of hanging, and nothing can prompt on `/dev/tty`.
- Steps run one after another, and the first failure ends the run.
- **Timeout:** 15 minutes per step by default, overridable with `timeout:`.
- **Cancel:** from the run view (the TUI sets the window's `@gc_cancel` and wakes the worker), `C-c` in the run's window, or closing the window. The worker sends `SIGTERM` to the step's whole process group, then `SIGKILL` after 10 seconds. A cancelled step marks the run as cancelled, and remaining steps are skipped.

**Env layers, each overriding the ones before:**
1. The tmux server's environment (your interactive shell's).
2. Global `env:` from the config.
3. The project environment's `vars`.
4. The values from any `env_file`.
5. `INPUT_<NAME>` for each pipeline input.
6. `OUTPUT_<KEY>` for each output an earlier step wrote (§6.4).
7. Variables set by GroundControl itself:
   - `GC_RUN_ID` (the run's window id), `GC_ENV`, `GC_OUTPUT`
   - `FORCE_COLOR=1`, `CLICOLOR_FORCE=1`, `TERM=xterm-256color`, `COLUMNS=120`. These keep colour and table layout even though output goes to a pipe rather than a terminal. The output pane scrolls horizontally when it is narrower than 120 columns.

A service's process gets only layers 1, 2 and 4 (§6.5). An approval message (§6.3) can refer to every layer except `env_file`, whose values are secret; a dry run (§8) and the project screen's step outline expand it the same way, with `GC_RUN_ID` shown as `dry-run`, and `$OUTPUT_*` (and, before inputs are chosen, `$INPUT_*`) left as written.

**Input safety.** Inputs are never substituted into command text. They reach commands only as environment variables, and their `pattern` is checked before the run starts.

### 6.2 Concurrency

- **One run per project at a time.** A start is refused while a live window holds an active run of the project: **rejected, not queued**. If two starts race, the newer worker sees the older one and ends its own run.
- Runs on different projects can happen in parallel.
- One worker per service: the oldest live window of a service owns it, and a worker that finds an older one closes its own window. A start while the service is up waits for it instead of starting a second copy.
- Several TUIs can be open at once. tmux is the only source of truth, and every TUI sees every change.

### 6.3 Approval steps (`approve:`)

- The run pauses with the status `awaiting_approval`. The worker alerts every attached tmux client (§10.5), and every open TUI shows an approval banner.
- **Approving happens only in the run view**, where the previous step's output is on screen. The dashboard banner offers only "review", which opens the run view.
- **Approving a protected run takes two keypresses:** approve, then a confirm prompt that names the project, environment and approval message. **Rejecting takes one key**, because rejecting is always safe.
- **Reject** ends the run with the status `rejected`, which is distinct from `failed`.
- **First answer wins.** The TUI writes the decision to the run window's `@gc_decision` once, wakes the waiting worker, and refuses a second answer. If two TUIs answer the same approval, the second is told it was already decided and by which action.
- An approval left unanswered for **30 minutes** cancels the run with the status `cancelled (approval expired)`.
- Approval steps are the only way a run pauses for a human. CLIs whose own prompts auto-confirm when nothing can answer them (for example `wrangler d1 migrations apply`) are made safe by placing an approval step before them.

### 6.4 Step outputs

A step may append `key=value` lines to the file named by `$GC_OUTPUT`, the same mechanism as GitHub Actions' `GITHUB_OUTPUT`.
- Outputs are published on the run's window and shown in the run view.
- Later steps receive them as `OUTPUT_<KEY>`.

A typical use is recording a restore point, such as a database bookmark, before a migration.

### 6.5 Services and groups

**Definition.** A service has a `start` command and optional `cwd`, `env_file`, `health`, `depends_on`, and `restart`.

**Health checks.**
- A check is either `url` (expects HTTP 200) or `port` (expects a TCP connection).
- A service counts as *healthy* once its check passes. It must pass within 30 seconds of start, or the start fails.
- A service without a check counts as healthy as long as its process is alive.

**Crash restarts (default `restart: on-failure`).**
- Waits between restart attempts grow from 1 second to a maximum of 60 seconds.
- After 5 crashes within 5 minutes, the service is marked **crashed** and its worker alerts the attached tmux clients (§10.5) and exits. The window keeps the last output; the TUI shows its last 50 lines.

**`restart: never`.**
- Exit code 0 shows as **stopped**.
- A non-zero exit code shows as **crashed**, and no restart is attempted.

**Groups.**
- A group starts its services in dependency order. Each service waits until the services it depends on are healthy.
- A group stops in reverse order.
- Dependency cycles are rejected by config validation.

**Ports in use by another process.** If a service's health port is already taken, the start is refused with the message "port N is in use (not started by GroundControl)". GroundControl never adopts, inspects or kills processes it didn't start.

**Stop.** The TUI sets the window's `@gc_stop` and wakes the worker (this works even while the window is in tmux copy mode). `C-c` in the window and closing the window also stop it. The worker sends `SIGTERM` to the service's process group, then `SIGKILL` after 10 seconds, publishes `stopped`, and only then exits, so a new start never races the old process for its port.

**After a reboot or logout** the tmux server is gone, and nothing starts again until you start it. There is no restore.

**Service output.** Only in the service window's scrollback, masked. tmux keeps up to 50,000 lines of scrollback per window, except in the window that creates the `ground-control` session, which gets tmux's default (usually 2,000). The session is created again whenever all its windows have closed.

### 6.6 Startup health

The Health screen shows:
- The tool-check results, resolved on `PATH` without running anything.
- Whether the tmux server is reachable.
- Whether Docker Desktop is reachable, checked only while the screen is open.
- The config validation status, with errors and their line numbers.
- The last config reload time.
- The version and the config path.

Nothing probes the system on a timer: the checks run once when the TUI opens and then only while the Health screen is open.

## 7. Configuration

A single file, `groundcontrol.yaml`, at the root of the GroundControl repository. It is edited by hand only; the TUI never writes it. An open TUI reloads it automatically when it is saved; each worker uses the config as it was when the worker started.

### 7.1 Schema reference

```yaml
env:                        # optional global env (non-secret)
  KEY: value

dev_dir: ~/dev              # optional; the folder the Projects screen scans for git repositories (default ~/dev)

projects:
  <project-id>:
    path: ~/code/...            # required
    remote: github.com/<owner>/<repo>   # if set, origin must match before any run or service start
    environments:
      <env-name>:
        protected: false        # true → confirm dialog + approve: required in every pipeline
        vars: { KEY: value }    # non-secret
    services:
      <service-id>:
        start: <command>
        cwd: <relative dir>
        env_file: <relative path>     # loaded in memory, values masked
        health: { url: <url> } | { port: <n> }
        depends_on: [<service-id>]
        restart: on-failure | never   # default on-failure
    groups:
      <group-id>: [<service-id>, ...]
    pipelines:
      <pipeline-id>:
        description: "<text>"   # optional human-readable summary, shown in the project screen
        protected: false        # pipeline-level protection (e.g. merge to main)
        warning: "<text>"      # optional destructive-action warning, shown in red in the run dialog
        environments: [<env>]   # which envs it can run against; omitted → env-less
        inputs:
          <name>: { type: string|bool|choice, pattern: <regex>, options: [..], default: <v> }
        steps:
          - name: <label>
            run: <command>
            cwd: <relative dir>
            env_file: <relative path>
            timeout: 15m
          - approve: "<message, may reference $VARS>"
          - service: start|stop|restart <service-id or group-id>
```

Service and group IDs share a namespace within a project.

### 7.2 Worked example: tmux-ui-manager

```yaml
  tmux-ui-manager:
    path: ~/dev/tmux-ui-manager
    remote: github.com/binara-sachin/tmux-ui-manager
    pipelines:
      deploy:
        steps:
          - name: Build
            run: cargo build --release
          - name: Reload tmux
            run: tmux source-file ~/.tmux.conf     # fails clearly if no tmux server is running
```

### 7.3 Validation rules

Config that fails an **error** rule is not loaded. The previous valid config stays active, and the errors are shown on the Health screen with line and column numbers.

| Rule | Severity |
|---|---|
| Schema: required fields, types, unique IDs, references to services, groups and environments resolve | Error |
| Every pipeline that is `protected`, or can run on a `protected` environment, contains at least one `approve:` step | Error |
| A step contains `push --force`, `push -f`, `reset --hard`, `clean -f`, or `branch -D` | Error |
| A service `start` command contains `--remote` | Error |
| A cycle in `depends_on` | Error |
| An input has no `pattern` (string inputs) or no `options` (choice inputs) | Error |
| A key containing `TOKEN`, `SECRET`, `KEY` or `PASSWORD` has a literal value | Warning |
| A project `path` doesn't exist | Warning (that project is disabled) |
| `dev_dir` is not a folder | Warning (the Projects screen has nothing to scan) |

The text-based checks (blocked commands, `--remote`) are **seatbelts, not guarantees**.

## 8. Safety model

1. **Manual triggers only.** Nothing runs unless you start it from the TUI. In particular, no protected run can ever start automatically.
2. **Confirmation dialog for protected runs.** It shows the project, environment, branch and commit.
   - If the working tree has uncommitted changes, or HEAD isn't on `origin`, the dialog shows a **warning but still allows** the run.
   - In that case the run view shows the run as `dirty` and/or `unpushed` for as long as its window exists.
3. **Approval steps are required** in every protected pipeline (§6.3, §7.3). The pattern is: a read-only step first, then `approve:`, then the step that makes the change. Approvals happen only in the run view, and protected approvals need a confirm keypress.
4. **Repository guard.** When `remote:` is set, `git remote get-url origin` must match it (after normalising SSH and HTTPS forms). This is checked before every run and every service start; a mismatch refuses the action.
5. **Blocked git commands** (§7.3).
6. **Dry run.** Every pipeline has a dry-run action. It shows each step's resolved command, working directory and environment variable *names*, the inputs, and which approval steps would trigger. Nothing is executed.
7. **No listener, no agent, no stored data.**
   - There is no socket and no network listener, so nothing can reach GroundControl except your own processes.
   - Nothing runs in the background of its own: every run and service is a tmux window you can see and attach to.
   - Nothing is written to disk, so there is no history or log to leak.
8. **Rollback.** A project can record a restore point before a risky step, show it in the approval message, and offer protected rollback pipelines. Nothing is kept after the run's window closes, so copy a restore point before approving.
9. **Timeouts, cancellation, and one run per project** (§6.1, §6.2).

## 9. Secrets and authentication

1. **Existing CLI sessions do the authentication**: `gh`, `wrangler`, and git's own credential setup. GroundControl never runs login flows.
2. **`env_file`.** The worker reads the file when it starts the process, passes the values into that process's environment, and keeps them in memory only for masking. They are never written anywhere, and the TUI shows variable names only.
3. **Masking.** Every `env_file` value of 6 or more characters is replaced with `***` in all output before it reaches the tmux pane, and so before any TUI sees it. The masker keeps a small overlap buffer so a secret split across two chunks of output is still caught. This is best-effort: encoded or transformed secrets are not caught. The masker has its own unit tests.
4. **The central config is in git and must never contain secrets.** Validation warns about likely literal secrets (§7.3).

## 10. Interface

The TUI is the only interface. §10.1 sets its **design language**: a look and a set of rules, not a fixed layout.

### 10.1 Design language

- **Chrome:** a top bar with the app name and numbered screen tabs (a `!` marks a screen needing attention), and on the right the config status with last reload time, and the clock. A bottom bar lists the keys valid on the current screen.
- **Panes:** bordered panes with the title set into the top border. The focused pane has a thick accent-coloured border; the others have a thin dim one. A selected row is also marked with `▶`.
- **Colour roles**, defined once as theme tokens in `src/tui/theme.rs`, the only place a colour is named (captured output keeps the colours the program wrote): accent (amber), success (green), warning (amber), danger (red), muted text, and selection background. Status is never shown by colour alone; it always has a word or symbol as well (`OK`, `FAIL`, `WAIT`, `●`, `×`, `○`).
- **Protected things are marked consistently:** a protected badge next to pipelines and environments, and a striped accent on approval banners and dialogs.
- **Layout correctness** (text must never be drawn over other text):
  - Columns have fixed or min/max widths, and overflow is truncated with `…`.
  - Width is measured in display cells (`unicode-width`), so wide characters such as the ⛅ wrangler prints take two cells.
  - Nothing is ever drawn on top of other text.
  - Below 100×28 cells the TUI shows a "terminal too small" message instead of a broken layout. Only `q` and `ctrl+c` respond until the terminal is larger again.

### 10.2 Global keys

| Key | Action |
|---|---|
| `1` `2` `3` | Dashboard, Projects, Health |
| `tab` / `shift+tab` | Next or previous pane |
| `↑↓` / `j k` | Move selection |
| `enter` | Open the selection |
| `esc` | Back; also closes help, output viewers, confirms and dialogs |
| `?` | Help |
| `q` | Quit the TUI (runs and services keep going); closes help, output viewers and confirms instead when one is open |
| `ctrl+c` | Quit from anywhere, with or without an overlay open (runs and services keep going) |
| Click | Select the row under the pointer and focus its pane; a top-bar tab opens its screen; the dashboard's approval banner opens the run on one click; fields and buttons in the run dialog and confirm act like their keys |
| Double-click | Open the row, like `enter` |
| Wheel | Move the selection in lists; scroll output (a sideways wheel pans it) |

### 10.3 Screens

| Screen | Contents |
|---|---|
| **Dashboard** | Full-width bands, top to bottom. A one-line approval banner, only when an approval waits (project and pipeline, run id, environment, message, expiry, "review"). **Projects:** branch, uncommitted changes, ahead/behind (relative to the last fetch; never fetched automatically), service summary. **Runs** that still have a window: status, id, environment, commit, duration, start time. Then the selected project's **services** (each service and group with status, port and uptime; a crashed service shows its exit code, as `crashed exit 1`) beside the selected service's **output** (last 50 lines). `tab` / `shift+tab` move focus banner → projects → runs → services (the banner only when present); the focused pane has a thick border. Keys, with the services pane focused: `s` start/stop and `r` restart the selected service or group, `l` shows its full output (the bottom bar shows them only then, as on the Project screen); `enter` opens the selection (the banner opens the run view). |
| **Projects** | Every git repository under `dev_dir` (3 levels down; hidden folders, `node_modules`, `vendor` and `target` are skipped) as a tree, nested repositories under their parent and plain folders that only group repositories dimmed. Columns are the dashboard's, plus two: **repo**, **branch**, **tree** (`clean`, or `4M 1U 2?`), **↑ ↓** (commits ahead and behind as of the last fetch), **commit** (age of the last commit) and **fetched** (age of the last fetch or pull, `never` if there has not been one). At the right end, under an **actions** heading and clear of the border, each repository has four icon buttons: `↻` fetch (`git fetch --all --prune`), `↑` push, `↓` pull, and the VS Code logo to open the folder with the `code` command (`o`; it works without a remote, and needs `code` on `PATH`). Push is a plain push (`-u` the first time a branch goes up) and never forced; pull only fast-forwards. A refusal or git error is shown as a toast. **Nothing fetches on its own**: opening the screen, `r` and the 30 s refresh only read local state, so the `↑ ↓` counts are as old as the fetched column says. Keys: `f` fetch, `p` pull, `P` push, `F` fetch every repository with a remote, `e` only repositories that need attention (uncommitted changes, commits to push or pull, or a branch not on the remote), `r` read again, `enter` opens the repository's Project screen when it is in `groundcontrol.yaml`. |
| **Project** | Header: path, whether `origin` matches, branch, commit, tree state, ahead/behind, and the run in progress. A **pipelines** pane with protected badges, the chosen environment and status hints (for example "1 waiting"), and a **services** pane; `tab` switches between them. A detail panel for the selected pipeline shows its steps, inputs, destructive warning and its latest run that still has a window. Keys, pipelines: `enter` run dialog, `d` dry run, `e` next environment. Keys, services: `s` start/stop, `r` restart, `enter` / `l` full output. |
| **Run dialog** | Titled `run <project>/<pipeline>`, with ` on <env>` when it has an environment; for protected runs it is the confirmation dialog. Rows for environment (changed with `←→` or `ctrl+e` when the pipeline has several), branch and commit. Dirty and unpushed warnings, each with the note that the run view will show the run that way. The inputs, each with a live pattern check. The pipeline's destructive warning, and the step outline showing where it will stop for approval. An error row and the buttons (start run, dry run, cancel) are always visible; when the dialog is too tall the outline, then the warnings, then other inputs give way. The bottom bar lists the keys for the focused field. Keys: `enter` start, `ctrl+d` dry run, `esc` cancel. |
| **Run view** | Header: pipeline and run id, status, environment, branch and commit, tree state (`clean, pushed`, `dirty`, `unpushed`), and elapsed time. On the left, a **steps** pane (status, exit code and duration per step, then the selected step's command and working directory) over an **outputs · inputs** pane. On the right, the selected step's **output**: live terminal output with follow mode. When an approval waits, an approval panel with the message and expiry time. Keys: `↑↓` select a step, `f` follow, `pgup`/`pgdn` `g`/`G` `←→` scroll and pan, `y` copy the command, `Y` copy the output (colour codes stripped), `a` approve (with a confirm for protected runs), `x` reject, `c` cancel the run. |
| **Health** | Full-width bands. **groundcontrol:** version and how long the TUI has been open. **tool check:** each tool once, with its path, and for tmux and Docker whether they answered. **config:** status, file, when it loaded and when a reload was last attempted, and errors and warnings with line and column numbers. Key: `r` re-checks now. |
| **Help** (`?`) | The keys of the current screen, then the ones that work everywhere (keys, mouse). |
| **Output viewer** | A service's full output, or a dry-run plan (each step's resolved command, working directory, environment variable names, and the approvals that would trigger). Keys: scroll and pan, `Y` copy, `esc` close. |

### 10.4 Output pane

- Step output, captured from the run's tmux pane, is fed through a `vt100` parser and the parsed screen is drawn into the pane. Colours, `\r`-based progress lines and box-drawing tables render as they would in a real terminal.
- Output is pipe-based, so commands still behave non-interactively (§6.1). This is not an interactive terminal.
- When the pane is narrower than `COLUMNS` (120), it scrolls horizontally rather than wrapping.
- The run's pane is split into steps at the header line the worker prints before each step. The TUI re-captures it while the run is on screen and replaces each step's view with its current output, keeping the scroll position.
- Output lasts as long as the window and its scrollback. tmux keeps up to 50,000 lines of scrollback per window, except in the window that creates the `ground-control` session, which gets tmux's default (usually 2,000). The session is created again whenever all its windows have closed. When a step's header has scrolled away, its remaining lines are still shown under that step.

### 10.5 Notifications

Workers raise an alert when a run fails, when an approval is waiting, and when a service crashes and stops restarting:
- a message in the status line of **every attached tmux client** (`display-message`), whichever session it is looking at, because the `ground-control` session is detached and a bell there is only flagged inside that session;
- the tmux bell in the worker's own pane, which marks the window in `ground-control` (visible in `choose-tree` or when attached).

Successful runs are silent. GroundControl never runs `osascript`. When no tmux client is attached (for example the TUI and every terminal are closed), nobody is told: a waiting approval is only seen the next time a TUI opens, and expires after 30 minutes. An alert only informs; approving still requires the TUI.

### 10.6 Engine calls and events

The TUI talks to its in-process engine (`Engine`, `src/engine.rs`) with typed method calls and receives events on a channel. There is no socket, envelope or handshake.

| Call | Purpose |
|---|---|
| `projects()` | Projects with git and service status |
| `start_run(RunStartParams)` | Open a run's window and return its id, or with `dry_run` return the dry-run plan; the TUI sets `confirmed` once the user has seen the confirmation dialog, and a protected run is refused without it |
| `cancel_run(id)` | Ask the run's worker to cancel |
| `decide(id, step, decision, confirmed)` | Approve or reject; refuses a protected approval without `confirmed`, and answers "already decided" if another TUI answered first |
| `runs()` | Runs that still have a window, newest first, and waiting approvals |
| `run(id)` | One run's details |
| `step_output(run, step)` | A step's output so far, from the run's pane |
| `control_service(project, id, action)` | Start, stop or restart a service or group |
| `service_output(project, id)` | A service window's output |
| `health()` | Tool check, config status, tmux and Docker |
| `watch(Watch)` | Replace what the engine follows: whether the Health screen is open, which runs' step output and which services' output to capture |

Once a second the engine lists the windows and turns changes into events; git status events come from its own 30-second poll, and config events from a reload.

| Event | Meaning |
|---|---|
| `Run`, `Step`, `Approval`, `Output` | A run's summary, a step, an approval or a value a step wrote to `$GC_OUTPUT` changed |
| `Service` | A service's status changed |
| `Git` | A project's git status changed |
| `Projects` | The project list changed shape (a config reload or a run opening or closing); call `projects()` again |
| `Config` | The config status changed |
| `StepOutput` | A followed step's output so far |
| `ServiceOutput` | A followed service's whole output, which replaces what was shown |

## 11. Where state lives

GroundControl stores nothing on disk. Everything it knows is in the tmux server's memory and goes away with the `ground-control` session (for example at logout or reboot).

| What | Where |
|---|---|
| What a window is | Window options `@gc_kind` (`svc` or `run`), `@gc_project`, `@gc_service` or `@gc_pipeline` |
| A run's parameters and snapshot | `@gc_env`, `@gc_inputs`, `@gc_branch`, `@gc_commit`, `@gc_dirty`, `@gc_unpushed`, `@gc_protected`, `@gc_started` |
| A run's progress | `@gc_state`, `@gc_reason`, `@gc_finished`, `@gc_steps`, `@gc_outputs`, `@gc_approval` (published by the worker) |
| A service's state | `@gc_state`, `@gc_healthy`, `@gc_msg`, `@gc_pid`, `@gc_started`, `@gc_restarts`, `@gc_exit` (published by the worker) |
| Requests to a worker | `@gc_cancel`, `@gc_decision` (runs), `@gc_stop` (services), written by the TUI |
| Output | The window's pane scrollback, masked (50,000 lines; tmux's default in the window that created the session) |
| Waking a worker | The window's `tmux wait-for` channel, signalled by the TUI with each request; an idle worker starts no processes |
| A run's id | The number of its window's id (`@12` is run 12); ids restart when the tmux server does |

Option values are percent-encoded where tmux would alter them (control characters, non-ASCII) or where they would break field separators (`|`, `,`).

The only files involved are your `groundcontrol.yaml` (linked into `~/.config/ground-control/` by `make install`) and a run's `$GC_OUTPUT` scratch file in the system temp folder, deleted when the run ends.

## 12. MVP scope and later phases

### MVP
- TUI engine: config loading, validation and hot reload; tool check; run and service workers in the `ground-control` tmux session, with all state in tmux.
- Pipelines: environments, typed inputs, `approve:`, `service:` steps, step outputs, dry run, all §8 safety checks.
- Services and groups: health checks, restart policy.
- TUI: Dashboard, Project, Run dialog, Run view with output pane, Health.
- Secret masking; alerts to attached tmux clients.
- Projects: **tmux-ui-manager**.

### Phase 2
- A key that switches to the selected service's or run's tmux window.
- Live-vs-HEAD indicators per environment, read from the deploy targets (GroundControl keeps no history to build them from).
- Checking Cloudflare for the version actually live, to detect deploys made outside GroundControl.
- New built-in step types, if particular shell steps keep repeating.

### Later, only with a concrete need
- Git-hook triggers for unprotected environments.
- Schedules.
- Conditions, parallel steps, pipelines spanning projects.
- More projects (diffgrid, others).

## 13. Open questions and items to verify during implementation

1. **Placeholder commands** for any project added later: write them as commands that fail loudly until filled in.
2. ~~D1 Time Travel~~ Dropped with the finance-analyzer project.
3. ~~Prompt behaviour of `wrangler`~~ Dropped with the finance-analyzer project.
4. ~~Production environment name in Wrangler~~ Dropped with the finance-analyzer project.
5. ~~Merge method for `merge-pr`~~ No project defines a merge pipeline.
6. ~~Repository paths and remotes for tmux-ui-manager~~ Set in `groundcontrol.yaml`.
7. ~~Local sign-in for the Git Internals Dashboard~~ Dropped with the git-internals project.
8. ~~YAML crate~~ `yaml-rust2`, whose events carry line and column positions (`src/yaml.rs`).
9. ~~TUI details~~ Minimum terminal size 100×28; the theme colours are in `src/tui/theme.rs`; the tmux popup binding is the example in §5.4.
10. **Security tool:** confirm that the endpoint-security alerts no longer fire with this design (see [`architecture.md`](architecture.md)).
