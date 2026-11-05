//! The navigation tree: project nodes -> group nodes -> lesson leaves, built per query from the
//! lessons visible at query time. Pure and deterministic; no LLM text anywhere in a description.

use crate::guard::visible;
use crate::meta::LessonMeta;
use chrono::{DateTime, Datelike, Utc};
use std::collections::BTreeMap;

/// A group above this many lessons is split by creation month (then by chunks of this size).
pub const SPLIT_AT: usize = 150;
pub const MISC: &str = "misc";
const TITLE_CHARS: usize = 80;
const RECENT_TITLES: usize = 3;
const TOP_DOMAINS: usize = 8;
const LEAF_TEXT_CHARS: usize = 250;

#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub label: String,
    pub members: Vec<usize>,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectNode {
    pub project: String,
    pub count: usize,
    pub description: String,
}

/// Corpus indices of lessons created at or before `at`. `meta` is sorted by creation time.
pub fn visible_prefix(meta: &[LessonMeta], at: DateTime<Utc>) -> usize {
    meta.partition_point(|m| visible(m.created, at))
}

fn clip(s: &str, n: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(n).collect()
}

fn month_key(t: DateTime<Utc>) -> String {
    format!("{:04}-{:02}", t.year(), t.month())
}

fn group_description(label: &str, members: &[usize], meta: &[LessonMeta]) -> String {
    // `members` is in creation order, so the most recent lessons are at the end.
    let recent: Vec<String> = members.iter().rev().take(RECENT_TITLES).map(|&i| clip(&meta[i].title, TITLE_CHARS)).collect();
    format!("{label}: {} lessons. Recent: {}", members.len(), recent.join("; "))
}

/// Project nodes over every lesson visible at `at`, sorted by project id.
pub fn build_projects(meta: &[LessonMeta], at: DateTime<Utc>) -> Vec<ProjectNode> {
    let prefix = visible_prefix(meta, at);
    let mut by: BTreeMap<&str, (usize, BTreeMap<&str, usize>)> = BTreeMap::new();
    for m in &meta[..prefix] {
        let e = by.entry(m.project.as_str()).or_default();
        e.0 += 1;
        if let Some(d) = m.domain.as_deref() {
            *e.1.entry(d).or_default() += 1;
        }
    }
    by.into_iter()
        .map(|(project, (count, doms))| {
            let mut doms: Vec<(&str, usize)> = doms.into_iter().collect();
            doms.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            let top: Vec<&str> = doms.iter().take(TOP_DOMAINS).map(|d| d.0).collect();
            let domains = if top.is_empty() { "none".to_string() } else { top.join(", ") };
            ProjectNode { project: project.to_string(), count, description: format!("{project}: {count} lessons; domains: {domains}") }
        })
        .collect()
}

/// Group `items` (corpus index, project label), in any order. Groups are keyed by
/// (project label, first domain cue or `misc`); a group above `SPLIT_AT` is split by creation
/// month and a month above `SPLIT_AT` into sequential chunks. Lessons created after `at` are
/// dropped, so a caller cannot leak the future even by passing the wrong pool.
pub fn build_groups(meta: &[LessonMeta], items: &[(usize, String)], at: DateTime<Utc>) -> Vec<Group> {
    let mut keyed: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (i, label) in items {
        if !visible(meta[*i].created, at) {
            continue;
        }
        let dom = meta[*i].domain.clone().unwrap_or_else(|| MISC.to_string());
        keyed.entry((label.clone(), dom)).or_default().push(*i);
    }
    let mut out = Vec::new();
    for ((project, dom), mut members) in keyed {
        members.sort_unstable(); // corpus order is creation order
        let base = format!("{project} / {dom}");
        if members.len() <= SPLIT_AT {
            out.push(Group { description: group_description(&base, &members, meta), label: base, members });
            continue;
        }
        let mut months: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for i in members {
            months.entry(month_key(meta[i].created)).or_default().push(i);
        }
        for (month, ms) in months {
            let parts = ms.len().div_ceil(SPLIT_AT);
            for (p, chunk) in ms.chunks(SPLIT_AT).enumerate() {
                let label = if parts == 1 { format!("{base} [{month}]") } else { format!("{base} [{month} part {}]", p + 1) };
                out.push(Group { description: group_description(&label, chunk, meta), label, members: chunk.to_vec() });
            }
        }
    }
    out
}

