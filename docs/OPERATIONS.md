# Operations — Fieldcraft

How Fieldcraft runs in production, how to observe it correctly, how to deploy and restart it, and the one audit mistake you must never repeat.

For *what it is*, see [../README.md](../README.md). For *how it's built*, see [../ARCHITECTURE.md](../ARCHITECTURE.md). For *where it's going*, see [../ROADMAP.md](../ROADMAP.md).

---

## Topology — and the audit trap

Fieldcraft runs in one of two modes:

| Mode | Where the store lives | Typical use |
|---|---|---|
| **Local** | `~/.fieldcraft/memory.sqlite3` on the same machine as the agent | One developer, one machine |
| **Shared server** | `fieldcraft-server` on one always-on machine (for example on your tailnet or LAN); clients point `FIELDCRAFT_SERVER_URL` at it | Several machines or several agents sharing one store |

A client may also run a local **gateway** (`fieldcraft-server --upstream-url <server-url>`) that
proxies to the shared server. In shared-server mode, only the server holds real data; a client or
gateway may still have a stale local `~/.fieldcraft/memory.sqlite3` left over from earlier use.

Clients read `FIELDCRAFT_SERVER_URL` from `~/.fieldcraft/env`; that persisted value is the routing authority and beats stale inherited shell environment. If it is non-local, commands use the HTTP server. If it is localhost, DB-backed commands may use the local SQLite catalog.

> Operator-specific hostnames, ssh aliases and usernames for a given deployment belong in the
> gitignored `docs/OPERATIONS.local.md`, never in this file.

### The mistake to never repeat

> A prior audit opened a **client machine's** local `~/.fieldcraft/memory.sqlite3`, saw ~0 outcome events, and concluded the closed-loop feedback was dead. It was not. The **authoritative** store on the shared server had ~1,759 confirmed events at the time. The audit measured the wrong database.

**Rules:**

1. **In shared-server mode, never inspect a client's local SQLite to judge system health.** It is a stale local artifact, not the system.
2. **Always observe through `fieldcraft stats`** — it calls the authoritative `/stats` endpoint (`closed_loop_stats` in `crates/fieldcraft-server/src/lib.rs`), which reads the server's DB.
3. **To inspect the DB directly, go to the source:** ssh to the server host, then query `~/.fieldcraft/memory.sqlite3` there. Never run `sqlite3 ~/.fieldcraft/memory.sqlite3` on a client and trust the result.
4. If `fieldcraft stats` returns near-empty numbers, suspect the configured server URL or the server, not the loop. Check that `~/.fieldcraft/env` points `FIELDCRAFT_SERVER_URL` at the authoritative server unless you intentionally want local mode.

---

## Observe

### First: `fieldcraft doctor`

```bash
fieldcraft doctor            # config, secrets, embedding call, server + token, hooks, distiller, query round trip
fieldcraft doctor --json     # same, machine-readable; exit code 1 on any failing check
```

Run it after every install, update or restart. On a client it also reports whether the shared
server has an embedding key (`server-embeddings`, from `/status`), which is how the three-week
keyless period of September 2026 would have been caught on day one.

### Primary: `fieldcraft stats`

```bash
fieldcraft stats            # closed-loop stats over the last 7 days (authoritative /stats)
fieldcraft session <id>     # full per-session log with dynamic_confidence deltas
```

`stats` is the only correct first move when asking "is the loop working?" It surfaces workhorse lessons, noise candidates, and session lift, computed on the server.

### Daily report

An optional launchd job on the server (`com.fieldcraft.daily-feedback-loop-report`, runs 9am) executes `~/.fieldcraft/bin/daily-feedback-loop-report.sh` and writes `~/.fieldcraft/reports/feedback-loop-latest.md`.

### Improvement-suggestions registrar

`~/.fieldcraft/bin/improvement-registrar.sh` writes `~/.fieldcraft/reports/improvement-suggestions.md`.

> These `~/.fieldcraft/bin` and `~/.fieldcraft/reports` paths exist only where the report jobs are installed (normally the server). Their absence on a client is expected, not a fault.

### Live in-flight snapshot

`scripts/status.sh` (repo root) prints the volatile picture: branches, worktrees, open PRs. [STATUS.md](../STATUS.md) holds the judgment-layer orientation.

---

## The feedback loop in operation

