# Public Release Prep

## Status
- Owner: repo owner (agent-assisted)
- Last updated: 2026-09-27
- Current phase: Stages 1–4 done on branch `chore/public-release-prep` (local, not pushed); Stage 5 awaits owner decisions
- Decision log: see Change Log

## Problem

Fieldcraft is going from a private repo to a public GitHub repo. Today the tree and its history
are written for one operator's private two-machine setup: private hostnames and usernames are in
code, docs, and eval logs; the README opens with operator topology rather than what the project
is; eval data includes third-party and private content; the `@claude` workflow would react to
anyone's comment. There is also no public explanation of what Fieldcraft does differently from
other agent-memory systems.

## Scenarios

1. A stranger lands on the GitHub page and understands in under a minute what Fieldcraft is, how
   it differs from Mem0 / Letta / Zep / CLAUDE.md-style memory, and what is proven vs experimental.
2. A developer clones, builds, and runs Fieldcraft on one machine by following the README.
3. The owner's own agents still find the private operator topology (hostnames, ssh alias) locally.
4. Nothing in the published tree or published history exposes credentials, private hostnames,
   another person's username, or private project content.

## Scope

- Scrub private infrastructure identifiers from tracked code, docs, and data.
- Remove private and third-party research data from the public tree (kept in private history and
  an archive tag).
- Move operator-specific topology into a gitignored `docs/OPERATIONS.local.md`.
- Harden GitHub workflows for public-repo triggers; add `SECURITY.md`.
- Rewrite the README for a public audience; add a comparison doc (vs other memory systems) based
  on the Fable 5.1 research pass; fold its improvement list into the roadmap.

## Non-Goals

- Rewriting git history or pushing (owner decision, see Stage 5).
- Implementing the technical improvements the research pass recommends (tracked in ROADMAP).
- Re-running benchmarks.

## Current Evidence

- `git grep` for the tailnet FQDN / usernames: hits in `crates/fieldcraft-cli/src/main.rs` tests,
  `crates/fieldcraft-storage/src/lib.rs` test, `scripts/solvers/claude_agent.py`, README, STATUS,
  ARCHITECTURE(+INDEX), docs/OPERATIONS.md, CH-0003, docs/plans, and ~470 files under `eval/`
  (4,354 `/Users/<owner>` + 2,152 second-user path references).
- `gitleaks git` over 275 commits: one real credential — an old `tr_sk_…` bearer token in
  `scripts/sync-all-projects.sh` at `fbda5ac` (file later deleted). Other hits are example
  strings inside third-party skill text and the LoCoMo dataset.
- `eval/skills/corpus/public_registry_skills_500.jsonl` and `local_skills.jsonl` redistribute
  third-party skill bodies with unknown licenses plus private local skills.
- `benchmarks/swe-bench-cl/` (third-party, MIT, 1,406 files) is already deleted in the working tree.
- `.github/workflows/claude.yml` runs on any `@claude` mention with the owner's OAuth token.

## Requirements

- Zero matches in the tracked tree for the tailnet FQDN, tailnet ID, second user's username, and
  owner home-directory paths (checked by `git grep`).
- `gitleaks dir` on the tree reports no real secrets.
- `cargo test` passes for crates whose test fixtures changed; `cargo clippy -D warnings` clean.
- Public copy makes no claim that the repo's own eval evidence does not support.
- No private host details in public docs; owner's local operator notes survive in a gitignored file.

## Architecture

No runtime changes. Doc layout after this work:

- `README.md` — public: what it is, how it works, quick start, status, links.
- `docs/COMPARISON.md` — Fieldcraft vs other memory systems (cited).
- `docs/OPERATIONS.md` — generic single-machine and shared-server operations.
- `docs/OPERATIONS.local.md` — gitignored, owner's real topology.
- `eval/README.md` — what eval data ships and what was archived.

## Implementation Stages

1. **Privacy scrub.** Replace private hostnames/paths in test fixtures and scripts with neutral
   placeholders; genericize topology in README/STATUS/ARCHITECTURE/OPERATIONS/CH-0003/plans;
   write `docs/OPERATIONS.local.md`; add `*.local.md` to `.gitignore`.
   Rollback: `git revert`.
2. **Data removal.** `git rm` `eval/oracle_validation_v1`, `eval/skills`, `eval/patch_planning`;
   commit the existing `benchmarks/swe-bench-cl` deletion; drop its `.gitignore` exception; add
   `eval/README.md`. Rollback: data remains in history and tag `archive/pre-public`.
