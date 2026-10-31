# GroundControl build, install and update (spec §5.4). macOS only for install.

LABEL   := dev.binara.groundcontrol
PREFIX  ?= $(HOME)/.local
BIN     := $(PREFIX)/bin/groundcontrol
PLIST   := $(HOME)/Library/LaunchAgents/$(LABEL).plist
CONFIG  := $(CURDIR)/groundcontrol.yaml
STATE   := $(HOME)/Library/Application Support/GroundControl
DOMAIN  := gui/$(shell id -u)

.PHONY: build test lint install uninstall restart logs

build:
	cargo build --release

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

## install: build to ~/.local/bin, write the launchd plist, load (or restart) the agent.
install: build
	@test "$$(uname)" = Darwin || { echo "make install needs macOS (launchd)"; exit 1; }
	install -d "$(PREFIX)/bin" "$(HOME)/Library/LaunchAgents"
	install -m 0755 target/release/groundcontrol "$(BIN)"
	install -d -m 0700 "$(STATE)"
	sed -e 's|@LABEL@|$(LABEL)|g' -e 's|@BIN@|$(BIN)|g' -e 's|@CONFIG@|$(CONFIG)|g' -e 's|@STATE@|$(STATE)|g' \
		packaging/$(LABEL).plist.in > "$(PLIST)"
	@if launchctl print "$(DOMAIN)/$(LABEL)" >/dev/null 2>&1; then \
		launchctl bootout "$(DOMAIN)/$(LABEL)" 2>/dev/null || true; \
	fi
	launchctl bootstrap "$(DOMAIN)" "$(PLIST)"
	launchctl kickstart -k "$(DOMAIN)/$(LABEL)"
	@echo "installed $(BIN); daemon running as $(LABEL). Open the TUI with: groundcontrol"

## restart: restart the daemon (services stop and, on the same boot, come back).
restart:
	launchctl kickstart -k "$(DOMAIN)/$(LABEL)"

uninstall:
	-launchctl bootout "$(DOMAIN)/$(LABEL)"
	rm -f "$(PLIST)" "$(BIN)"
	@echo "state kept in $(STATE)"

logs:
	tail -f "$(STATE)/daemon.log"