1. **SessionStart hook** → `fieldcraft query` → retrieval (exact dense cosine + FTS5 BM25, cue-aware scoring). The query cue bundle is logged into `retrieval_log.query_cue_bundle`.
2. **Stop hook** (non-blocking) → always writes a session-end marker and runs `fieldcraft trace-analyze --session <id> --transcript <path> --cwd <dir>`. The CLI extracts cited `lesson:<id>` references, contradictions, and the session's git-changed files, then POSTs to `/sessions/trace-analyze`. A detached LLM distiller runs only when `fieldcraft distill-gate` reports enough friction (`FIELDCRAFT_DISTILL_MIN_FRICTION`, default 3). Quiet sessions skip the LLM. Decisions: `~/.fieldcraft/state/distill.log`.
3. **Server** correlates against `retrieval_log` over the session time window and emits outcome events:
   - **CONFIRMED** — the agent cited `lesson:<id>` in the transcript, **or** file-overlap: a served lesson's `affected_files` intersect the session's changed files (source `file_overlap`).
   - **CONTRADICTED** — the agent explicitly contradicted a served lesson.
   - **IGNORED** — a retrieved-but-uncited lesson whose cue-match factor against the query exceeds the threshold (cue-gated; default 0.35).
4. **`apply_feedback_outcome`** updates `dynamic_confidence` asymptotically (`crates/fieldcraft-storage/src/repository.rs`): confirmed nudges up (`c + 0.05·(1−c)`), ignored/contradicted decay geometrically (`·0.92` / `·0.88`), clamped to [0.05, 0.95]. Every update also writes a row to `lesson_outcome_events`.

All emissions are idempotent on `(knowledge_id, session_id, source)`.

### Shadow mode — what is and isn't live

- `dynamic_confidence` is **computed and stored but is not the ranking authority.** It only feeds the small additive confidence feature (weight 0.05); its multiplicative use is gated behind env `FIELDCRAFT_DYNAMIC_CONF` (default **OFF = shadow**). The loop records outcomes; it barely steers retrieval yet.
- `cue_match_factor` exists and a cue-match adjustment is added to scores, but the full cue-aware ranking is not the ranking authority yet.
- To flip dynamic confidence on for an experiment, set `FIELDCRAFT_DYNAMIC_CONF=1` (or `true`) in the server's environment and restart. Do this only after a precision audit of the outcome events.

### Merge / consolidation

A background pass deduplicates near-identical lessons via `knowledge_merge_queue` → `obvious_merge_candidate`, scored by dense-vector cosine **≥ 0.92** plus cue overlap. It is **non-destructive**: the loser is marked `state = 'superseded'` and archived with a `duplicate_merged` provenance event, never deleted and never resurrected as an independent serving memory. ~8,955 merges done as of this writing. No time-decay of confidence yet (planned).

`duplicate_score` also merges records with no dense vector at all — HTTP-written records are embedded server-side on a best-effort basis (`index_record_best_effort`), but can stay vectorless when the embedding call fails or no key is set, so a vectorless pair only merges when title and `retrieval_text` are byte-identical after trimming and non-contradictory (`repository.rs:2779`). This closed a gap where identical HTTP-posted lessons never deduplicated.

---

## Memory lifecycle (drop, consolidate, dream)

Live on the reference shared-server deployment since 2026-07-01/02. Full design and change log:
[docs/plans/memory-lifecycle.md](plans/memory-lifecycle.md).

### What's live

- **L1 storage foundation.** Every record has a `protected` column. `is_protected` is true for an
  explicit pin, `kind in {directive, context}`, or `confidence >= 0.9`. `served_activation` is a
  ln-sum decay over `retrieval_log` appearances; `None` means never served — the eviction sentinel.
  `archive_record` / `restore_record` flip `tier` for plain archives and write a `knowledge_events`
  audit row; superseded archives are provenance tombstones and are not restorable. Neither path ever
  issues a SQL DELETE.
- **L2 eviction.** `archive_candidates` (`crates/fieldcraft-storage/src/repository.rs:1029`) finds
  ghost candidates (`age > 60d AND never served AND 0 confirmations AND not protected`) and
  contradicted-losers (`contradicted >= 3 AND contradicted > confirmed AND not protected`). The
  `fieldcraft lifecycle` CLI is dry-run by default, `--apply` gated. The server runs the same logic
  on a scheduled `lifecycle_loop` (`crates/fieldcraft-server/src/lib.rs:380`, 6h cadence via
  `FIELDCRAFT_LIFECYCLE_INTERVAL_SECS`), applying only when **`FIELDCRAFT_LIFECYCLE_APPLY=1`** — set
  in the server's launchd plist. The first production cull archived 41 records (reversible,
  audited); active count moved from ~12,920 to ~12,879.
