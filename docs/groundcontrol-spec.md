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
3. **One place to start and stop local services.** Test stacks such as the Git Internals Dashboard backend and webapp, and the finance UI test stack, start in dependency order with health checks, and stop from the same place.
4. **Guardrails for irreversible actions.** Production deploys, D1 migrations, and merges to `main` go through structural safety checks and explicit approval steps.

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
| **Engine** | Runs inside the TUI while it is open. Loads and validates `groundcontrol.yaml` (§7.4), watches it, and keeps the previous good config when a new one is invalid, with line and column numbers. Opens run windows, starts and stops services in dependency order, and writes approval decisions and cancel and stop requests to the windows. Once a second it lists the `ground-control` windows and re-captures the panes on screen, and turns changes into live updates. Polls git status every 30 seconds and after each run; it never fetches. |
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
                               ├─ finance/deploy   groundcontrol run-exec ──▶ @gc_* options, pane
                               │                     └─ zsh -c "pnpm … deploy"
                               └─ finance/api      groundcontrol svc-exec ──▶ @gc_* options, pane
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
| Config | A YAML parser that reports line and column positions, with `serde` for typed config (crate to confirm, §13) |
| File watching | `notify` |
| Clipboard | `pbcopy` (the TUI always runs on the Mac) |

Rust was chosen over Go for reasons other than performance, which is not a factor at this scale. The owner already builds Ratatui TUIs. The terminal-in-a-pane libraries are more mature in Rust. Go's main advantage here, embedding and serving a web UI, no longer applies.

### 5.4 Install and update

- **Install:** `make install` builds the binary to `~/.local/bin/groundcontrol` and links the repository's `groundcontrol.yaml` into `~/.config/ground-control/`, so the TUI finds it from any directory. Nothing is registered to start at login.
- **Opening the TUI:** run `groundcontrol` in any terminal. The recommended setup is a tmux popup binding in `~/.tmux.conf`, for example `bind g display-popup -E -w 90% -h 85% groundcontrol`.
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