/// Leaf description: kind, title, first `LEAF_TEXT_CHARS` characters of the retrieval text.
pub fn leaf_description(m: &LessonMeta, text: &str) -> String {
    format!("[{}] {}: {}", m.kind, clip(&m.title, 160), clip(text, LEAF_TEXT_CHARS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::parse_ts;

    fn lesson(project: &str, domain: Option<&str>, title: &str, day: u32, month: u32) -> LessonMeta {
        LessonMeta {
            project: project.into(),
            kind: "claim".into(),
            title: title.into(),
            global: false,
            domain: domain.map(str::to_string),
            created: parse_ts(&format!("2026-{month:02}-{day:02}T00:00:00+00:00")).unwrap(),
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        parse_ts(s).unwrap()
    }

    fn pool(meta: &[LessonMeta], label: &str) -> Vec<(usize, String)> {
        (0..meta.len()).map(|i| (i, label.to_string())).collect()
    }

    #[test]
    fn groups_by_project_and_first_domain_with_misc_fallback() {
        let meta = vec![
            lesson("p", Some("billing"), "a", 1, 1),
            lesson("p", Some("billing"), "b", 2, 1),
            lesson("p", None, "c", 3, 1),
            lesson("p", Some("auth"), "d", 4, 1),
        ];
        let g = build_groups(&meta, &pool(&meta, "p"), at("2026-12-01T00:00:00+00:00"));
        let labels: Vec<&str> = g.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, vec!["p / auth", "p / billing", "p / misc"]);
        assert_eq!(g[1].members, vec![0, 1]);
        assert_eq!(g[2].members, vec![2]);
    }

    #[test]
    fn description_has_count_and_three_most_recent_titles() {
        let meta = vec![
            lesson("p", Some("x"), "oldest", 1, 1),
            lesson("p", Some("x"), "second", 2, 1),
            lesson("p", Some("x"), "third", 3, 1),
            lesson("p", Some("x"), "newest", 4, 1),
        ];
        let g = build_groups(&meta, &pool(&meta, "p"), at("2026-12-01T00:00:00+00:00"));
        assert_eq!(g[0].description, "p / x: 4 lessons. Recent: newest; third; second");
    }

    #[test]
    fn large_group_splits_by_month_then_by_chunk_and_every_lesson_lands_once() {
        let mut meta = Vec::new();
        for i in 0..100 {
            meta.push(lesson("p", Some("x"), &format!("jan{i}"), 1 + (i % 28) as u32, 1));
        }
        for i in 0..160 {
            meta.push(lesson("p", Some("x"), &format!("feb{i}"), 1 + (i % 28) as u32, 2));
        }
        let g = build_groups(&meta, &pool(&meta, "p"), at("2026-12-01T00:00:00+00:00"));
        let labels: Vec<&str> = g.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, vec!["p / x [2026-01]", "p / x [2026-02 part 1]", "p / x [2026-02 part 2]"]);
        assert!(g.iter().all(|g| g.members.len() <= SPLIT_AT));
        let mut all: Vec<usize> = g.iter().flat_map(|g| g.members.clone()).collect();
        all.sort_unstable();
        assert_eq!(all, (0..260).collect::<Vec<_>>());
    }

    #[test]
    fn small_group_is_not_split() {
        let meta: Vec<LessonMeta> = (0..SPLIT_AT).map(|i| lesson("p", Some("x"), &format!("t{i}"), 1 + (i % 28) as u32, 1 + (i % 3) as u32)).collect();
        assert_eq!(build_groups(&meta, &pool(&meta, "p"), at("2026-12-01T00:00:00+00:00")).len(), 1);
    }

    #[test]
    fn construction_is_deterministic_regardless_of_item_order() {
        let meta = vec![lesson("p", Some("b"), "t0", 1, 1), lesson("p", Some("a"), "t1", 2, 1), lesson("q", None, "t2", 3, 1)];
        let a = build_groups(&meta, &pool(&meta, "p"), at("2026-12-01T00:00:00+00:00"));
        let mut rev = pool(&meta, "p");
        rev.reverse();
        assert_eq!(a, build_groups(&meta, &rev, at("2026-12-01T00:00:00+00:00")));
    }

    #[test]
    fn time_guard_drops_future_lessons_from_groups_counts_and_titles() {
        let meta = vec![lesson("p", Some("x"), "past", 1, 1), lesson("p", Some("x"), "future", 1, 6)];
        let g = build_groups(&meta, &pool(&meta, "p"), at("2026-03-01T00:00:00+00:00"));
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].members, vec![0]);
        assert!(!g[0].description.contains("future"));
        let p = build_projects(&meta, at("2026-03-01T00:00:00+00:00"));
        assert_eq!(p[0].count, 1);
        assert_eq!(visible_prefix(&meta, at("2026-03-01T00:00:00+00:00")), 1);
    }

    #[test]
    fn project_node_lists_top_domains_by_count_then_name() {
        let mut meta = Vec::new();
        for d in ["z", "z", "a", "b", "b", "b"] {
            meta.push(lesson("proj", Some(d), "t", 1, 1));
        }
        meta.push(lesson("proj", None, "t", 1, 1));
        meta.push(lesson("other", None, "t", 1, 1));
        let p = build_projects(&meta, at("2026-12-01T00:00:00+00:00"));
        assert_eq!(p[0].description, "other: 1 lessons; domains: none");
        assert_eq!(p[1].description, "proj: 7 lessons; domains: b, z, a");
    }

    #[test]
    fn leaf_description_clips_text_and_flattens_whitespace() {
        let m = lesson("p", None, "A  title", 1, 1);
        let d = leaf_description(&m, &format!("line one\nline two {}", "x".repeat(1000)));
        assert!(d.starts_with("[claim] A title: line one line two x"));
        assert_eq!(d.chars().count(), "[claim] A title: ".len() + LEAF_TEXT_CHARS);
    }
}