- **L3 resurrection.** After every live retrieval, the server spawns a background task
  (`tokio::spawn`, never on the response path) that scans plain `tier='archive'` records against the query
  embedding, excluding rows with `state='superseded'` or `superseded_by` set. Matches at or above
  **`FIELDCRAFT_RESURRECTION_COSINE`** (default **0.85**) log an `archive_match` event; 3 hits
  (`FIELDCRAFT_RESURRECTION_THRESHOLD`) auto-restore via
  `restore_record`. The 0.85 default is live-calibrated: identical-text queries measure only ~0.927
  against cue-enriched record embeddings on the production store, so the original 0.93 target was
  unreachable by construction. `FIELDCRAFT_RESURRECTION=0` disables the feature. Proven end-to-end on
  production.
- **L4 RIF demotion.** `rif_demotion_candidates` finds near-twin pairs (cosine `0.85..0.92`,
  repeatedly co-served `>= 3` times) where one side has strictly more confirms; the loser is archived
  reversibly. Winners and protected records are never demoted. Runs in both the CLI `lifecycle`
  command and the server's scheduled loop, alongside L2.
- **L5 dreaming (built, `--apply` gated).** `fieldcraft dream` is dry-run by default. It clusters
  same-project/same-kind lessons (union-find, pairwise cosine `0.80..0.92`, `>= 3` members, `>= 2`
  confirms) and, with `--apply`, uses an LLM (default `codex exec --sandbox read-only`) to abstract
  ONE derived Claim per cluster. Guardrails: must cite `>= 3` sources (audited `derived_from` events),
  lands `state='provisional'`, originals are retained untouched. A `confirmed` outcome on a
  provisional record promotes it to `active` (`promoted_active` audit event). Dry-run on production
  found 3 clusters; `--apply` has run nightly on the reference deployment since 2026-07-02 under the
  guardrails above.

### Env vars

| Var | Default | Effect |
|---|---|---|
| `FIELDCRAFT_LIFECYCLE_APPLY` | unset (dry-run) | `1` makes the server's scheduled loop actually archive. |
| `FIELDCRAFT_LIFECYCLE_INTERVAL_SECS` | `21600` (6h) | Cadence of the scheduled lifecycle pass. |
| `FIELDCRAFT_RESURRECTION` | unset (enabled) | `0` disables the resurrection scan entirely. |
| `FIELDCRAFT_RESURRECTION_COSINE` | `0.85` | Minimum cosine for an archive match. |
| `FIELDCRAFT_RESURRECTION_THRESHOLD` | `3` | Number of archive matches before auto-restore. |
| `FIELDCRAFT_ENROLL_SECRET` | unset | When set, `POST /enroll` requires it in `X-Fieldcraft-Enroll-Secret` from every caller. When unset, only loopback callers can enroll. Clients pass it via `install.sh --enroll-secret`. Existing tokens are unaffected. |
| `FIELDCRAFT_ENROLL_OPEN` | unset | `1` allows unauthenticated enrollment from any address (pre-2026-09 behaviour). Fully trusted networks only. |
| `FIELDCRAFT_SECRETS_FILE` | `<data_dir>/secrets.env` | Override the secrets file location. The environment always takes precedence over the file. |
| `FIELDCRAFT_EMBED_GEMINI_BASE_URL` | `https://generativelanguage.googleapis.com/v1beta` | Embedding API root, for a proxy or a test double. |
| `FIELDCRAFT_PROMOTE_MIN_CONFIRMED` | `3` | Promote eligibility: minimum confirmed outcomes. |
| `FIELDCRAFT_PROMOTE_MIN_CONFIRM_RATE` | `0.5` | Promote eligibility: minimum confirm / total rate. |
| `FIELDCRAFT_PROMOTE_MAX_CONTRADICTED` | `0` | Promote eligibility: maximum contradicted outcomes. |

`fieldcraft promote` is local-catalog, dry-run by default, and **never scheduled**. `--apply` writes a SKILL.md package or AGENTS.md snippet for one `--id` and records `lesson_promoted` provenance. See [promote-checklist.md](promote-checklist.md).

### Backups

Take a copy of `~/.fieldcraft/memory.sqlite3` (for example into `~/.fieldcraft/backups/`) before
enabling `FIELDCRAFT_LIFECYCLE_APPLY` or running `fieldcraft dream --apply` for the first time.

---

## Deploy

### Local client update

Run this on each client:

```bash
fieldcraft update
```

`fieldcraft update` pulls the repo, reinstalls the binaries, and preserves the configured remote
server URL. In client mode, `FIELDCRAFT_SERVER_URL` should point at the shared server, not a stale local
SQLite catalog.

### Run under launchd (macOS)

