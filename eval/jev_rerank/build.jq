# One TypeSafe System One request per logged query.
# Input: a query row from the export (result_ids / confirmed_ids are JSON text).
# $m[0]: lesson map {id: {id, kind, title, text}}; $model: Jev model id.
. as $q
| ($m[0]) as $map
| ($q.result_ids | fromjson) as $ids
| ($ids | map($map[.] // {id: ., kind: "unknown", title: "", text: ""})) as $cands
| {
    rid: $q.rid,
    request: {
      model: $model,
      state: {
        task: $q.query_text,
        candidates: ($cands | to_entries | map({index: .key, kind: .value.kind, title: .value.title, text: .value.text}))
      },
      questions: (
        $cands | to_entries | map({
          key: ("c" + (.key | tostring)),
          value: {
            type: "noul",
            instructions: ("Would an engineer working on `task` act on the guidance in `candidates[" + (.key | tostring) + "]`? Answer yes only if it concerns the same code, tool, command, file, or situation as the task. A lesson that merely shares the project or some vocabulary with the task is no.")
          }
        }) | from_entries
      )
    }
  }
