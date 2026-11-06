# GroundControl build and install (docs/architecture.md). Installs one binary
# and a link to the config, and nothing that starts at login: runs and
# services live in windows of the `ground-control` tmux session.

PREFIX     ?= $(HOME)/.local
BIN        := $(PREFIX)/bin/groundcontrol
CONFIG     := $(CURDIR)/groundcontrol.yaml
CONFIG_DIR := $(HOME)/.config/ground-control

.PHONY: build test lint install uninstall

build:
	cargo build --release

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

## install: build, copy the binary to ~/.local/bin and link the config into
## ~/.config/ground-control, so the TUI finds it from any directory.
install: build
	install -d "$(PREFIX)/bin"
	install -m 0755 target/release/groundcontrol "$(BIN)"
	install -d "$(CONFIG_DIR)"
	ln -sfn "$(CONFIG)" "$(CONFIG_DIR)/groundcontrol.yaml"
	@echo "installed $(BIN). Open the TUI with: groundcontrol (tmux is required)"

## uninstall: remove the binary and the config link.
uninstall:
	rm -f "$(BIN)" "$(CONFIG_DIR)/groundcontrol.yaml"