launchd runs `~/.cargo/bin/fieldcraft-server --data-dir ~/.fieldcraft --port 7777` under label `com.fieldcraft.server`. On a **gateway** client the args also include `--upstream-url <server-url>`; on the **server** there is no upstream (authoritative mode). The server plist is where `FIELDCRAFT_LIFECYCLE_APPLY=1` and any `FIELDCRAFT_RESURRECTION_*` overrides go — gateways do not run the lifecycle loop.

`scripts/install-macos-launchagent.sh` automates installing the LaunchAgent plist.

### Full deploy (shared server)

```bash
ssh <server-host>
cd ~/Code/fieldcraft
git pull --ff-only
FIELDCRAFT_SERVER_PORT=7777 scripts/install-macos-launchagent.sh
curl -fsS http://127.0.0.1:7777/health
```

### Secrets: `~/.fieldcraft/secrets.env`

API keys live in one owner-only file next to the data, `<data_dir>/secrets.env`
(`~/.fieldcraft/secrets.env` by default), as `export KEY=VALUE` lines. `install.sh` creates it
once and never rewrites it; the `fieldcraft` and `fieldcraft-server` binaries read it themselves
whenever a key is missing from the environment; `~/.fieldcraft/env` sources it for shells and
hooks. Do not also put a key in the launchd plist: the environment wins over the file, so a stale
plist copy would shadow a rotated key (the installers strip such copies when the file defines the
key). The server logs `embeddings: gemini api key configured source=File|Env` at startup, or a
`NO EMBEDDING KEY` warning; check that line after every restart or reinstall.

```bash
printf 'export GEMINI_API_KEY=%s\n' '<key>' >> ~/.fieldcraft/secrets.env && chmod 600 ~/.fieldcraft/secrets.env
```

Why: in September 2026 a reinstall rewrote the server plist with only `RUST_LOG`; the server ran
for three weeks with no embedding key, every query silently fell back to BM25, ~4,000 new lessons
got no vector, and merge, resurrection and dream did nothing. Nothing alerted.

### Enrolling a new client

`POST /enroll` accepts only loopback callers unless the server has `FIELDCRAFT_ENROLL_SECRET` in its
launchd environment. To add a machine, set that secret on the server (restart it), then on the
client run `bash install.sh --server-url <server-url> --enroll-secret <secret>`. Clients that
already hold a token keep working; `install.sh` reuses the token in `~/.fieldcraft/env`.

### Restart

```bash
launchctl kickstart -k gui/$(id -u)/com.fieldcraft.server
```

**Watch for the port-release race.** Restarting can hit "Address already in use" because the old process hasn't released port 7777 yet, and the supervisor may thrash. If `kickstart` thrashes:

```bash
launchctl bootout gui/$(id -u)/com.fieldcraft.server   # stop the job
pkill fieldcraft-server                                # ensure the port is freed
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.fieldcraft.server.plist
```

This bootout/pkill/bootstrap sequence is the recovery path any time `kickstart` alone thrashes — not
just for the lifecycle loop, but generally, since the underlying cause (stale port binding) is the
same.

### On the server

The repo checkout location is up to you (the examples use `~/Code/fieldcraft`); data lives at `~/.fieldcraft/memory.sqlite3`. Same build/restart commands apply, minus the `--upstream-url` flag.

---

## Quick triage

| Symptom | First check |
|---|---|
| Anything at all | `fieldcraft doctor` first; it names the failing layer. |
| "The loop looks dead / no outcome events" | **Are you reading a client's local DB?** Run `fieldcraft stats` instead. |
| `fieldcraft stats` is empty or errors | Is `FIELDCRAFT_SERVER_URL` pointing at the intended server? Server up? Token correct? ssh to the server and check launchd. |
| Server won't restart, port in use | Port-release race — bootout + `pkill fieldcraft-server` + bootstrap (above). |
| Retrieval ignores confidence signal | Expected — shadow mode. `FIELDCRAFT_DYNAMIC_CONF` is OFF by default. |
| Active-lesson count not declining | Check `FIELDCRAFT_LIFECYCLE_APPLY=1` is set in the server's launchd plist (gateways do not run the loop); check server logs for `lifecycle maintenance pass applied`. |
| Suspect a record was wrongly archived | Plain archives are reversible: `restore_record` via resurrection (3 matching queries) or manual restore. Superseded records are canonical tombstones; write a new/current memory instead of restoring the old one. Nothing is ever deleted. |
| Duplicate lessons piling up | Merge pass is conservative (cosine ≥ 0.92 + cue overlap); check `knowledge_merge_queue`. |
