# CH-0008: Public release prep

Date: 2026-09-27

## What changed

- **Privacy scrub.** Private hostnames, tailnet IDs, usernames and home-directory paths were removed
  from code fixtures, scripts and docs. Topology is described generically (local vs shared server);
  operator-specific detail lives in the gitignored `docs/OPERATIONS.local.md`.
- **Tree trimmed from ~100 MB to ~2.5 MB.** Skill-intelligence research data (raw run logs with
  local paths, private session content, third-party skill corpora) and the vendored SWE-Bench-CL
  checkout are no longer tracked; `eval/README.md` says what ships.
- **`/enroll` hardened.** It was unauthenticated on a `0.0.0.0` bind. `EnrollPolicy` now allows
  loopback callers only, unless `FIELDCRAFT_ENROLL_SECRET` is set, in which case every caller must
  send it (`X-Fieldcraft-Enroll-Secret`). `FIELDCRAFT_ENROLL_OPEN=1` restores the old behaviour.
  `install.sh --enroll-secret` passes it. Existing tokens are unaffected.
- **Secrets file.** API keys now live in `~/.fieldcraft/secrets.env` (owner-only), one place
  that `install.sh` creates once and never rewrites. `fieldcraft_core::secrets` reads it whenever
  a key is missing from the environment (environment wins), so `fieldcraft-server`, `fieldcraft
  reindex` and the enroll policy all find the key regardless of how the process was started. The
  server logs `embeddings: gemini api key configured source=…` or a `NO EMBEDDING KEY` warning at
  startup. Installers keep operator-added plist env vars but strip plist copies of keys the file
  defines, so a stale copy cannot shadow a rotated key.
  Why: a September reinstall rewrote the server plist with only `RUST_LOG`; the production server
  ran three weeks without an embedding key, silently keyword-only, and ~4,000 lessons got no vector.
- **CLI.** `fieldcraft write --kind` accepts all five documented kinds (was claim/procedure only).
- **Onboarding.** `fieldcraft setup` (interactive: store mode, join-a-server enrollment, Gemini key
  checked live and saved to `secrets.env`, distiller choice defaulting to `none`, then the wiring
  via `install.sh --yes --skip-build --distiller …`) and `fieldcraft doctor` (non-interactive
  checklist with `--json`, non-zero exit on failure: config, secrets and permissions, live
  embedding call, server health and token, server embedding key via a new `/status` field, hooks
  and instruction blocks per detected agent, distiller, `fieldcraft query` round trip).
  `install.sh` on a terminal builds and hands over to the wizard; `--yes` keeps the old
  non-interactive behaviour and `fieldcraft update` passes it.
- **Embedding calls no longer hang.** `fieldcraft reindex` froze at 0% CPU mid-backfill on
  2026-07-06 and 2026-09-27: `encode_dense` built a new `reqwest::blocking::Client` per batch
  from inside the CLI's current-thread tokio runtime, and the blocking client's park/unpark could
  be lost (stack: `cmd_reindex` → `encode_dense` → `ClientBuilder::build` → `thread::park`).
  The client is now built once per process and every Gemini call runs on a scoped OS thread;
  a regression test drives 200 calls from a current-thread runtime against a local mock
  (`FIELDCRAFT_EMBED_GEMINI_BASE_URL`). Until this ships to a server, the July
  `reindex-watchdog.sh` (kill and relaunch on stall) remains the workaround.
- **Workflows.** `@claude` responds only to owners, members and collaborators; the review workflow
  skips fork PRs.
- **Docs.** Public README, `docs/COMPARISON.md` (vs other memory systems, cited),
  `docs/research/2026-09-public-launch-review.md` (technical review and evidence audit),
  `SECURITY.md`, a "Public launch track" in ROADMAP, and overclaim fixes (exact cosine not ANN,
  shadow-mode wording, Shen et al. citation, installed agent instructions, plugin description,
  embedding model, dream status).

## Why

The repository is becoming public. It had private infrastructure details and third-party data in
the tree, a token-minting endpoint open to the local network, an install path that could not embed,
and no public explanation of how Fieldcraft differs from other agent memory systems.

## Decisions

- Published with fresh history: the public repository starts from a single commit of this tree.
  Earlier history (with private hostnames, raw logs and a revoked token) stays in a private archive.

- Keep one repository; publish the cleaned tree with fresh history rather than flipping the existing
  repo public, because history still contains private paths and an old `tr_sk_` token
  (owner decision, not yet executed). Local tag `archive/pre-public` marks the old `main`.
- Position Fieldcraft on the mechanism that is actually different (automatic per-lesson outcome
  attribution, typed lessons, reversible lifecycle, CLI for every agent) and state the evidence
  limits plainly: shadow mode, one 19-task oracle-lesson study, no public benchmark win yet.

## How to verify

1. `cargo test --workspace` (432 pass) and `cargo clippy --workspace --all-targets -- -D warnings`.
2. `cargo test -p fieldcraft-server enroll` covers the enroll policy.
3. `git grep` for the private host, user and machine names returns nothing.
4. `git archive HEAD | tar -x -C <dir> && gitleaks dir <dir>` reports no leaks.

Plan: [docs/plans/public-release.md](../docs/plans/public-release.md).
