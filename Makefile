# GroundControl build and install (docs/architecture.md). Installs one binary
# and nothing that starts at login: runs and services live in tmux windows
# that you start from the TUI.

PREFIX  ?= $(HOME)/.local
BIN     := $(PREFIX)/bin/groundcontrol
CONFIG  := $(CURDIR)/groundcontrol.yaml
ifeq ($(shell uname),Darwin)
STATE   := $(HOME)/Library/Application Support/GroundControl
else
STATE   := $(HOME)/.local/share/GroundControl
endif

.PHONY: build test lint install uninstall

build:
	cargo build --release

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

## install: build, copy the binary to ~/.local/bin and link the config into the
## state folder, so the TUI finds it from any directory.
install: build
	install -d "$(PREFIX)/bin"
	install -m 0755 target/release/groundcontrol "$(BIN)"
	install -d -m 0700 "$(STATE)"
	ln -sfn "$(CONFIG)" "$(STATE)/groundcontrol.yaml"
	@echo "installed $(BIN). Open the TUI with: groundcontrol (tmux is required)"

## uninstall: remove the binary and the config link. State (history, logs) is kept.
uninstall:
	rm -f "$(BIN)" "$(STATE)/groundcontrol.yaml"
	@echo "state kept in $(STATE)"
