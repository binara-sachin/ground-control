# GroundControl

A terminal app that starts a developer's local services and runs their deploy pipelines as windows of one tmux session, keeping everything it knows about them in that session.

## Language

**Worker**:
The GroundControl process running inside a window, supervising one service or executing one pipeline run.
_Avoid_: daemon, executor, agent

**Run window**:
A tmux window holding one pipeline run: its parameters, the git snapshot it started from, and the progress its worker publishes.
_Avoid_: run record, job

**Service window**:
A tmux window holding one supervised service and the status its worker publishes.
_Avoid_: service record, process window

**Window schema**:
The full set of `@gc_*` options a run window or service window can carry, and what each one means for that kind of window.
_Avoid_: window state, keys

**Request**:
An option the TUI writes on a window to ask its worker to act (stop a service, cancel a run, decide an approval), followed by a wake.
_Avoid_: command, signal, message