**Environment variables, in increasing precedence:**
1. The tmux server's environment (your interactive shell's).
2. Global `env:` from the config.
3. The project environment's `vars`.
4. The values from any `env_file`.
5. `INPUT_<NAME>` for each pipeline input.
6. Variables set by GroundControl itself:
   - `GC_RUN_ID` (the run's window id), `GC_ENV`, `GC_OUTPUT`
   - `FORCE_COLOR=1`, `CLICOLOR_FORCE=1`, `TERM=xterm-256color`, `COLUMNS=120`. These keep colour and table layout even though output goes to a pipe rather than a terminal. The output pane scrolls horizontally when it is narrower than 120 columns.

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

The main use is recording the D1 Time Travel bookmark before a migration.

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
        protected: false        # pipeline-level protection (e.g. merge to main)
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

### 7.2 Worked example: finance-analyzer (Worker + D1)

```yaml
projects:
  finance-analyzer:
    path: ~/code/finance-analyzer                     # <confirm>
    remote: github.com/binara-sachin/finance-analyzer # <confirm>
    environments:
      prod: { protected: true }

    services:
      importer:
        cwd: apps/importer
        start: "<importer start command>"
        health: { url: "http://127.0.0.1:<port>/" }
        restart: never
      api-local:
        start: pnpm --filter backend exec wrangler dev --local --port 8787
        health: { port: 8787 }
      webapp-local:
        start: pnpm --filter webapp dev
        depends_on: [api-local]
        health: { port: <vite port> }
    groups:
      ui-test: [api-local, webapp-local]

    pipelines:
      # ── git ──
      pull:
        steps:
          - run: git pull --ff-only
      new-branch:
        inputs:
          branch: { type: string, pattern: "^[a-z0-9._/-]+$" }
        steps:
          - run: git switch -c "$INPUT_BRANCH"
      push:
        steps:
          - run: git push -u origin HEAD
      open-pr:
        steps:
          - run: gh pr create --fill
      merge-pr:
        protected: true
        inputs:
          pr: { type: string, pattern: "^[0-9]+$" }
        steps:
          - name: Show PR
            run: gh pr view "$INPUT_PR" && gh pr checks "$INPUT_PR"
          - approve: "Merge PR #$INPUT_PR into main?"
          - run: gh pr merge "$INPUT_PR" --squash        # <confirm merge method>

      # ── local UI testing with seed data ──
      ui-test:
        steps:
          - name: Seed local D1
            run: "<existing seed command>"           # must target --local
          - service: start ui-test

      # ── Cloudflare ──
      deploy-backend:
        environments: [prod]
        steps:
          - name: Test
            run: pnpm --filter backend test
          - approve: "Deploy backend Worker to $GC_ENV?"
          - name: Deploy
            run: pnpm --filter backend exec wrangler deploy
      deploy-webapp:
        environments: [prod]
        steps:
          - run: pnpm --filter webapp build
          - approve: "Deploy webapp to $GC_ENV?"
          - run: "<webapp deploy command>"

      migrate:
        environments: [prod]
        steps:
          - name: Pending migrations
            run: pnpm --filter backend exec wrangler d1 migrations list ledger --remote
          - name: Record restore point
            run: |
              bm=$(<extract bookmark from: pnpm --filter backend exec wrangler d1 time-travel info ledger>)
              echo "d1_bookmark=$bm" >> "$GC_OUTPUT"
          - approve: "Apply these migrations to prod D1 'ledger'? Restore point: $OUTPUT_D1_BOOKMARK"
          - name: Apply
            run: pnpm --filter backend exec wrangler d1 migrations apply ledger --remote

      # ── rollback ──
      restore-d1:
        environments: [prod]
        inputs:
          bookmark: { type: string, pattern: "^[A-Za-z0-9-]+$" }
        steps:
          - run: pnpm --filter backend exec wrangler d1 time-travel info ledger
          - approve: "Restore prod D1 to $INPUT_BOOKMARK? ALL writes after it are lost."
          - run: pnpm --filter backend exec wrangler d1 time-travel restore ledger --bookmark "$INPUT_BOOKMARK"
      rollback-worker:
        environments: [prod]
        steps:
          - run: pnpm --filter backend exec wrangler deployments list
          - approve: "Roll back the backend Worker to the previous deployment? (Does not undo D1 migrations.)"
          - run: pnpm --filter backend exec wrangler rollback
```

Notes:
- **Validation of the local test stack.** Validation rejects `--remote` in any service `start` command, and the seed step must target the local D1. The `ui-test` stack can never touch production D1, and it never reads `data/processed/`.
- **Keep the bookmark.** GroundControl keeps no history, so the bookmark the `migrate` run records is visible only while its window exists: in the approval message, the run view's outputs and the pane. A succeeded run's window closes 5 seconds after it ends, so copy the bookmark before approving the migration. `wrangler d1 time-travel info` can also look up a restore point afterwards.
- **Rollbacks are independent.** Rolling back the Worker does not undo a migration. Restoring D1 discards every write made after the bookmark, including real transactions that arrived in the meantime. The TUI repeats this warning on both rollback pipelines.

### 7.3 Worked examples: Git Internals Dashboard and tmux-ui-manager

```yaml
  git-internals:
    path: ~/code/cs-tools                         # <confirm>
    remote: github.com/wso2-open-operations/cs-tools
    # No git pipelines by design. Dashboard shows read-only git status.
    services:
      postgres:
        cwd: apps/git-internals-dashboard
        start: docker compose up postgres          # foreground; stop → containers stop
        health: { port: 5432 }
      backend:
        cwd: apps/git-internals-dashboard/backend
        start: "<go run command>"
        env_file: .env                             # GitHub token, CORS_ALLOWED_ORIGINS, DB URL
        depends_on: [postgres]
        health: { url: "<backend health url>" }
      webapp:
        cwd: apps/git-internals-dashboard/webapp
        start: pnpm dev
        depends_on: [backend]
        health: { port: 5173 }
    groups:
      local: [postgres, backend, webapp]
    pipelines:
      seed:
        steps:
          - cwd: apps/git-internals-dashboard/backend
            env_file: .env
            run: "<seed command>"

  tmux-ui-manager:
    path: ~/code/tmux-ui-manager                  # <confirm: dev checkout symlinked into ~/.tmux/plugins>
    remote: github.com/binara-sachin/tmux-ui-manager
    pipelines:
      deploy:
        steps:
          - name: Build
            run: cargo build --release
          - name: Reload tmux
            run: tmux source-file ~/.tmux.conf     # fails clearly if no tmux server is running
```

### 7.4 Validation rules

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

The text-based checks (blocked commands, `--remote`) are **seatbelts, not guarantees**.

## 8. Safety model

1. **Manual triggers only.** Nothing runs unless you start it from the TUI. In particular, no protected run can ever start automatically.
2. **Confirmation dialog for protected runs.** It shows the project, environment, branch and commit.
   - If the working tree has uncommitted changes, or HEAD isn't on `origin`, the dialog shows a **warning but still allows** the run.
   - In that case the run view shows the run as `dirty` and/or `unpushed` for as long as its window exists.
3. **Approval steps are required** in every protected pipeline (§6.3, §7.4). The pattern is: a read-only step first, then `approve:`, then the step that makes the change. Approvals happen only in the run view, and protected approvals need a confirm keypress.
4. **Repository guard.** When `remote:` is set, `git remote get-url origin` must match it (after normalising SSH and HTTPS forms). This is checked before every run and every service start; a mismatch refuses the action.
5. **Blocked git commands** (§7.4).
6. **Dry run.** Every pipeline has a dry-run action. It shows each step's resolved command, working directory and environment variable *names*, the inputs, and which approval steps would trigger. Nothing is executed.
7. **No listener, no agent, no stored data.**
   - There is no socket and no network listener, so nothing can reach GroundControl except your own processes.
   - Nothing runs in the background of its own: every run and service is a tmux window you can see and attach to.
   - Nothing is written to disk, so there is no history or log to leak.
8. **Rollback** (finance-analyzer): a D1 Time Travel bookmark is taken before every migration and shown in its approval message, and there are two protected rollback pipelines (§7.2). The bookmark is not kept after the run's window closes.
9. **Timeouts, cancellation, and one run per project** (§6.1, §6.2).

## 9. Secrets and authentication

1. **Existing CLI sessions do the authentication**: `gh`, `wrangler`, and git's own credential setup. GroundControl never runs login flows.
2. **`env_file`.** The worker reads the file when it starts the process, passes the values into that process's environment, and keeps them in memory only for masking. They are never written anywhere, and the TUI shows variable names only.
3. **Masking.** Every `env_file` value of 6 or more characters is replaced with `***` in all output before it reaches the tmux pane, and so before any TUI sees it. The masker keeps a small overlap buffer so a secret split across two chunks of output is still caught. This is best-effort: encoded or transformed secrets are not caught. The masker has its own unit tests.
4. **The central config is in git and must never contain secrets.** Validation warns about likely literal secrets (§7.4).

## 10. Interface

The TUI is the only interface. The mockups shared during the interview set the **design language**; they are a reference, not a layout to copy.

### 10.1 Design language

- **Chrome:** a top bar with the app name and numbered screen tabs (a `!` marks a screen needing attention), and on the right the config status with last reload time, and the clock. A bottom bar lists the keys valid on the current screen.
- **Panes:** bordered panes with the title set into the top border. The focused pane has an accent-coloured border; the others are dim.
- **Colour roles**, defined once as theme tokens: accent (amber), success (green), warning (amber), danger (red), muted text, and selection background. Status is never shown by colour alone; it always has a word or symbol as well (`OK`, `FAIL`, `WAIT`, `●`, `×`, `○`).
- **Protected things are marked consistently:** a protected badge next to pipelines and environments, and a striped accent on approval banners and dialogs.
- **Layout correctness** (the mockups showed text drawn over other text, which must not happen):
  - Columns have fixed or min/max widths, and overflow is truncated with `…`.
  - Width is measured in display cells (`unicode-width`), so wide characters such as the ⛅ wrangler prints take two cells.
  - Nothing is ever drawn on top of other text.
  - Below a minimum terminal size (to confirm, §13), the TUI shows a "terminal too small" message instead of a broken layout.

### 10.2 Global keys

| Key | Action |
|---|---|
| `1` `2` | Dashboard, Health |
| `tab` / `shift+tab` | Next or previous pane |
| `↑↓` / `j k` | Move selection |
| `enter` | Open the selection |
| `esc` | Back |
| `?` | Help |
| `q` | Quit the TUI (runs and services keep going) |

### 10.3 Screens

| Screen | Contents |
|---|---|
| **Dashboard** | An approval banner when any run is waiting (with "review" only). Projects pane: branch, uncommitted changes, ahead/behind (relative to the last fetch; never fetched automatically), service summary. Services pane for the selected project: each service and group with status, health, port and uptime. Runs that still have a window. Output pane for the selected service (last 50 lines). Keys: start/stop group, restart service, open all of its output. |
| **Project** | Header: path, whether `origin` matches, branch, commit, tree state, and the run in progress. Pipelines pane with protected badges and status hints (for example "1 waiting"). Detail pane for the selected pipeline: its steps, environment, and its latest run that still has a window. Services pane. Keys: run, dry run, choose environment, start/stop, open output. |
| **Run dialog** | Project, environment, branch and commit. Dirty or unpushed warnings with the note that the run view will show it that way. An input form with live pattern checking. Destructive warnings (for example on `restore-d1`), and the step outline showing where it will stop for approval. Keys: start, dry run, cancel. |
| **Run view** | Header with run ID, status, environment, commit, tree state, and elapsed time. Steps pane: status, exit code and duration per step. Command line of the selected step. Outputs pane. Output pane showing live terminal output for the selected step, with follow mode. Approval panel with the message and expiry time; approve (with confirm for protected runs) and reject. Keys: copy command, copy output (colour codes stripped), follow, cancel run. |
| **Health** | Tool check, tmux and Docker reachability, config status and errors with line and column numbers, last reload time, version and config path. |

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

### 10.6 Engine requests

The TUI talks to its in-process engine with these requests. There is no socket, envelope or handshake.

| Request | Purpose |
|---|---|
| `projects.list` | Projects with git and service status |
| `run.start {project, pipeline, env, inputs, dry_run}` | Open a run's window, or return a dry-run plan |
| `run.cancel {id}` | Ask the run's worker to cancel |
| `run.decide {id, step, decision}` | Approve or reject; returns "already decided" if another TUI answered first |
| `runs.list` | Runs that still have a window, newest first, and waiting approvals |
| `run.get {id}` | One run's details |
| `log.read {run, step}` | A step's output so far, from the run's pane |
| `service.control {project, id, action}` | Start, stop or restart a service or group |
| `service.log {project, id}` | A service window's output |
| `health.get` | Tool check, config status, tmux and Docker |
| `subscribe {topics}` | Which runs' and services' output to follow and whether the Health screen is open; once a second the engine lists the windows for run, step, approval, service, git and config changes and delivers them as events |

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
- finance-analyzer rollback pipelines (D1 bookmark restore and Worker rollback).
- Projects: **finance-analyzer**, **Git Internals Dashboard** (services only), **tmux-ui-manager**.

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

1. **Placeholder commands to fill in from project docs:** importer start command and port, finance webapp deploy command, local Vite port, existing seed command (and confirming it targets `--local`), Git Internals backend run command, health URL and seed command.
2. **D1 Time Travel.** Confirm the current `wrangler d1 time-travel info` and `restore` syntax for wrangler 4.15x, how to extract the bookmark reliably (JSON output if available), and how far back restores can go on your Workers plan.
3. **Prompt behaviour** of `wrangler rollback` and `wrangler deploy` without a terminal. Confirm each auto-confirms or fails, and never hangs.
4. **Production environment name in Wrangler** for finance-analyzer: is it the top-level config or a named `--env`?
5. **Merge method** for `merge-pr` (squash, merge commit, or rebase) and whether the merged branch should be deleted.
6. **Repository paths and remotes** for all three projects. Is tmux-ui-manager's dev checkout the copy symlinked into `~/.tmux/plugins`?
7. **Local sign-in for the Git Internals Dashboard.** Does local testing still need an Asgardeo login, or is there a development bypass?
8. **YAML crate.** Choose a maintained Rust YAML parser that reports line and column positions for validation errors.
9. **TUI details:** minimum terminal size, the tmux popup key binding, and final theme colours.
10. **Security tool:** confirm that the endpoint-security alerts no longer fire with this design (see [`architecture.md`](architecture.md)).
