# GroundControl build and install (docs/redesign-daemonless.md, "Housekeeping").
# Installs one binary and nothing that starts at login: runs and services live
# in tmux windows that you start from the TUI.

PREFIX  ?= $(HOME)/.local
BIN     := $(PREFIX)/bin/groundcontrol
CONFIG  := $(CURDIR)/groundcontrol.yaml
ifeq ($(shell uname),Darwin)
STATE   := $(HOME)/Library/Application Support/GroundControl
else
STATE   := $(HOME)/.local/share/GroundControl
endif

# The launchd agent earlier versions installed.
OLD_LABEL := dev.binara.groundcontrol
OLD_PLIST := $(HOME)/Library/LaunchAgents/$(OLD_LABEL).plist

.PHONY: build test lint install uninstall remove-old-agent

build:
	cargo build --release

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

## install: build, copy the binary to ~/.local/bin and link the config into the
## state folder, so the TUI finds it from any directory.
install: build remove-old-agent
	install -d "$(PREFIX)/bin"
	install -m 0755 target/release/groundcontrol "$(BIN)"
	install -d -m 0700 "$(STATE)"
	ln -sfn "$(CONFIG)" "$(STATE)/groundcontrol.yaml"
	@echo "installed $(BIN). Open the TUI with: groundcontrol (tmux is required)"

## remove-old-agent: stop and delete the launchd agent of the daemon versions.
## Stopping it stops the services it was running.
remove-old-agent:
	@if [ -f "$(OLD_PLIST)" ]; then \
		launchctl bootout "gui/$$(id -u)/$(OLD_LABEL)" 2>/dev/null || true; \
		rm -f "$(OLD_PLIST)"; \
		echo "removed the old launchd agent $(OLD_LABEL)"; \
	fi

## uninstall: remove the binary and the config link. State (history, logs) is kept.
uninstall: remove-old-agent
	rm -f "$(BIN)" "$(STATE)/groundcontrol.yaml"
	@echo "state kept in $(STATE)"
