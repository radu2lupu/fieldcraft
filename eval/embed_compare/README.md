# embed_compare

Replays logged queries against two `fieldcraft-server`s that differ only in their embedding
provider and reports agreement (overlap@5) and how each ranks lessons agents later cited
(MRR@10, hit@1, recall@10), with a paired bootstrap 95% CI for the difference. Go, no
dependencies. Used for the stage-4 gate in
[docs/plans/local-embeddings-and-query-load.md](../../docs/plans/local-embeddings-and-query-load.md).

## Inputs (from a COPY of the store; private, never commit)

Queries whose served results contained a lesson that was then cited (`lesson:<id>`) in the same
session, plus random other queries for agreement only. Build them with indexed temp tables (a CTE
join over `json_each(result_ids)` runs for many minutes):

```sql
CREATE TEMP TABLE cites AS SELECT DISTINCT session_id, knowledge_id FROM lesson_outcome_events
  WHERE outcome='confirmed' AND source='trace_analyzer' AND note LIKE 'inline%citation%'
    AND session_id IS NOT NULL AND session_id<>'';
CREATE INDEX temp.ci ON cites(session_id, knowledge_id);
CREATE TEMP TABLE served AS SELECT r.id, r.query_text, r.project_id, r.session_id, j.value AS kid
  FROM retrieval_log r, json_each(r.result_ids) j
  WHERE r.session_id IS NOT NULL AND r.session_id<>'' AND length(trim(r.query_text))>0;
CREATE TEMP TABLE hits AS SELECT s.id, s.query_text, s.project_id, s.kid FROM served s
  JOIN cites c ON c.session_id=s.session_id AND c.knowledge_id=s.kid
  JOIN knowledge k ON k.id=s.kid AND k.state IN ('active','provisional') AND (k.tier='active' OR k.tier IS NULL);
.mode json
.output cited_raw.json
SELECT min(id) AS log_id, query_text AS q, project_id AS project,
       json_group_array(DISTINCT kid) AS cited
  FROM hits GROUP BY query_text ORDER BY (min(id) * 2654435761) % 1000003 LIMIT 400;
```

then `jq -c '.[] | .cited = (.cited|fromjson)' cited_raw.json > cited.jsonl`.

## Run

Two servers on two copies of the same reindexed store (both tags present), merge queue emptied,
an empty `FIELDCRAFT_SECRETS_FILE`; arm A with `FIELDCRAFT_EMBED_PROVIDER=gemini` and a key,
arm B with `FIELDCRAFT_EMBED_PROVIDER=gemma` and the sidecar:

```bash
go build -o /tmp/embed_compare ./eval/embed_compare
/tmp/embed_compare -a http://127.0.0.1:7801 -label-a gemini -b http://127.0.0.1:7802 -label-b gemma \
  -queries eval.jsonl -details /tmp/details.jsonl
```

Caveat: the cited lessons were picked from what the logged (Gemini-era) retrieval served, so the
metric favours the Gemini arm.
