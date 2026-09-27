#!/usr/bin/env bash
# Score served order vs Jev order against cited lessons. Args: <queries.jsonl> <out_dir>
# Prints a Markdown summary; writes per-query rows to <out_dir>/scored.tsv.
set -euo pipefail
QUERIES=$1; OUT=$2

# Per query: n candidates, best served rank of a cited lesson, best Jev rank,
# usage tokens. Ranks are 1-based. Jev order: noul desc, ties keep served order.
: > "$OUT/scored.tsv"
while IFS= read -r q; do
  rid=$(jq -r .rid <<<"$q")
  res="$OUT/responses/$rid.json"
  [[ -s "$res" ]] || continue
  jq -r --slurpfile r "$res" '
    (.result_ids | fromjson) as $ids
    | (.confirmed_ids | fromjson) as $conf
    | ($r[0].answers) as $a
    | [range(0; $ids | length)] as $idx
    # Candidate score: per-candidate Noul (build.jq) or the Choice distribution (build_choice.jq).
    | def p(i): ($a["c" + (i | tostring)].noul // $a.pick.probabilities["c" + (i | tostring)] // 0);
      ($idx | map({i: ., p: p(.)}) | sort_by(-.p, .i) | map(.i)) as $jev
    | ($idx | map(select($ids[.] as $x | $conf | index($x) != null))) as $hits
    | ($hits | map(. + 1) | min) as $served_rank
    | ($hits | map(. as $h | ($jev | index($h)) + 1) | min) as $jev_rank
    | [.rid, ($ids | length), $served_rank, $jev_rank,
       ($r[0].usage.input_tokens // 0), ($r[0].usage.output_tokens // 0),
       ($hits | map(p(.)) | max),
       ($idx | map(select(. as $i | $hits | index($i) == null)) | map(p(.)) | max // 0)]
    | @tsv' <<<"$q" >> "$OUT/scored.tsv"
done < "$QUERIES"

awk -F'\t' -v seed=20260927 '
# BSD awk has no asort; insertion sort is fine for 2000 bootstrap draws.
function isort(arr, n,   i, j, v) {
  for (i = 2; i <= n; i++) { v = arr[i]; j = i - 1; while (j >= 1 && arr[j] > v) { arr[j + 1] = arr[j]; j-- } arr[j + 1] = v }
}
{
  n++; cands[n] = $2; sr[n] = $3; jr[n] = $4; tin += $5; tout += $6; hitp[n] = $7; nonp[n] = $8
  s_h1 += ($3 == 1); j_h1 += ($4 == 1); s_h3 += ($3 <= 3); j_h3 += ($4 <= 3)
  s_mrr += 1 / $3; j_mrr += 1 / $4
  if ($4 < $3) up++; else if ($4 > $3) down++; else same++
}
END {
  if (n == 0) { print "no scored rows"; exit 1 }
  srand(seed); B = 2000; ge_mrr = 0; ge_h1 = 0
  for (b = 1; b <= B; b++) {
    dm = 0; dh = 0
    for (i = 1; i <= n; i++) { k = int(rand() * n) + 1; dm += 1 / jr[k] - 1 / sr[k]; dh += (jr[k] == 1) - (sr[k] == 1) }
    bm[b] = dm / n; bh[b] = dh / n
  }
  isort(bm, B); isort(bh, B)
  lo = int(B * 0.025) + 1; hi = int(B * 0.975)
  printf "| metric | served order | Jev order | delta | 95%% CI (paired bootstrap) |\n|---|---|---|---|---|\n"
  printf "| hit@1 | %.3f | %.3f | %+.3f | [%+.3f, %+.3f] |\n", s_h1 / n, j_h1 / n, (j_h1 - s_h1) / n, bh[lo], bh[hi]
  printf "| hit@3 | %.3f | %.3f | %+.3f | |\n", s_h3 / n, j_h3 / n, (j_h3 - s_h3) / n
  printf "| MRR | %.3f | %.3f | %+.3f | [%+.3f, %+.3f] |\n", s_mrr / n, j_mrr / n, (j_mrr - s_mrr) / n, bm[lo], bm[hi]
  printf "\nqueries scored: %d; cited lesson moved up: %d, down: %d, unchanged: %d\n", n, up, down, same
  printf "Jev tokens: %d in / %d out; at $0.042 per M input that is $%.4f\n", tin, tout, tin / 1e6 * 0.042
  s1 = 0; s2 = 0; for (i = 1; i <= n; i++) { s1 += hitp[i]; s2 += nonp[i] }
  printf "mean Jev probability: cited lesson %.3f, best non-cited candidate %.3f\n", s1 / n, s2 / n
}' "$OUT/scored.tsv"

# Latency from curl timings.
cat "$OUT"/responses/*.meta 2>/dev/null | awk '$1 == 200 {print $2}' | sort -n \
  | awk '{a[NR] = $1} END { if (NR) printf "latency: p50 %.2fs, p90 %.2fs, n=%d\n", a[int(NR * 0.5) + 1], a[int(NR * 0.9) + 1], NR }'
