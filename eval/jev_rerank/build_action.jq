# Variant: change-the-action question (jev-recall style) plus the context the
# Fieldcraft ranker sees but the text-only variants did not: project match,
# lesson age at query time, affected files, phase, kind.
# Input rows carry query_at; $m[0] maps id -> {kind,title,text,project_id,created_at,phase,files}.
. as $q
| ($m[0]) as $map
| ($q.result_ids | fromjson) as $ids
| ($q.query_at // "" | .[0:19]) as $qat
| ($ids | map($map[.] // {id: ., kind: "unknown", title: "", text: "", project_id: "", created_at: "", phase: null, files: "[]"})) as $cands
| def age_days(c): if ($qat | length) < 19 or ((c.created_at // "") | length) < 19 then null
    else ((($qat | strptime("%Y-%m-%dT%H:%M:%S") | mktime) - ((c.created_at | .[0:19]) | strptime("%Y-%m-%dT%H:%M:%S") | mktime)) / 86400 | floor) end;
{
  rid: $q.rid,
  request: {
    model: $model,
    state: {
      task: $q.query_text,
      project: $q.project_id,
      candidates: ($cands | to_entries | map({
        index: .key,
        kind: .value.kind,
        same_project: (.value.project_id == $q.project_id),
        age_days: age_days(.value),
        phase: .value.phase,
        files: (try (.value.files | fromjson) catch []),
        title: .value.title,
        text: .value.text
      }))
    },
    questions: (
      $cands | to_entries | map({
        key: ("c" + (.key | tostring)),
        value: {
          type: "noul",
          instructions: ("Does `candidates[" + (.key | tostring) + "]` matter for `task` in `project`? Yes: knowing this lesson would change what the engineer does, avoids, checks, or mentions while doing the task. No: it is unrelated, or it only shares a topic, tool name, or project name with the task.")
        }
      }) | from_entries
    )
  }
}
