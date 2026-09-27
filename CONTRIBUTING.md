# Contributing to fieldcraft

## Dev environment

Requires Rust stable (edition 2024) — install via [rustup](https://rustup.rs). The workspace
tests run offline and need no API keys.

Running Fieldcraft for real (not just the tests) also needs a `GEMINI_API_KEY` for embeddings and
one agent CLI (Codex, Claude Code or Grok) for the background distiller; see the README.

```bash
git clone https://github.com/radu2lupu/fieldcraft.git
cd fieldcraft
cargo build --workspace
cargo test --workspace
```

## Project structure

```
crates/
  fieldcraft-core/        shared types
  fieldcraft-storage/     SQLite schema and repositories
  fieldcraft-retrieval/   search, embeddings, ranking
  fieldcraft-extract/     transcript ingestion pipeline
  fieldcraft-cli/         CLI entrypoint
  fieldcraft-server/      HTTP server
eval/             evaluation harnesses and results (see eval/README.md)
docs/             architecture, operations and design notes
scripts/          utility and evaluation scripts
hooks/            Claude Code plugin hooks
install/          hooks and agent instructions installed by install.sh
```

## Running tests

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Evaluation-first development

Changes to retrieval, extraction, or scoring should come with evidence. The outcome harness is
`eval/usefulness_harness/` (warm vs cold agent runs on SWE-bench Verified tasks); see
[eval/README.md](eval/README.md) for what each suite measures and
[docs/learning-optimization-loop.md](docs/learning-optimization-loop.md) for the methodology.

PRs that change retrieval behavior without benchmark numbers will be asked to add them.

## Code style

- `cargo clippy --workspace -- -D warnings` must pass
- No `unwrap()` on error paths that can realistically fail
- New knowledge kinds or scoring signals should have a rationale comment

## Submitting a PR

1. Fork and create a branch from `main`
2. Make your changes with tests
3. Run `cargo test --workspace` and `cargo clippy --workspace -- -D warnings`
4. Open a PR with a clear description of what changed and why

For larger changes, open an issue first to align on approach.

## Licensing of contributions

The project is licensed under the PolyForm Noncommercial License 1.0.0. By submitting a
contribution you agree that it is licensed under the same terms and that the maintainer may
also license it, including commercially, as part of Fieldcraft.
