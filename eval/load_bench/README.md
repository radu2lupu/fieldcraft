# load_bench

Measures `fieldcraft-server` query latency under concurrency and checks that two servers answer
identically. Go, no dependencies.

```bash
go build -o /tmp/load_bench ./eval/load_bench

# Logged queries from a COPY of a store (never commit the output; it holds private text):
sqlite3 -json copy.sqlite3 "select query_text as q, project_id as project from retrieval_log
  where length(trim(query_text))>0 order by id desc limit 4000" | jq -c '.[]' > queries.jsonl

# Stand-in for the Gemini embedding API (deterministic vectors, optional fixed delay), so runs
# need no key and no network. Point a server at it with
# FIELDCRAFT_EMBED_GEMINI_BASE_URL=http://127.0.0.1:7790 GEMINI_API_KEY=anything
/tmp/load_bench stub -addr 127.0.0.1:7790 -delay 150ms

# p50/p95/p99/max with N concurrent workers; enrolls itself over loopback; also prints the
# numeric /status counters that changed during the run.
/tmp/load_bench run -url http://127.0.0.1:7801 -queries queries.jsonl -c 16 -n 320

# Same queries, one at a time, to two servers; exits 1 if any answer differs (ids, order, score).
/tmp/load_bench compare -a http://127.0.0.1:7801 -b http://127.0.0.1:7802 -queries queries.jsonl -n 500
```

Run servers against copies with `--data-dir`, an empty `FIELDCRAFT_SECRETS_FILE`, and the copy's
`knowledge_merge_queue` emptied, so background merges do not change the corpus mid-run.
