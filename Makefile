# agentic-dev — Rust backend (single binary) + Node SDK bridge deps.
#
# `make build` installs the bridge's npm deps (Claude Agent SDK, in server-rs/sdk-bridge/) and produces
# target/release/agentic-dev-server. The bridge deps are a RUNTIME requirement: the server
# spawns server-rs/sdk-bridge/sdk-bridge.mjs per turn, and preflights the SDK at boot.

SERVER := server-rs
BIN    := $(SERVER)/target/release/agentic-dev-server
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' $(SERVER)/Cargo.toml | head -1)
TARGET  := $(shell rustc -vV | sed -n 's/^host: //p')

.PHONY: build bridge-deps test run deploy dist clean help
.DEFAULT_GOAL := help

## build: install bridge npm deps + compile the release binary
build: bridge-deps
	cd $(SERVER) && cargo build --release

## bridge-deps: npm install the SDK bridge's runtime deps (server-rs/sdk-bridge/node_modules)
bridge-deps:
	cd $(SERVER)/sdk-bridge && npm install --omit=dev --no-fund --no-audit --loglevel=error

## test: run the Rust test suite (no external services, never hits real claude)
test:
	cd $(SERVER) && cargo test

## run: build then run locally (needs AGENTIC_PASSWORD)
run: build
	./$(BIN)

## deploy: build on the deploy host (then restart the service yourself)
deploy: build
	@echo "built. now: systemctl --user restart agentic-dev"

## dist: build + assemble the release tarball for this host into dist/
# Pass $(BIN) explicitly so a stale target/$(TARGET)/release binary from an old cross build
# can never shadow the host binary `make build` just produced.
dist: build
	bash scripts/package.sh $(TARGET) $(VERSION) $(BIN)

## clean: cargo clean
clean:
	cd $(SERVER) && cargo clean

help:
	@echo "agentic-dev targets:"
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /'
