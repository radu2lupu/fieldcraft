# Unused Code Audit

This audit records the cleanup pass for legacy and unused code. Confidence is
based on repository evidence:

- High: not compiled, not referenced, not shipped, or superseded by the current
  documented workflow.
- Medium: likely legacy, but still plausibly useful for experiments or plugin
  distribution.
- Low: old-looking, but tied to docs, tests, benchmarks, or transcript parsing.

## High-confidence removals

### npm distribution

- Evidence:
  - `package.json` defined the deprecated npm package `fieldcraft`.
  - `.github/workflows/publish.yml` published releases to npm.
  - CI previously ran `npm pack --dry-run`.
  - README previously documented npm as the recommended install path.
- Decision: delete npm package metadata and npm publish workflow; document
  Rust-first installation.
- Verification:
  - `rg -n "npm|fieldcraft|package\\.json|setup-node|npm publish|npm pack|postinstall" README.md docs .github scripts hooks commands crates -g '!target'`
  - Remaining matches are generic transcript/prompt examples for detecting npm
    logs in user projects, not distribution paths.

### Orphaned V1 storage modules

- Files:
  - `crates/fieldcraft-storage/src/artifact_store.rs`
  - `crates/fieldcraft-storage/src/jobs.rs`
  - `crates/fieldcraft-storage/src/server.rs`
- Evidence:
  - `fieldcraft-storage/src/lib.rs` does not declare these modules.
  - Cross-repo `rg` found references only inside the three orphaned files.
  - `cargo metadata --format-version 1 --no-deps` lists only
    `crates/fieldcraft-storage/src/lib.rs` as the compiled `fieldcraft_storage` library target.
  - The orphaned modules reference V1 API variants that are not present in the
    compiled `fieldcraft-core` API, so declaring them would not currently compile.
- Decision: delete the orphaned files.
- Verification:
  - `rg -n "ArtifactStoreV1|JobStoreV1|V1Canonical|open_v1_shared_repository|write_canonical_node" crates README.md docs scripts hooks commands .github -g '!target'`
  - `cargo check --workspace --all-targets`

### Stage B prompt experiment surface

- Files/symbols:
  - `scripts/prompt_eval.py`
  - `VARIANT_DEFAULT`, `VARIANT_A_DECISION_TREE`,
    `VARIANT_B_LEAD_WITH_PROCEDURE`, `VARIANT_C_DISTRIBUTION_QUOTA`, and
    `VARIANT_E_COT_KIND_REASONING` in `crates/fieldcraft-extract/src/stage_b.rs`
  - `TR_PROMPT_VARIANT` runtime selection
- Evidence:
  - `TR_PROMPT_VARIANT` is not documented or used by CI.
  - References were limited to `stage_b.rs` tests and `scripts/prompt_eval.py`.
  - The production default was already variant D.
- Decision: keep the selected production prompt as `STAGE_B_SYSTEM`, remove the
  runtime prompt switch and prompt-eval script.
- Verification:
  - `rg -n "TR_PROMPT_VARIANT|VARIANT_[A-E]|VARIANT_DEFAULT|prompt_eval\\.py|scripts/prompt_eval" crates scripts README.md docs eval .github -g '!target'`
  - `cargo test --workspace --no-run`

## Additional proven cleanup

### Claude plugin assets

- Files:
  - `.claude-plugin/plugin.json`
  - `hooks/`
- Evidence:
  - `hooks/hooks.json` references both hook scripts.
  - The hook scripts call `fieldcraft query` and `fieldcraft write`, both
    present in `fieldcraft --help`.
- Confidence: Medium.
- Decision: keep plugin runtime hooks. Stale slash commands and marketplace
  metadata are documented below as removed cleanup candidates.

### Stale slash commands and install wrapper

- Files:
  - `commands/memory-setup.md`
  - `commands/memory-rebuild.md`
  - `commands/memory-write.md`
  - `scripts/install.sh`
  - `scripts/install-pre-push-hook.sh`
  - `.claude-plugin/marketplace.json`
- Evidence:
  - `fieldcraft --help` has no `install`, `server`, `client`, or
    `backfill-categorize` commands.
  - The slash commands target the old `~/.ai-memory/knowledge` markdown flow.
  - `install-pre-push-hook.sh` lost its only known package-script entrypoint
    when npm was removed, and has no remaining docs or CI references.
  - `marketplace.json` had a stale package-style version and no runtime
    references.
- Decision: delete.

### Stale direct-import and sync scripts

- Files:
  - `scripts/import_codex_sessions.py`
  - `scripts/ingest_sessions.py`
  - `scripts/sync-all-projects.sh`
- Evidence:
  - No docs, CI, hooks, or compiled code reference these entrypoints.
  - Current CLI has `ingest` and `import-sessions` commands for session import.
  - `sync-all-projects.sh` targets old `/api/v1/projects/...` endpoints and
    embeds a static bearer token.
- Decision: delete.

### Broken benchmark launchers

- Files:
  - `scripts/gen120_launch.sh`
  - `scripts/gen120_relaunch_stalled.sh`
  - `scripts/gen120_detach_launcher.py`
  - `scripts/gen120_v4_launch.sh`
- Evidence:
  - Each launcher calls missing `scripts/swebench_codex_pilot.py`.
  - Existing benchmark comparison/evaluation scripts can still read stored
    artifacts and are retained.
- Decision: no retained tracked launcher files remain; keep artifact
  readers/evaluators.

## Retained for now

### Benchmark and research artifacts

- Files/directories:
  - `benchmarks/`
  - `eval/`
  - `research/`
  - benchmark helper scripts under `scripts/`
- Evidence:
  - README and `docs/learning-optimization-loop.md` describe evaluation-first
    development and point to benchmark results.
- Confidence: Medium.
- Decision: keep for this cleanup pass.

### Generic npm transcript patterns

- Files:
  - `crates/fieldcraft-extract/src/patterns.rs`
  - selected Stage B prompt examples
- Evidence:
  - These references classify arbitrary user project logs and instructions.
  - They are not distribution or package-manager wiring for this repo.
- Confidence: Low as cleanup candidates.
- Decision: keep.

## Verification commands

Run these after cleanup:

```bash
cargo check --workspace --all-targets
cargo test --workspace --no-run
cargo clippy --workspace -- -D warnings
bash -n scripts/*.sh hooks/*.sh
rg -n "npm|fieldcraft|package\\.json|TR_PROMPT_VARIANT|ArtifactStoreV1|JobStoreV1|V1Canonical" README.md docs .github scripts hooks commands crates
```
