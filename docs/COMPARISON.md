# Fieldcraft vs other memory systems

As of September 2026, based on each project's public documentation (links at the bottom). "Not
documented" means the source we read did not describe the feature, not that it cannot exist. If a
row is wrong or out of date, please open an issue.

## The short answer

Most agent memory systems answer one question: *what should the agent remember?* They extract
facts or notes, store them, and inject the relevant ones into the next prompt. Almost none of them
check what happened next: whether the agent used the memory, contradicted it, or ignored it.

Fieldcraft is built around that second question. Every retrieval is logged with the session it
served. When the session ends, Fieldcraft reads the transcript and the git diff and records, per
lesson, whether it was cited, contradicted, ignored, or touched the files the agent changed. Each
lesson carries a confidence that moves with those outcomes, and a lifecycle that retires lessons
nobody uses and restores them if they become relevant again.

The recording is live. Using that signal to rank lessons is still mostly switched off (see
[What is proven](#what-is-proven-and-what-is-not)). So today the honest pitch is "memory that keeps
score", not "memory that has been shown to learn".

## The landscape

Four kinds of system get called "agent memory":

1. **Hosted memory APIs for chat and product agents.** Mem0, Zep, Supermemory, Letta, cognee,
   Hindsight. They extract facts about users and conversations and serve them through an SDK or MCP.
2. **Memory built into a coding tool.** Claude Code auto memory and `CLAUDE.md`, Codex memories and
   `AGENTS.md`, Cursor Memories, Windsurf Cascade Memories, GitHub Copilot Memory.
3. **Plugins for coding agents.** claude-mem, basic-memory, ByteRover CLI and similar tools that
   capture sessions through hooks or MCP and store notes locally.
4. **Research on learning from experience.** Reflexion, ExpeL, Agent Workflow Memory,
   ReasoningBank, ACE. These do close the loop on outcomes, usually with an LLM judge, but they are
   research code, not tools you install for daily work.

Fieldcraft belongs to group 3 by form (a CLI plus hooks for coding agents) and borrows its core
idea from group 4 (memory graded by outcomes).

## Comparison

| | What is stored | Who writes it | Tracks whether a memory helped | Forgetting | Surface | Where data lives |
|---|---|---|---|---|---|---|
| **Fieldcraft** | Typed lessons (claim, procedure, directive, context, episode) with files, phase and cues | The agent via CLI, a background distiller, bulk ingest | **Yes, automatically:** cited / contradicted / ignored / file overlap, from the transcript and git diff, per lesson. Recorded; only lightly used in ranking so far | Reversible archive, query-triggered restore, near-twin retirement, provenance-gated consolidation | CLI + hooks (no MCP) | Your SQLite file, optional self-hosted server; embeddings via the Gemini API |
| Claude Code auto memory [1] | `MEMORY.md` index + topic files per repo | Claude | No | Claude edits; told to trim near size limits | Built in | Machine-local, not shared across machines |
| Codex memories / `AGENTS.md` [2] | Background session summaries; static rules file | Codex / you | Not documented | Not documented | Built in | Device-local |
| Cursor Memories [3] | Short facts, user approval before saving | Cursor | No | Manual | Built in | Per user, per project |
| Windsurf Cascade Memories [4] | Auto-generated facts | Cascade | No | Manual | Built in | Workspace-local |
| GitHub Copilot Memory [5] | Repo facts with code citations, user preferences | Copilot agents | Partly: citations re-checked against the branch before use; unused entries expire | Deleted after 28 days unused; timer resets on validated use | Built in | GitHub-hosted, repo-scoped |
| claude-mem [6] | AI-compressed observations of tool use | Plugin, via 5 lifecycle hooks | Not documented | Not documented | Claude Code plugin (plus other agents) | Local SQLite + Chroma; optional hosted tier |
| Mem0 / OpenMemory [7] | LLM-extracted facts | LLM extraction | Manual: a `feedback()` API takes POSITIVE / NEGATIVE / VERY_NEGATIVE from the developer | Explicit update/delete | SDK, REST, MCP | Hosted platform, or your own stores |
| Letta (MemGPT) [8] | Memory blocks the agent edits; archival store | The agent; sleep-time agents | Not documented | Agent rewrites | SDK / API | Self-hosted or cloud |
| Zep / Graphiti [9] | Bi-temporal knowledge graph | LLM extraction | Not documented | Edges invalidated over time | SDK / API | Cloud, or self-hosted Graphiti |
| cognee [10] | Graph + vector | Extraction pipeline | An `improve` operation applies feedback; mechanism not documented | Explicit `forget` | SDK, REST, MCP | Local or self-hosted |
| basic-memory [11], ByteRover CLI [12] | Markdown notes / curated context tree | Humans and agents | No | Manual | MCP / CLI | Local files, optional sync |
| ReasoningBank, ACE (research) [13][14] | Distilled strategies, evolving playbooks | LLM reflection | Yes, via LLM judge or execution feedback | Curation | Research code | n/a |

## What is actually different

**1. Outcome attribution is automatic and per lesson.** The closest alternatives are Mem0's
feedback API, which needs a developer to call it with a verdict, and Copilot Memory, which re-checks
citations and expires unused entries. Fieldcraft derives the verdict from the agent's own session:
a `lesson:<id>` citation or an overlap between the lesson's files and the files the agent changed
counts as confirmed; an explicit "lesson:<id> is wrong" counts as contradicted; a lesson served
with a strong cue match but never used counts as ignored. Each event is idempotent, audited, and
moves that lesson's confidence. Among the systems above, only the research group grades memories by
outcome, and none of it is packaged for daily coding work.

**2. It stores lessons, not transcripts or facts.** A lesson has a kind (claim, procedure,
directive, context, episode), structured fields such as affected files, preconditions and
"invalid if", a task phase (analyze, reproduce, edit, verify) and deterministic cues. Ranking can
therefore weight file overlap and phase, not only text similarity.

**3. The store forgets, and can un-forget.** Lessons that are old, never served and never
confirmed, or repeatedly contradicted, are archived, never deleted. An archived lesson that keeps
matching live queries restores itself. When two near-duplicates are served together repeatedly and
one keeps winning, the other is retired. A consolidation pass can draft a derived lesson from a
cluster of confirmed ones; it must cite its sources and stays provisional until a real session
confirms it. Copilot has a 28-day expiry and Zep invalidates graph edges; none of the other systems
above documents restoring a forgotten memory.

**4. One CLI for every agent and every machine.** Agents talk to Fieldcraft only through the
`fieldcraft` binary, so Claude Code, Codex and Grok get identical behaviour, with no MCP server and no
IDE lock-in. Point several machines at one self-hosted server and every agent on every machine
shares one store, each with its own token.

## Where Fieldcraft is weaker

- **Not fully local.** The store is a local SQLite file, but lessons and queries are embedded with
  Google's Gemini API. Without a key, retrieval falls back to keyword search and the vector-based
  maintenance (merging, near-twin retirement, restore, consolidation) does nothing.
- **No public benchmark win yet.** See below.
- **The outcome signal is unaudited.** Nobody has hand-checked a sample of confirmed and ignored
  events for precision. File-overlap confirmation matches file basenames, so a common name such as
  `main.rs` can confirm an unrelated lesson.
- **macOS only.** The installer uses launchd.
- **Single maintainer**, early project, and a fair amount of research scaffolding in the tree.

## When not to use Fieldcraft

- You need memory for a chat or product agent (user preferences, conversation history). Mem0, Zep,
  Supermemory or Hindsight are built for that.
- You want a hosted, zero-ops service. Fieldcraft is self-hosted.
- You cannot send lesson text or queries to a third-party embedding API.
- You use one coding tool, its built-in memory is enough, and you do not need outcome tracking or
  sharing across machines and agents.
- You want human-curated, git-versioned project notes. basic-memory or ByteRover CLI fit better.
- You are on Linux or Windows.

## What is proven and what is not

- **Working and in daily use:** typed lessons, hybrid retrieval (exact dense cosine, BM25 and
  identity cues, fused), cue-aware scoring, automatic outcome attribution, per-lesson confidence
  updates, reversible archival with restore, duplicate merging, near-twin retirement, hooks for
  Claude Code, Codex and Grok, shared-server mode.
- **Shadow mode:** confidence from outcomes is recorded for every served lesson but contributes
  only a small additive term to ranking. The switch that would make it a multiplier
  (`FIELDCRAFT_DYNAMIC_CONF=1`) exists and is off by default.
- **Experimental:** the consolidation ("dream") pass, whose output quality has not been measured;
  the background distiller.
- **Evaluation:** the one end-to-end measurement is a 19-task SWE-bench Verified study (pytest
  only) in which hand-written, idealized lessons were put in the prompt. Agents took about 19% fewer
  steps (−3.47 steps, 95% CI −6.58 to −0.53). The changes in resolution rate and tokens were not
  statistically significant. Other retrieval checks are small or synthetic, and an earlier held-out
  patch-planning test showed no gain over a no-retrieval baseline. Details:
  [eval/README.md](../eval/README.md).

The next milestone is a reproducible benchmark on the real product path: lessons written by the
distiller rather than by hand, retrieved through the hooks, several repositories, several runs per
arm, and a memory-suppressed control. See [ROADMAP.md](../ROADMAP.md).

## Sources

1. Claude Code, "How Claude remembers your project": https://code.claude.com/docs/en/memory
2. Codex memories: https://learn.chatgpt.com/docs/customization/memories.md?surface=app ; outcome-aware memory request: https://github.com/openai/codex/discussions/12567
3. Cursor Memories: https://forum.cursor.com/t/about-cursors-memory-record-feature/107355 ; https://cursor.com/docs/cloud-agent/automations
4. Windsurf Cascade Memories: https://docs.windsurf.com/plugins/cascade/memories
5. GitHub Copilot Memory: https://docs.github.com/en/copilot/concepts/agents/copilot-memory
6. claude-mem: https://github.com/thedotmack/claude-mem
7. Mem0: https://docs.mem0.ai/core-concepts/how-it-works ; feedback API: https://docs.mem0.ai/platform/features/feedback-mechanism ; OpenMemory: https://mem0.ai/openmemory
8. Letta: https://www.letta.com/blog/memory-blocks/ ; https://www.letta.com/blog/sleep-time-compute/
9. Zep: https://arxiv.org/abs/2501.13956 ; Graphiti: https://neo4j.com/blog/developer/graphiti-knowledge-graph-memory/
10. cognee: https://github.com/topoteretes/cognee
11. basic-memory: https://github.com/basicmachines-co/basic-memory
12. ByteRover CLI: https://github.com/campfirein/byterover-cli
13. ReasoningBank: https://research.google/blog/reasoningbank-enabling-agents-to-learn-from-experience/
14. ACE (Agentic Context Engineering): https://arxiv.org/abs/2510.04618

Related: SWE-ContextBench (https://arxiv.org/html/2602.08316) finds that correctly selected
context helps coding agents while unfiltered context can hurt, which is the retrieval-precision
problem Fieldcraft's outcome tracking is aimed at.