3. **Repo hardening.** Guard `claude.yml` by `author_association`; skip review on fork PRs;
   add `SECURITY.md`; fix stale CONTRIBUTING requirements.
4. **Positioning.** From the Fable 5.1 report: rewrite README, add `docs/COMPARISON.md`, add the
   improvement list to ROADMAP. Verify every number against the repo.
5. **Owner decisions before publishing** (not done by the agent):
   - Publish with fresh history (recommended: orphan squash of the cleaned tree into a new public
     repo or a force-pushed `main`, keeping the private repo as archive) vs `git filter-repo`.
   - Revoke the leaked `tr_sk_…` token if any server still accepts it.
   - Push branch / open PR.

## Testing Plan

- `git grep -n -E '<fqdn>|<tailnet-id>|<second-user>|/Users/<owner>'` → no output.
- `gitleaks dir .` on the working tree.
- `cargo test -p fieldcraft-cli -p fieldcraft-storage`; `cargo clippy --workspace -- -D warnings`.
- Relative-link check over changed Markdown files.
- Manual read of README and COMPARISON for claims vs `eval/RESULTS.md` / ROADMAP status.

## Review Plan

- Wrong-problem: does the public tree still leak via docs/plans or change_history? (grep gate)
- Regression: owner's agents lose topology → covered by `docs/OPERATIONS.local.md` + pointer.
- Evidence: every comparative claim in COMPARISON cites a source; every Fieldcraft number cites a
  repo file; shadow-mode and gated features labeled as such.
- PR review via the repo's Claude review workflow once pushed.

## Definition Of Done

- Grep gate and gitleaks gate clean on the tree.
- Tests/clippy pass for touched crates.
- README + COMPARISON published in the branch, claims verified.
- Owner has a short decision list for history, token revocation, and push.

## Open Launch Blockers (found during prep)

- ~~**Fresh local install cannot embed.**~~ Fixed in `bd7b363`: the plist keeps operator env,
  receives `GEMINI_API_KEY` from the installer's environment, and is chmod 600. Still open: a
  preflight/doctor check or local-embedding fallback (ROADMAP "Public launch track").
- **macOS only.** `install.sh` calls `launchctl` unconditionally and uses `python3` to write the
  plist. Linux users have no supported install path. The README says so.
- **Distiller bypasses approvals/sandbox** on transcript text by default (`codex`). Owner decision:
  default new installs to `none` or a read-only sandbox.
- The full prioritized list from the Fable 5.1 review is the ROADMAP "Public launch track".

## Change Log

- 2026-09-27: Plan created. Audit findings recorded above.
- 2026-09-27: Stages 1–3 done on `chore/public-release-prep` (commits `341e787`, `3275c69`).
  Added scope: `/enroll` was unauthenticated on a `0.0.0.0` bind, so any host on the network could
  mint a token; now loopback-only unless `FIELDCRAFT_ENROLL_SECRET` is set (tests added). Tree
  size after data removal: 2.4 MB (from ~100 MB). Grep gate clean except README (Stage 4);
  `gitleaks dir` on the exported HEAD tree: no leaks. Local tag `archive/pre-public` marks the
  pre-cleanup `main`.
- 2026-09-27: Stage 4 done (`7968b06`): README, `docs/COMPARISON.md`, launch review, ROADMAP
  track, overclaim fixes. Review also found and fixed `fieldcraft write` rejecting 3 of 5
  documented kinds (`77a3e42`) and the installer dropping the Gemini key (`bd7b363`). Gates: grep
  gate clean on the whole tree, 432 tests pass, clippy clean, relative links resolve. Remaining:
  Stage 5 owner decisions.
- 2026-09-27 (later): production check found the mini server keyless since the Sep 8 reinstall
  (plist rewritten with only `RUST_LOG`), lifecycle apply unset since July, dream skipping every
  cluster. Added `fieldcraft_core::secrets` (`~/.fieldcraft/secrets.env`, env-first) so binaries
  find keys regardless of plist/env rewrites; installers strip plist copies; server logs key
  status at startup. Sandbox-verified. The production key install was blocked by the auto-mode
  classifier twice, then executed after the owner bypassed permissions: secrets.env written,
  plist mirrored (old binary), watchdog and env source the file, server restarted under launchd
  with the key, backfill of 4,003 vectorless lessons run (it stalled once at 2,048: a reqwest-blocking hang inside the
  CLI tokio runtime, seen before in July; finished under the July watchdog, 0 missing at 13:40Z;
  root cause fixed in the retrieval crate with a regression test). Runbook in `docs/OPERATIONS.local.md`.
