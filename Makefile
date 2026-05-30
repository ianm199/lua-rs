# lua-rs: one way to build and test.
#
#   make test          build + Rust tests + conformance (what CI runs)
#   make rust          workspace unit/integration tests + embedding doctests
#   make conformance   official Lua 5.4 suite against the lua-rs binary
#   make parity        behavioral diff vs reference C Lua 5.4.7 (the oracle)
#   make perf          benchmark vs reference C Lua (measurement, not a gate)
#   make scaling       flag superlinear (O(n^2)) behavior in hot operations
#   make profile F=... sample a hotpath profile of running script F (needs samply)
#   make build         debug lua-rs binary
#   make setup         bootstrap the conformance test directory
#
# `make test` is the gate. It builds its own binary (no stale-binary trap)
# and runs exactly what CI runs.

CARGO ?= cargo
TEST_TIMEOUT_S ?= 90
export TEST_TIMEOUT_S

.PHONY: help test build setup rust conformance parity perf scaling profile clean

help:
	@grep -E '^#   make ' Makefile | sed 's/^#   /  /'

test: build rust conformance
	@echo "== all gates passed =="

build:
	$(CARGO) build --bin lua-rs

# reference/lua-c is gitignored; recreate the testes symlink the harness
# expects, pointing at the committed test files. Idempotent.
setup:
	@mkdir -p reference/lua-c
	@ln -sfn ../lua-5.4.7-tests reference/lua-c/testes
	@echo "conformance tests available: $$(ls reference/lua-c/testes/*.lua 2>/dev/null | wc -l | tr -d ' ')"

rust:
	$(CARGO) test --workspace --lib --tests
	$(CARGO) test -p lua-rs-runtime --doc

conformance: build setup
	./harness/run_official_all.sh

# Behavioral parity oracle: same wrapped test through lua-rs AND reference C
# 5.4.7, diff normalized stdout+exit. Exits nonzero on any divergence.
# This is the truth-teller `conformance` (no-crash) cannot be.
parity: build setup
	./harness/parity_check.sh

perf:
	@[ -x reference/lua-5.4.7/src/lua ] || $(MAKE) -C reference/lua-5.4.7 guess
	$(CARGO) build --release --bin lua-rs
	bash harness/bench/compare.sh

scaling:
	$(CARGO) build --release --bin lua-rs
	python3 harness/bench/scaling-check.py

# Hotpath profile of a script: make profile F=harness/bench/workloads/gc_pressure.lua
profile:
	@command -v samply >/dev/null 2>&1 || { echo "samply not found: cargo install samply"; exit 1; }
	@[ -n "$(F)" ] || { echo "usage: make profile F=<script.lua>"; exit 1; }
	$(CARGO) build --release --bin lua-rs
	samply record target/release/lua-rs $(F)

clean:
	$(CARGO) clean
