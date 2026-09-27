# Retrieval Usefulness Eval

This eval measures the progressive-reveal decision layer: given retrieved skill previews, should the runtime reveal the top card or abstain before any code editing?

It does not score patch correctness. It isolates whether retrieval output is actionable enough to support the next rung: patch-planning quality.

## Policy Comparison

| policy | reveals | reveal precision | reveal recall | abstain accuracy | false reveals | avg preview tokens | avg revealed tokens |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| top1_any | 207 | 0.995 | 0.995 | 1.000 | 1 | 1000 | 315 |
| high_or_use | 197 | 0.995 | 0.947 | 1.000 | 1 | 1000 | 300 |
| use_only | 197 | 0.995 | 0.947 | 1.000 | 1 | 1000 | 300 |

## Selected Runtime Policy

`high_or_use`: reveal the top card only when Fieldcraft marks it `high` confidence or `use_this_skill_card`.

- Reveal precision: 0.995 (196/197 [0.972, 0.999])
- Reveal recall over positive tasks: 0.947 (196/207 [0.907, 0.970])
- Abstain accuracy on no-skill tasks: 1.000 (140/140 [0.973, 1.000])
- False reveals: 1
- Top-3 recoverable misses: 11

## Label Source Breakdown

| source | rows | positives | abstain | reveal precision | reveal recall |
| --- | ---: | ---: | ---: | ---: | ---: |
| deterministic_corpus_holdout | 154 | 154 | 0 | 1.000 | 0.994 |
| manual_adversarial_holdout | 140 | 0 | 140 | n/a | n/a |
| manual_independent_holdout | 36 | 36 | 0 | 1.000 | 0.778 |
| manual_near_miss_holdout | 17 | 17 | 0 | 0.938 | 0.882 |

## False Reveals

| id | top skill | expected | query |
| --- | --- | --- | --- |
| `near-miss-0320` | `skill-installer` | `skill-creator` | Generate a reusable Codex skill with frontmatter and trigger guidance; not install an existing skill from GitHub. |

## Missed Positive Reveals

| id | top skill | expected | recoverable top-3 | query |
| --- | --- | --- | ---: | --- |
| `holdout-0141` | `browser` | `browser` | True | I need help with a real project task: the relevant workflow automation for the Codex in-app the relevant workflow. Work for developer the relevant workflow tasks on local targets such as localhost, 127.0.0.1, file:// and |
| `near-miss-0306` | `swiftui-performance-audit` | `swiftui-performance-audit` | True | Optimize SwiftUI scrolling performance by auditing body recomputation and state updates; not a visual animation polish task. |
| `near-miss-0320` | `skill-installer` | `skill-creator` | True | Generate a reusable Codex skill with frontmatter and trigger guidance; not install an existing skill from GitHub. |
| `independent-0324` | `swiftui-animation` | `swiftui-animation` | True | The card-to-detail transition snaps instead of flowing. Tune the matched geometry and transition behavior. |
| `independent-0326` | `swiftui-liquid-glass` | `build-ios-apps:swiftui-liquid-glass, swiftui-liquid-glass` | True | Adopt the translucent iOS 26 look in this SwiftUI navigation stack and make sure the API usage is right. |
| `independent-0337` | `google-calendar-group-scheduler` | `google-calendar, google-calendar-group-scheduler, google-calendar:google-calendar, google-calendar:google-calendar-group-scheduler` | True | Find times next week when all four attendees are free and rank the options. |
| `independent-0339` | `google-sheets` | `google-drive:google-sheets, google-sheets` | True | Open the connected spreadsheet, inspect the tab, clean the table, and update exact cell ranges. |
| `independent-0341` | `workers-best-practices` | `cloudflare, cloudflare:cloudflare, cloudflare:workers-best-practices, workers-best-practices` | True | Review this Worker for production mistakes around bindings, promises, globals, and observability. |
| `independent-0344` | `app-store-changelog` | `app-store-changelog` | True | Write App Store release notes from git history since the last tag, only user-visible changes. |
| `independent-0356` | `embedding-strategies` | `embedding-strategies` | True | Choose and tune embedding models and chunking strategy for a RAG app. |
| `independent-0357` | `rag-implementation` | `rag-implementation` | True | Implement document Q&A over proprietary files with retrieval-augmented generation. |

## Validity Notes

- This is a policy/usefulness eval, not a retrieval benchmark and not a code-solving benchmark.
- A positive hit means the revealed top card matches the frozen expected/acceptable label.
- A false reveal means the runtime would spend reveal context on a card not labeled useful for that task.
- The next rung should ask whether revealed guidance improves file localization and patch planning.

