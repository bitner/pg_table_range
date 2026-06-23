# Thin wrapper so `make` / `make install` build this pgrx (Rust) extension via cargo-pgrx,
# for compatibility with PGXS-style tooling (e.g. `pgxn install`). This is NOT a real PGXS
# build — it shells out to cargo-pgrx, which must be installed:
#
#     cargo install cargo-pgrx --version 0.18.1 --locked
#
# Usage:
#     make                                              # build for the PostgreSQL PG_CONFIG finds
#     make install                                      # install into that PostgreSQL (often sudo)
#     make PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config install
#
# The PostgreSQL major version is detected from PG_CONFIG and mapped to the matching pgrx
# feature (pg16/pg17/pg18).

PG_CONFIG ?= pg_config
PG_MAJOR  := $(shell $(PG_CONFIG) --version | sed -E 's/[^0-9]*([0-9]+).*/\1/')
CARGO     ?= cargo
PGRX_HOME ?= $(HOME)/.pgrx
PGRX_ARGS := --no-default-features --features pg$(PG_MAJOR) --pg-config $(PG_CONFIG)

.PHONY: all build install clean pgrx-config

all: build

build: pgrx-config
	$(CARGO) pgrx package $(PGRX_ARGS)

install: pgrx-config
	$(CARGO) pgrx install --release $(PGRX_ARGS)

clean:
	$(CARGO) clean

# cargo-pgrx needs a config.toml that knows this PostgreSQL. Write a minimal one if absent
# (avoids `cargo pgrx init`, which would run initdb to create a throwaway dev cluster). If
# you already manage ~/.pgrx, register this server yourself: cargo pgrx init --pg$(PG_MAJOR) $(PG_CONFIG)
pgrx-config:
	@mkdir -p "$(PGRX_HOME)"
	@if [ ! -f "$(PGRX_HOME)/config.toml" ]; then \
		printf '[configs]\npg%s = "%s"\n' "$(PG_MAJOR)" "$$(command -v $(PG_CONFIG))" > "$(PGRX_HOME)/config.toml"; \
		echo "wrote $(PGRX_HOME)/config.toml for PostgreSQL $(PG_MAJOR)"; \
	fi
