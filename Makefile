# =============================================================================
#  my-agent - container-first development commands
#
#  Every target below runs inside a disposable container via
#  `docker compose run --rm ...`. Nothing is compiled, downloaded or executed
#  on the host, so a malicious dependency (Shai-Hulud style supply-chain
#  attack) cannot reach host credentials or persist outside the container.
# =============================================================================

SHELL := /bin/bash
.DEFAULT_GOAL := help

# --- compose binary autodetection (v2 plugin or standalone) ------------------
ifndef COMPOSE
COMPOSE := $(shell docker compose version >/dev/null 2>&1 && echo "docker compose" || echo "docker-compose")
endif

# --- host identity, so bind-mounted files keep sane ownership ----------------
export UID ?= $(shell id -u)
export GID ?= $(shell id -g)

# --- generic runners ---------------------------------------------------------
RUN      := $(COMPOSE) run --rm
DEV      := $(RUN) dev
APP      := $(RUN) app
CARGO    := $(DEV) cargo

# --- overridable arguments ---------------------------------------------------
ARGS  ?=
CMD   ?=
Q     ?=
MODEL ?= qwen3:8b
BIN   ?= agent

.PHONY: help setup env image build release run ask chat tools doctor test lint fmt \
        fmt-check fix check ci sh shell cargo exec audit tree deps clean clean-all \
        ollama-up ollama-down ollama-pull ollama-logs ps

## ---------------------------------------------------------------------------
## Help
## ---------------------------------------------------------------------------
help: ## Show this help
	@echo ""
	@echo "  my-agent - make targets (all run inside containers)"
	@echo ""
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	  | sort \
	  | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "  Examples:"
	@echo "    make setup"
	@echo "    make ask Q=\"list the files under crates/domain\""
	@echo "    make chat"
	@echo "    make cargo CMD=\"add --package agent-cli indicatif\""
	@echo "    make exec CMD=\"ls -la /target\""
	@echo ""
	@echo "  compose binary: $(COMPOSE)   uid/gid: $(UID)/$(GID)"
	@echo ""

## ---------------------------------------------------------------------------
## Bootstrap
## ---------------------------------------------------------------------------
setup: env image ## First-time setup: create .env and build the dev image
	@echo "==> setup complete. Edit .env, then run: make doctor"

env: ## Create .env from .env.example if missing
	@if [ ! -f .env ]; then cp .env.example .env; echo "==> created .env"; \
	 else echo "==> .env already exists (left untouched)"; fi

image: ## Build/rebuild the development container image
	$(COMPOSE) build dev

## ---------------------------------------------------------------------------
## Build & run
## ---------------------------------------------------------------------------
build: ## cargo build (workspace, debug)
	$(CARGO) build --workspace $(ARGS)

release: ## cargo build --release
	$(CARGO) build --workspace --release $(ARGS)

run: ## Run the agent CLI. Pass flags with ARGS="..."
	$(APP) $(ARGS)

chat: ## Start the interactive agent REPL
	$(APP) chat

ask: ## One-shot prompt. Usage: make ask Q="your prompt"
	@if [ -z "$(Q)" ]; then echo "usage: make ask Q=\"your prompt\""; exit 2; fi
	$(APP) run "$(Q)"

tools: ## List the tools exposed to the LLM
	$(APP) tools

doctor: ## Show resolved configuration and ping the LLM endpoint
	$(APP) doctor

## ---------------------------------------------------------------------------
## Quality gates
## ---------------------------------------------------------------------------
test: ## Run the whole test suite
	$(CARGO) test --workspace --all-features $(ARGS)

lint: ## clippy with warnings denied
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings

fmt: ## Format the code
	$(CARGO) fmt --all

fmt-check: ## Verify formatting without writing
	$(CARGO) fmt --all -- --check

fix: ## cargo fix + clippy --fix
	$(CARGO) clippy --workspace --all-targets --all-features --fix --allow-dirty --allow-staged

check: fmt-check lint test ## fmt-check + lint + test

ci: check ## Alias used by CI

audit: ## Vulnerability scan of the dependency tree (installs cargo-audit on demand)
	$(DEV) bash -lc 'command -v cargo-audit >/dev/null || cargo install --locked cargo-audit; cargo audit'

deps: ## Show the dependency tree
	$(CARGO) tree $(ARGS)

## ---------------------------------------------------------------------------
## Generic escape hatches
## ---------------------------------------------------------------------------
cargo: ## Run any cargo command. Usage: make cargo CMD="add serde"
	@if [ -z "$(CMD)" ]; then echo "usage: make cargo CMD=\"<cargo args>\""; exit 2; fi
	$(DEV) cargo $(CMD)

exec: ## Run any shell command in the dev container. Usage: make exec CMD="ls -la"
	@if [ -z "$(CMD)" ]; then echo "usage: make exec CMD=\"<shell command>\""; exit 2; fi
	$(DEV) bash -lc '$(CMD)'

sh: shell ## Alias for `shell`
shell: ## Interactive shell inside the dev container
	$(DEV) bash

ps: ## Show compose services
	$(COMPOSE) ps

## ---------------------------------------------------------------------------
## Local LLM (Ollama)
## ---------------------------------------------------------------------------
ollama-up: ## Start the local Ollama service
	$(COMPOSE) --profile ollama up -d ollama
	@echo "==> ollama listening on http://localhost:$${OLLAMA_PORT:-11434}"
	@echo "    from containers use: AGENT_BASE_URL=http://ollama:11434/v1"

ollama-pull: ## Pull a model into Ollama. Usage: make ollama-pull MODEL=qwen3:8b
	$(COMPOSE) --profile ollama exec ollama ollama pull $(MODEL)

ollama-logs: ## Tail Ollama logs
	$(COMPOSE) --profile ollama logs -f ollama

ollama-down: ## Stop the local Ollama service
	$(COMPOSE) --profile ollama stop ollama

## ---------------------------------------------------------------------------
## Cleanup
## ---------------------------------------------------------------------------
clean: ## Remove build artifacts (keeps the dependency cache)
	-$(CARGO) clean

clean-all: ## Remove containers, images and every cache volume
	$(COMPOSE) down --volumes --remove-orphans --rmi local
