# Fieldcraft

**Memory for coding agents that keeps score.**
Lessons from past sessions, retrieved on cue, graded by what happened next.

[![CI](https://github.com/radu2lupu/fieldcraft/actions/workflows/ci.yml/badge.svg)](https://github.com/radu2lupu/fieldcraft/actions/workflows/ci.yml)
[![License: PolyForm Noncommercial](https://img.shields.io/badge/license-PolyForm%20Noncommercial%201.0.0-blue.svg)](LICENSE)

Fieldcraft turns finished Claude Code, Codex and Grok sessions into typed lessons (claims,
procedures, directives, project context, episodes) and gives the relevant ones to the next session
through a plain CLI. Unlike memory systems that only store and retrieve, Fieldcraft records what
happened after retrieval: which lessons the agent cited, which it contradicted, which it ignored,
and which touched the files it changed. Each lesson carries a confidence that moves with those
outcomes, and a lifecycle that archives lessons nobody uses, restores them if they start matching
again, and consolidates repeated ones under provenance. It is a Rust CLI and server over one SQLite
file, with an optional shared server so every machine and every agent you use draws from the same
store.

**How it compares to Mem0, Letta, Zep, claude-mem, Copilot Memory and built-in agent memory:**
[docs/COMPARISON.md](docs/COMPARISON.md).

---

## How it works

```text
 session start                    during the session                  session end
 ─────────────                    ──────────────────                  ───────────
 hook ─► fieldcraft query ─► relevant lessons in context ─► agent cites lesson:<id> as it works
                                                                              │
    per-lesson confidence ◄── confirmed / contradicted / ignored ◄── hook reads transcript + git diff
              │
    scheduled: archive the unused, restore what matches again, merge twins, draft derived lessons
```

1. **Session start.** A hook reminds the agent to run `fieldcraft query "<what you are about to do>"`.
   Retrieval fuses exact dense-vector search, BM25 and project-identity cues, then ranks by file
   overlap, task overlap, phase, recency and confidence, and returns a token-budgeted set of
   `<remembered_note id="lesson:…">` blocks.
2. **During the session.** The agent cites lessons it acts on as `lesson:<id>`, writes new ones with
   `fieldcraft write`, and marks bad ones with `fieldcraft feedback <id> --outcome contradicted`.
3. **Session end.** A non-blocking hook scans the transcript and the git diff and records outcome
   events: *confirmed* (cited, or the lesson's files were among the files changed), *contradicted*,
   or *ignored* (served with a strong cue match but never used). Each event nudges that lesson's
   confidence and is logged for audit. Optionally, a background distiller reviews the transcript and
   writes at most one new lesson.
4. **Maintenance.** A scheduled pass archives lessons that are old, never served and never
   confirmed, or repeatedly contradicted: reversibly, never deleted. Archived lessons that keep
   matching live queries restore themselves. Near-duplicates merge; when two near-twins are
   repeatedly served together and one keeps winning, the other is retired. An optional "dream" pass
   clusters confirmed lessons and drafts one derived lesson per cluster, citing its sources; it stays
   provisional until a real session confirms it.

Agents talk to Fieldcraft only through the `fieldcraft` CLI. There is no MCP server, so every agent
behaves the same way. Details: [ARCHITECTURE.md](ARCHITECTURE.md).

## Status: what is proven, what is experimental

- **Working and in daily use by the maintainer:** typed lessons, hybrid retrieval with cue-aware
  scoring, automatic outcome attribution at session end, per-lesson confidence updates, reversible
  archival with restore, duplicate merging, near-twin retirement, hooks for Claude Code, Codex and
  Grok, and shared-server mode with per-client tokens.
- **Shadow mode:** outcome-driven confidence is recorded for every served lesson but is not yet the
  ranking authority; it contributes only a small additive term. The multiplicative gate
  (`FIELDCRAFT_DYNAMIC_CONF=1`) exists, is off by default, and the precision of the automatic
  outcome signal has not been audited yet.
- **Experimental:** the dream consolidation pass (built and provenance-gated; output quality not yet
  measured) and the background distiller.
- **No performance claims.** The outcome loop is instrumented and running; whether it makes agents
  measurably better is not something we assert yet. See [ROADMAP.md](ROADMAP.md) for what would
  change that.

## Quick start

Requirements today:

- **macOS** (the installer registers a launchd service; Linux is on the roadmap)
- Rust toolchain ([rustup](https://rustup.rs)), `jq`, and `python3` (used by the installer)
- An **embedding provider** (`FIELDCRAFT_EMBED_PROVIDER`): `gemini` (default today; needs a Google
  Gemini API key, and lesson text and queries are sent to the Gemini API), `gemma` (EmbeddingGemma 2
  on a local llama.cpp server; nothing leaves the machine; see [docs/OPERATIONS.md](docs/OPERATIONS.md)),
  or `none` (keyword-only retrieval; vector-based maintenance does nothing). The mode is always
  reported by `fieldcraft doctor` and the server's `/status`.
- Optional: `codex`, `claude` or `grok` on your `PATH` for the background distiller

```bash
git clone https://github.com/radu2lupu/fieldcraft.git
cd fieldcraft
bash install.sh
```

On a terminal, `install.sh` builds the binaries and hands over to `fieldcraft setup`, an
interactive wizard that asks where lessons should live (this machine, or a shared server you join
with an enrollment secret), takes your Gemini key (checked live, saved to
`~/.fieldcraft/secrets.env`), lets you pick the background distiller (`none` by default), wires
the agents, and ends with `fieldcraft doctor`. For a scripted install use
`bash install.sh --yes [--distiller none] [--server-url URL --enroll-secret S]` with
`GEMINI_API_KEY` in the environment.

The install starts the server under launchd on port 7777 (data in `~/.fieldcraft/`), mints a local
token into `~/.fieldcraft/env` (chmod 600), stores API keys in `~/.fieldcraft/secrets.env` (chmod
600, never rewritten by later installs, read by the binaries themselves), and wires the agents:

- hooks into `~/.claude/settings.json`, `~/.codex/hooks.json` and, if present, `~/.grok/hooks/`
- a marked instruction block into `~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md` and `~/.grok/rules/`
- a loader for `~/.fieldcraft/env` into your shell rc files

`bash uninstall.sh` reverses it. The background distiller runs your agent CLI non-interactively
with approvals bypassed; set `FIELDCRAFT_DISTILLER=none` in `~/.fieldcraft/env` to turn it off.

Then, in any repo:

```bash
fieldcraft write --kind procedure "To regenerate fixtures, run make fixtures before the snapshot tests."
fieldcraft query "update snapshot tests"
fieldcraft stats
```

`fieldcraft doctor` checks the whole install at any time: config, secrets, a live embedding call,
the server and your token, hooks and instruction blocks per detected agent, the distiller, and a
query round trip. It exits non-zero when something is broken.

### Sharing one store across machines

Run the server on one always-on machine on a private network (a tailnet or LAN), set
`FIELDCRAFT_ENROLL_SECRET` in its environment, and install clients with:

```bash
bash install.sh --server-url http://<server>:7777 --enroll-secret <secret>
```

See [docs/OPERATIONS.md](docs/OPERATIONS.md) for deployment, restart and triage, and
[SECURITY.md](SECURITY.md) before exposing the server to other machines.

## CLI

```text
fieldcraft setup                     Interactive onboarding (store, key, distiller, hooks, then doctor)
fieldcraft doctor [--json]           Check the install; non-zero exit on failure
fieldcraft query <text>              Retrieve relevant lessons for the current task
fieldcraft write --kind <k> <text>   Record a lesson (claim|procedure|episode|directive|context)
fieldcraft feedback <id> --outcome   Record an outcome (confirmed|ignored|contradicted)
fieldcraft distill-gate --transcript Cheap local friction score; the Stop hook skips quiet sessions
fieldcraft trace-analyze --session   Emit outcome events from a session transcript (run by the Stop hook)
fieldcraft hook prompt-submit        UserPromptSubmit hook: Jev-gated lesson injection (see ARCHITECTURE.md section 8)
fieldcraft stats                     Closed-loop stats from the server
fieldcraft session <id>              Per-session log with confidence deltas
fieldcraft status                    Lesson store statistics
fieldcraft lifecycle [--apply]       Propose (or apply) reversible archival
fieldcraft promote [--apply]         Preview (or write) a workhorse lesson as SKILL.md / AGENTS.md
fieldcraft dream [--apply]           Cluster confirmed lessons and draft derived lessons
fieldcraft ingest                    Ingest local Claude/Codex session logs
fieldcraft reindex [--provider P]     Build missing vectors for a provider (resumable; other providers kept)
fieldcraft update                    Pull the repo and reinstall binaries and config
```

`fieldcraft --help` lists everything. `FIELDCRAFT_SERVER_URL` in `~/.fieldcraft/env` decides where
commands go: a non-local URL uses the shared server, localhost uses the local store.

### The agent contract

The installed instructions ask agents to:

1. Query at session start: `fieldcraft query "<topic>"`.
2. Cite lesson IDs inline as `lesson:<id>` when acting on them, so the session-end pass can confirm them.
3. Write lessons at the end of non-trivial work: `fieldcraft write --kind <kind> "<summary>"`.
4. Mark wrong lessons: `fieldcraft feedback <id> --outcome contradicted --note "<why>"`.

## Code map

| Subsystem | File |
|---|---|
| Schema, tables, migrations | `crates/fieldcraft-storage/src/schema.rs` |
| Write path, merge, lifecycle, outcome updates | `crates/fieldcraft-storage/src/repository.rs` |
| Retrieval and scoring | `crates/fieldcraft-retrieval/src/retrieval.rs`, `cue_match.rs`, `sqlite_search.rs` |
| Embedding providers and tags | `crates/fieldcraft-core/src/embedding.rs`, `crates/fieldcraft-retrieval/src/lib.rs`, `gemma.rs` |
| Record cache (query-path scans) | `crates/fieldcraft-retrieval/src/record_cache.rs`, `crates/fieldcraft-storage/src/record_index.rs` |
| Items (things apps push): store, search, API | `crates/fieldcraft-storage/src/items.rs`, `crates/fieldcraft-retrieval/src/items.rs`, `crates/fieldcraft-server/src/items.rs` |
| Server, `trace-analyze`, `/stats`, auth | `crates/fieldcraft-server/src/lib.rs`, `auth.rs` |
| CLI (all subcommands, HTTP client) | `crates/fieldcraft-cli/src/main.rs` |
| Cue extraction | `crates/fieldcraft-core/src/cue_extract.rs` |
| Citation matching (trace analysis) | `crates/fieldcraft-extract/src/citation_match.rs` |
| Stop-hook friction gate | `crates/fieldcraft-extract/src/friction.rs`, `install/bin-hooks/stop-feedback.sh` |
| Hooks and installed agent instructions | `install/` |

## Documentation

| You want to… | Go to |
|---|---|
| Compare Fieldcraft with other memory systems | [docs/COMPARISON.md](docs/COMPARISON.md) |
| Understand retrieval, scoring and the feedback loop | [ARCHITECTURE.md](ARCHITECTURE.md) |
| See what is shipped, gated or planned | [ROADMAP.md](ROADMAP.md) |
| Deploy, restart or debug the server | [docs/OPERATIONS.md](docs/OPERATIONS.md) |
| Read what changed and why | [change_history/](change_history/) |
| Report a vulnerability | [SECURITY.md](SECURITY.md) |

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

The tests run offline and need no API keys. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[PolyForm Noncommercial 1.0.0](LICENSE). Free to use, modify and share for personal, research,
educational and nonprofit purposes. Commercial use requires a separate license: open an issue or
contact the author.
