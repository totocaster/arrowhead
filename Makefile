# Arrowhead build and installation helpers

PREFIX ?= $(if $(ARROWHEAD_INSTALL_ROOT),$(ARROWHEAD_INSTALL_ROOT),$(HOME)/.local)
CARGO ?= cargo
LOCKED ?= 1
FORCE ?= 0

WORKSPACE_ROOT := $(abspath .)
CLI_PATH := $(WORKSPACE_ROOT)/crates/arrowhead-cli
DAEMON_PATH := $(WORKSPACE_ROOT)/crates/arrowhead-daemon
BIN_DIR := $(PREFIX)/bin

CARGO_LOCKED := $(if $(filter 0,$(LOCKED)),,--locked)
CARGO_FORCE := $(if $(filter 1,$(FORCE)),--force,)

.PHONY: install clean
install:
	@set -euo pipefail; \
	BIN_DIR="$(BIN_DIR)"; \
	echo "Installing Arrowhead CLI to $$BIN_DIR"; \
	ARROWHEAD_BIN=""; \
	if [ -x "$$BIN_DIR/arrowhead" ]; then \
		ARROWHEAD_BIN="$$BIN_DIR/arrowhead"; \
	elif command -v arrowhead >/dev/null 2>&1; then \
		ARROWHEAD_BIN=$$(command -v arrowhead); \
	fi; \
	RUNNING=0; \
	if [ -n "$$ARROWHEAD_BIN" ]; then \
		echo "Ensuring no active Arrowhead daemon before install"; \
		set +e; \
		stop_output=$$("$${ARROWHEAD_BIN}" index stop 2>&1); \
		stop_status=$$?; \
		set -e; \
		printf "%s\n" "$$stop_output"; \
		if [ $$stop_status -eq 0 ] && printf "%s" "$$stop_output" | grep -q "shutdown signal sent to arrowhead indexer"; then \
			RUNNING=1; \
		elif [ $$stop_status -ne 0 ]; then \
			echo "Warning: failed to contact Arrowhead daemon (exit $$stop_status)" >&2; \
		fi; \
	else \
		echo "Arrowhead CLI not found; skipping daemon shutdown check."; \
	fi; \
	mkdir -p "$$BIN_DIR"; \
	"$(CARGO)" install --path "$(CLI_PATH)" --root "$(PREFIX)" $(CARGO_LOCKED) $(CARGO_FORCE); \
	echo "Installing Arrowhead daemon (arrowheadd) to $$BIN_DIR"; \
	"$(CARGO)" install --path "$(DAEMON_PATH)" --root "$(PREFIX)" $(CARGO_LOCKED) $(CARGO_FORCE); \
	if [ $$RUNNING -eq 1 ] && [ -x "$$BIN_DIR/arrowhead" ]; then \
		echo "Restarting Arrowhead daemon"; \
		if "$$BIN_DIR/arrowhead" index start; then \
			echo "Arrowhead daemon restarted successfully"; \
		else \
			echo "Warning: failed to restart Arrowhead daemon" >&2; \
		fi; \
	elif [ $$RUNNING -eq 1 ]; then \
		echo "Warning: Arrowhead CLI missing after install; unable to restart daemon" >&2; \
	elif [ -n "$$ARROWHEAD_BIN" ]; then \
		echo "Arrowhead daemon was not running prior to install"; \
	fi; \
	printf '\nArrowhead CLI and daemon installed to: %s\nEnsure "%s" is on your PATH so both `arrowhead` and `arrowheadd` are runnable.\n' "$$BIN_DIR" "$$BIN_DIR"

clean:
	@echo "Cleaning workspace"
	@"$(CARGO)" clean

.PHONY: help
help:
	@printf 'Available targets:\n'
	@printf '  install    Build and install arrowhead-cli and arrowheadd (uses --locked by default; override PREFIX, LOCKED=0, FORCE=1 only when needed)\n'
	@printf '  clean      Remove target artifacts via cargo clean\n'
