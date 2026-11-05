# Can Jev gate retrieval per turn? (2026-10-31)

Question: does Jev's P(prior-session notes would change the action | task text) separate retrievals
that later got a lesson confirmed from ones that did not?

Data: production snapshot of 2026-10-27, retrievals with a session id and at least one
trace-analyzer outcome on a served lesson: n = 4,780 (1,572 sessions). Positive = a lesson served by
that retrieval was confirmed (4,253, 89%); negative = all served lessons ignored/contradicted (527).
Jev (jev-latest, two Noul questions per query, 430 input tokens each, all 200s). Data and
responses stay on the Dev SSD (private query text).

Result (AUC for "retrieval will be confirmed"):
- Jev act-question: 0.547. Jev "specific work" question: 0.428. Query length alone: 0.381.
- Suppressing the 29% lowest-scored retrievals (P<0.5) loses 28% of the positives, so it is
  indistinguishable from random suppression. Positives are 89% of the base rate, so there is little to gate.

Caveats: queries are agent-written keyword strings (median ~90 chars), not user prompts, which is
what a UserPromptSubmit hook would see. "Ignored" means not cited, not useless. Labels are
session-level analyzer output. This says Jev has no usable signal on this data and label set, not that
it cannot gate.

Decision: no gate for now. The hook injects top-k on every prompt with dedupe and a latency cap and
logs what it injected, so a gate can be trained later on real hook-time data.
