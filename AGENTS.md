# Project instructions

This file is loaded into the agent's system prompt automatically (and is the
same convention Claude Code / Codex follow), so keep it short and actionable.

For Claude Code sessions the fuller policy lives in `CLAUDE.md` and `.claude/`;
this file is the short version that this project's *own* agent reads. Keep the
two consistent - if a rule below changes, check `.claude/rules/` as well.

## What this repository is

A Rust CLI coding agent built with clean architecture. Five crates under
`crates/`, dependencies point inwards only:

- `domain` — entities, value objects, ports (traits). std + serde only.
- `application` — the agent loop, tool implementations, prompt assembly.
  **Must not depend on `infrastructure`, and must not depend on tokio.**
- `infrastructure` — LLM HTTP clients, sandboxed filesystem, config, telemetry.
- `sdk` — the single composition root (`unstable::assemble`), and a small
  stable API for embedding the agent (`Agent`, `Session`, the event DTOs).
- `cli` — argument parsing, rendering, the terminal's approval prompt and the
  REPL: a thin layer over `sdk`.

## Rules for changes

1. Never add a dependency from `application` to `infrastructure`. If a use case
   needs a new capability, add a port in `domain/src/ports/` and implement it in
   `infrastructure`.
2. Anything needing a runtime (timers, HTTP, blocking IO) belongs in
   `infrastructure`, usually as a decorator (see `TimeoutTool`,
   `RetryingProvider`).
3. File access goes through `WorkspacePath` / `WorkspaceRoot`. Do not add code
   that builds paths from raw strings.
4. Every new tool needs: a JSON schema, a `ToolSafety` level, and error messages
   written for a language model to act on (say what to do next, not just what
   failed).
5. Tests must not need the network or a real model.

## Commands

All development runs inside the dev container.

```
make check          # fmt-check + clippy -D warnings + tests. Run before finishing.
make test           # tests only
make cargo CMD="…"  # any cargo command
```

Never run `cargo` directly on the host.
