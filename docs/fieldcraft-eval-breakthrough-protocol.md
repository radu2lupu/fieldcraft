# Fieldcraft Eval Breakthrough Protocol

## Goal

Measure whether Fieldcraft improves software-agent work through online memory, skill retrieval, or both, without degrading patch correctness. The headline claim must be about a production-shaped workflow: the agent may pull Fieldcraft context at runtime, but task prompts do not front-load memories or full skills.

The benchmark is optimization-driven, but public claims require a frozen protocol, frozen skill corpus manifest, causal online memory ordering, and representative task streams.

## Core Hypotheses

1. Online memory improves future tasks when a prior solved task produced reusable procedural knowledge.
2. Skill retrieval improves efficiency and sometimes solve rate when the frozen skill corpus contains a relevant broad workflow.
3. Memory plus skills should not regress code correctness relative to cold execution; any efficiency gain is invalid if it comes with meaningful solve-rate loss.
4. Pull-based retrieval should outperform prompt injection because the agent can inspect the repo first, form a focused query, and ignore irrelevant context.

## Evaluation Tiers

Production tiers:

- `cold`: no Fieldcraft tools, no memory learning.
- `memory`: memory retrieval tool enabled; solved tasks write memories; task N can only retrieve memories from tasks `< N` in the same repo stream.
- `skill`: skill retrieval tool enabled with a frozen pre-eval corpus; no online memory.
- `memory_skill`: both retrieval lanes enabled; online memory remains repo-local and chronological.

Diagnostic tiers:

- `cold_with_tools`: memory retrieval tool is visible and returns no memories; no online memory is written. This isolates prompt/tool overhead from the no-tool cold baseline.
- `memory_suppressed`: memory tool is visible and has the same prompt/tool overhead, but retrieval returns no memories. Solved tasks still write memories so overhead is isolated from retrieval content.
- `skill_suppressed`: skill tool is visible, but retrieval returns no skill cards.
- `memory_skill_suppressed`: both tools are visible, both return no content.
- `oracle_memory_diagnostic`: optional, non-headline upper-bound tier that can force retrieval of causally prior human-reviewed memories.

Suppressed tiers are required for attribution. If `memory` beats `cold` but not `memory_suppressed`, the gain is likely tool/prompt behavior rather than memory content. Suppression must not be exposed in agent-visible tool responses; it belongs only in benchmark telemetry.

The online `memory` and `memory_suppressed` tiers can accumulate different memory sets because `memory` may solve extra tasks and then write extra memories. That divergence is part of the end-to-end online-learning product claim, but per-step marginal retrieval analysis must report it explicitly and use per-task retrieval logs.

## Stream Construction

Run one repository at a time. Clear benchmark-specific Fieldcraft state between repositories. Within each repository, preserve chronological SWE-Bench-CL ordering.

Use two stream families:

- Representative streams: unfiltered chronological tasks from the selected repos. These are eligible for headline claims.
- Transfer-enriched streams: predeclared motif-rich streams used for diagnostics and system improvement. These are not headline evidence unless reported as such.

Transfer labels for headline analysis must be predeclared without gold patch leakage. Allowed label sources:

- issue title/problem statement vocabulary
- repository component or module names visible before solving
- framework/API family
- failure/test names visible in the task record
- human label made from pre-solve metadata

Gold patch file overlap may be used only for disclosed diagnostic analysis after runs complete.

## Metrics

Correctness:

- solve rate per tier
- paired solve deltas against cold
- regression count: cold solved, enhanced tier failed
- new-solve count: cold failed, enhanced tier solved
- test-file touches and patch-apply failures

Efficiency:

- wall-clock time
- input, output, cached-input, and reasoning tokens
- retrieved payload token count
- tool call count and schema/tool overhead
- shell commands, file reads, searches, tests run
- exploration reduction: commands or file reads before first gold file touched, reported as eval-time diagnostic because it depends on gold files

Retrieval process:

- retrieval call compliance by enabled lane
- P@1/P@3 or human relevance for returned memories and skills
- abstention accuracy for no-skill/no-memory opportunities
- usage classification: used, ignored, contradicted, harmful
- suppressed-tier delta for causal attribution
- per-task retrieval event rows: query text, result IDs, scores, result count, latency, and suppression status in benchmark telemetry

Interaction:

- `memory_skill - memory - skill + cold` on solve rate and efficiency
- report cases where combined context regresses despite individual lanes helping

## Gates For Public Claims

A claimable run must satisfy all of these:

1. Model/task viability: cold solve rate must be high enough to show non-degradation and low enough to leave room for learning. Target range is 10-70%; prefer 10-40% for improvement studies.
2. Sample size: enough tasks and discordant paired outcomes to support the claim. Do not market solve-rate changes from a tiny stream even if the point estimate is positive.
3. Representative evidence: the same direction of effect must appear on representative streams, not only transfer-enriched diagnostics.
4. No correctness regression: enhanced tiers must not show material cold-solved regressions. Any regression must be explained and bounded.
5. Retrieval quality: preflight retrieval on the frozen corpus must pass the agreed relevance/abstention threshold.
6. Pull compliance: enabled tiers must actually call retrieval often enough for the result to test Fieldcraft rather than ordinary agent variance.
7. Suppressed counterfactual: production tiers must beat their suppressed counterparts on the claimed dimension.
8. Isolation: repo memories cannot cross repo boundaries unless the claim explicitly says cross-repo transfer.
9. Provenance: skill corpus manifest and version hashes must be frozen before the run.
10. Reproducibility: publish runner command, model, reasoning setting, task IDs, corpus manifest, telemetry schema, and evaluation report.
11. Scale: solve-rate claims require enough paired discordant outcomes for statistical power; small streams can support engineering diagnostics or efficiency case studies, not broad public solve-rate claims.

## Iteration Loop

Each Ralph loop iteration should:

1. Run or inspect a measurement.
2. Identify the dominant failure mode: retrieval miss, irrelevant retrieval, non-use, tool overhead, patch regression, benchmark harness issue, or task/model viability.
3. Apply one targeted change to retrieval, skills, prompts, instrumentation, or stream design.
4. Re-run the smallest diagnostic that can falsify the change.
5. Ask Claude for adversarial review before promoting the change to the main benchmark protocol.

Allowed improvements:

- broader externally sourced skills, frozen before eval
- skill normalization and atomization when a skill is too broad for precise retrieval
- better retrieval thresholds and abstention
- memory write quality gates based on successful prior tasks
- pull instructions that make retrieval advisory and ignorable
- instrumentation and reporting fixes

Disallowed for headline claims:

- oracle memories derived from target solutions
- skill cards written specifically from benchmark solution patches
- selecting only streams where memory is known after the fact to help
- injecting retrieved context directly into the task prompt while claiming pull-based retrieval
- hiding regressions behind aggregate averages
