# Arrowhead build and installation helpers

PREFIX ?= $(if $(ARROWHEAD_INSTALL_ROOT),$(ARROWHEAD_INSTALL_ROOT),$(HOME)/.local)
CARGO ?= cargo
LOCKED ?= 1
FORCE ?= 0

WORKSPACE_ROOT := $(abspath .)
CLI_PATH := $(WORKSPACE_ROOT)/crates/arrowhead-cli
BIN_DIR := $(PREFIX)/bin

CARGO_LOCKED := $(if $(filter 0,$(LOCKED)),,--locked)
CARGO_FORCE := $(if $(filter 1,$(FORCE)),--force,)

.PHONY: install clean
install:
	@echo "Installing Arrowhead CLI to $(BIN_DIR)"
	@mkdir -p "$(BIN_DIR)"
	@"$(CARGO)" install --path "$(CLI_PATH)" --root "$(PREFIX)" --features vector-lancedb $(CARGO_LOCKED) $(CARGO_FORCE)
	@printf '\nArrowhead installed to: %s\nEnsure \"%s\" is on your PATH to use the `arrowhead` command.\n' "$(BIN_DIR)" "$(BIN_DIR)"

clean:
	@echo "Cleaning workspace"
	@"$(CARGO)" clean

.PHONY: help
help:
	@printf 'Available targets:\n'
	@printf '  install    Build and install arrowhead-cli (override PREFIX, LOCKED=0, FORCE=1 as needed)\n'
	@printf '  clean      Remove target artifacts via cargo clean\n'
