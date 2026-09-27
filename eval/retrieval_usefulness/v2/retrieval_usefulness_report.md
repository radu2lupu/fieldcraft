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

`top1_any`: reveal the top card whenever Fieldcraft returns one. This relies on retrieval-time abstention to suppress no-skill tasks instead of adding a second confidence gate.

- Reveal precision: 0.995 (206/207 [0.973, 0.999])
- Reveal recall over positive tasks: 0.995 (206/207 [0.973, 0.999])
- Abstain accuracy on no-skill tasks: 1.000 (140/140 [0.973, 1.000])
- False reveals: 1
- Top-3 recoverable misses: 1

## Label Source Breakdown

| source | rows | positives | abstain | reveal precision | reveal recall |
| --- | ---: | ---: | ---: | ---: | ---: |
| deterministic_corpus_holdout | 154 | 154 | 0 | 1.000 | 1.000 |
| manual_adversarial_holdout | 140 | 0 | 140 | n/a | n/a |
| manual_independent_holdout | 36 | 36 | 0 | 1.000 | 1.000 |
| manual_near_miss_holdout | 17 | 17 | 0 | 0.941 | 0.941 |

## False Reveals

| id | top skill | expected | query |
| --- | --- | --- | --- |
| `near-miss-0320` | `skill-installer` | `skill-creator` | Generate a reusable Codex skill with frontmatter and trigger guidance; not install an existing skill from GitHub. |

## Missed Positive Reveals

| id | top skill | expected | recoverable top-3 | query |
| --- | --- | --- | ---: | --- |
| `near-miss-0320` | `skill-installer` | `skill-creator` | True | Generate a reusable Codex skill with frontmatter and trigger guidance; not install an existing skill from GitHub. |

## Validity Notes

- This is a policy/usefulness eval, not a retrieval benchmark and not a code-solving benchmark.
- A positive hit means the revealed top card matches the frozen expected/acceptable label.
- A false reveal means the runtime would spend reveal context on a card not labeled useful for that task.
- The next rung should ask whether revealed guidance improves file localization and patch planning.

