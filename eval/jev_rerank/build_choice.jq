# Variant: one Choice question per query, comparing the served candidates
# against each other (plus "none"). Same input/output contract as build.jq.
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
      questions: {
        pick: {
          type: "choice",
          instructions: "An engineer is about to work on `task`. Which one of `candidates` holds guidance they would act on for this exact task (same code, tool, command, file, or situation)? Choose none if no candidate is about this task rather than merely the same project.",
          criteria: (
            ($cands | to_entries | map({
              key: ("c" + (.key | tostring)),
              value: ("candidates[" + (.key | tostring) + "]: " + (.value.title | .[0:160]))
            }) | from_entries)
            + { none: "No candidate is specifically about this task" }
          )
        }
      }
    }
  }
