//! Tree navigation with a decision model (DecisionRAG, arXiv 2610.35868) over the lesson corpus,
//! plus the paper's comparators: Jev reranking of the hybrid top 100, a final Choice over the top
//! 20, and the "+ dense candidates" union. Rankings are corpus indices, best first.

use crate::arms::{RANK_DEPTH, RRF_K, rrf_weighted};
use crate::data::Query;
use crate::jev::{self, Jev, Level, LevelStat, Ops};
use crate::meta::LessonMeta;
use crate::tree::{self, Group};
use std::collections::HashSet;

pub const PROJECT_KEEP: usize = 3;
pub const GROUP_KEEP: usize = 10;
pub const LEAF_KEEP: usize = 20;
/// Hybrid candidates unioned into the final stage of the "+ dense candidates" variant.
pub const DENSE_UNION: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Own project plus `project_specific = 0` lessons from other projects.
    Project,
    /// All projects: a project level on top, keep the best `PROJECT_KEEP`.
    Global,
}

pub struct Corpus<'a> {
    pub meta: &'a [LessonMeta],
    pub texts: &'a [String],
}

pub struct NavRun {
    /// Judged leaves by P(yes) descending, cut to `RANK_DEPTH`.
    pub ranking: Vec<usize>,
    /// Projects that survived level 0 (empty in `Mode::Project`).
    pub kept_projects: Vec<String>,
    /// Every lesson inside a kept group.
    pub kept_members: HashSet<usize>,
    pub ops: Ops,
}

/// Indices of the `k` best scores, descending; ties keep the earlier position (deterministic).
pub fn top_k(scores: &[f64], k: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..scores.len()).collect();
    idx.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    idx.truncate(k);
    idx
}

fn leaf_descs(c: &Corpus, cand: &[usize]) -> Vec<String> {
    cand.iter().map(|&i| tree::leaf_description(&c.meta[i], &c.texts[i])).collect()
}

/// Judge `cand` as leaves in shared-context requests; scores are aligned with `cand`.
pub fn score_leaves(jev: &Jev, c: &Corpus, task: &str, cand: &[usize]) -> (Vec<f64>, LevelStat) {
    jev::judge_noul(jev, Level::Lesson, task, &leaf_descs(c, cand))
}

pub fn navigate(jev: &Jev, c: &Corpus, q: &Query, mode: Mode) -> NavRun {
    let mut ops = Ops::default();
    let prefix = tree::visible_prefix(c.meta, q.at);
    let mut kept_projects = Vec::new();
    let items: Vec<(usize, String)> = match mode {
        Mode::Project => (0..prefix)
            .filter(|&i| c.meta[i].project == q.project_id || c.meta[i].global)
            .map(|i| (i, if c.meta[i].project == q.project_id { q.project_id.clone() } else { "global".to_string() }))
            .collect(),
        Mode::Global => {
            let projects = tree::build_projects(c.meta, q.at);
            let descs: Vec<String> = projects.iter().map(|p| p.description.clone()).collect();
            let (scores, stat) = jev::judge_noul(jev, Level::Project, &q.text, &descs);
            ops.push(stat);
            kept_projects = top_k(&scores, PROJECT_KEEP).into_iter().map(|j| projects[j].project.clone()).collect();
            let keep: HashSet<&str> = kept_projects.iter().map(String::as_str).collect();
            (0..prefix).filter(|&i| keep.contains(c.meta[i].project.as_str())).map(|i| (i, c.meta[i].project.clone())).collect()
        }
    };
    let groups: Vec<Group> = tree::build_groups(c.meta, &items, q.at);
    let group_descs: Vec<String> = groups.iter().map(|g| g.description.clone()).collect();
    let (gscores, stat) = jev::judge_noul(jev, Level::Group, &q.text, &group_descs);
    if !groups.is_empty() {
        ops.push(stat);
    }
    let kept = top_k(&gscores, GROUP_KEEP);
    let mut kept_members = HashSet::new();
    let mut leaves: Vec<(usize, f64)> = Vec::new(); // (lesson, parent group score)
    for &g in &kept {
        for &m in &groups[g].members {
            kept_members.insert(m);
            leaves.push((m, gscores[g]));
        }
    }
    let cand: Vec<usize> = leaves.iter().map(|l| l.0).collect();
    let (lscores, stat) = if cand.is_empty() { (Vec::new(), LevelStat::default()) } else { score_leaves(jev, c, &q.text, &cand) };
    if !cand.is_empty() {
        ops.push(stat);
    }
    let mut order: Vec<usize> = (0..cand.len()).collect();
    order.sort_by(|&a, &b| {
        lscores[b].total_cmp(&lscores[a]).then(leaves[b].1.total_cmp(&leaves[a].1)).then(cand[b].cmp(&cand[a]))
    });
    let ranking = order.into_iter().take(RANK_DEPTH).map(|j| cand[j]).collect();
    NavRun { ranking, kept_projects, kept_members, ops }
}

/// Reorder the top `LEAF_KEEP` of `ranking` by a Choice over them; everything else keeps its order.
/// A failed call leaves the ranking unchanged (counted in the stat).
pub fn apply_choice(jev: &Jev, c: &Corpus, task: &str, ranking: &[usize]) -> (Vec<usize>, LevelStat) {
    let head: Vec<usize> = ranking.iter().take(LEAF_KEEP).copied().collect();
    if head.is_empty() {
        return (Vec::new(), LevelStat::default());
    }
    let (probs, stat) = jev::judge_choice(jev, task, &leaf_descs(c, &head));
    let Some(probs) = probs else { return (ranking.to_vec(), stat) };
    let mut out: Vec<usize> = top_k(&probs, head.len()).into_iter().map(|j| head[j]).collect();
    out.extend(ranking.iter().skip(head.len()).copied());
    (out, stat)
}

/// Paper "reranking" comparator: score `cand` (in the given order) and sort by P(yes); ties keep
/// the input order. Entries of `rest` follow in their own order.
pub fn rerank(jev: &Jev, c: &Corpus, task: &str, cand: &[usize], rest: &[usize]) -> (Vec<usize>, LevelStat) {
    if cand.is_empty() {
        return (rest.to_vec(), LevelStat::default());
    }
    let (scores, stat) = score_leaves(jev, c, task, cand);
    let mut out: Vec<usize> = top_k(&scores, cand.len()).into_iter().map(|j| cand[j]).collect();
    let seen: HashSet<usize> = out.iter().copied().collect();
    out.extend(rest.iter().filter(|i| !seen.contains(i)));
    out.truncate(RANK_DEPTH);
    (out, stat)
}

/// "+ dense candidates": the navigation top `LEAF_KEEP` unioned with the hybrid top `DENSE_UNION`,
/// scored together in a fresh context (listed in corpus order so neither source is favoured by
/// position), ties broken by the best rank either source gave the lesson.
pub fn rerank_union(jev: &Jev, c: &Corpus, task: &str, nav: &[usize], hybrid: &[usize]) -> (Vec<usize>, LevelStat) {
    let nav_head = &nav[..nav.len().min(LEAF_KEEP)];
    let hyb_head = &hybrid[..hybrid.len().min(DENSE_UNION)];
    let mut cand: Vec<usize> = nav_head.iter().chain(hyb_head).copied().collect::<HashSet<_>>().into_iter().collect();
    cand.sort_unstable();
    let best_rank = |i: usize| {
        let a = nav_head.iter().position(|&x| x == i).unwrap_or(usize::MAX);
        let b = hyb_head.iter().position(|&x| x == i).unwrap_or(usize::MAX);
        a.min(b)
    };
    // Pre-order by prior rank so the tie-break in `rerank` (input order) is the prior rank.
    let mut by_prior = cand.clone();
    by_prior.sort_by_key(|&i| (best_rank(i), i));
    let rest: Vec<usize> = nav.iter().chain(hybrid).copied().collect();
    let (scores, stat) = score_leaves(jev, c, task, &cand);
    let score_of: std::collections::HashMap<usize, f64> = cand.iter().copied().zip(scores).collect();
    let mut head = by_prior;
    head.sort_by(|a, b| score_of[b].total_cmp(&score_of[a])); // stable: prior rank breaks ties
    let seen: HashSet<usize> = head.iter().copied().collect();
    let mut seen2 = seen.clone();
    head.extend(rest.into_iter().filter(|i| seen2.insert(*i)));
    head.truncate(RANK_DEPTH);
    (head, stat)
}

/// RRF of the nav ranking (weight `w`) with the hybrid ranking (weight `1 - w`), depth `RANK_DEPTH`.
pub fn blend(nav: &[usize], hybrid: &[usize], w: f32) -> Vec<usize> {
    let mut fused = rrf_weighted(&[nav.to_vec(), hybrid.to_vec()], &[w, 1.0 - w], RRF_K);
    fused.truncate(RANK_DEPTH);
    fused
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loss {
    /// A cited lesson is in the final top 20.
    Found,
    /// A cited lesson was judged (kept group) but ranked below the top 20.
    RankedBelowK,
    /// A cited lesson was in the project/global pool but its group was pruned.
    WrongGroup,
    /// No cited lesson is in the pool (own project + global, or the 3 kept projects).
    WrongProject,
}

#[derive(Debug, Clone, Copy)]
pub struct Diag {
    pub in_pool: bool,
    pub kept_group: bool,
    pub top20: bool,
    pub hybrid20: bool,
    pub union20: bool,
    pub loss: Loss,
}

/// Where a query's cited lessons are lost. "Any cited lesson" semantics: a stage counts if at
/// least one cited lesson of the query reaches it.
pub fn diagnose(cited: &[usize], in_pool: impl Fn(usize) -> bool, kept: &HashSet<usize>, nav: &[usize], hybrid: &[usize]) -> Diag {
    let any = |f: &dyn Fn(usize) -> bool| cited.iter().any(|&i| f(i));
    let top20 = &nav[..nav.len().min(LEAF_KEEP)];
    let hyb20 = &hybrid[..hybrid.len().min(DENSE_UNION)];
    let (pool, kg, t, h) = (any(&|i| in_pool(i)), any(&|i| kept.contains(&i)), any(&|i| top20.contains(&i)), any(&|i| hyb20.contains(&i)));
    let loss = if t {
        Loss::Found
    } else if kg {
        Loss::RankedBelowK
    } else if pool {
        Loss::WrongGroup
    } else {
        Loss::WrongProject
    };
    Diag { in_pool: pool, kept_group: kg, top20: t, hybrid20: h, union20: t || h, loss }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_k_orders_by_score_and_breaks_ties_by_position() {
        assert_eq!(top_k(&[0.1, 0.9, 0.9, 0.5], 3), vec![1, 2, 3]);
        assert_eq!(top_k(&[0.5, 0.5, 0.5], 2), vec![0, 1]);
        assert_eq!(top_k(&[], 5), Vec::<usize>::new());
    }

    #[test]
    fn diagnose_classifies_each_loss_stage() {
        let pool = |i: usize| i < 100;
        let kept: HashSet<usize> = [1, 2, 3].into_iter().collect();
        let nav: Vec<usize> = (1..=3).collect();
        let hyb: Vec<usize> = vec![50, 51];
        assert_eq!(diagnose(&[2], pool, &kept, &nav, &hyb).loss, Loss::Found);
        let d = diagnose(&[3], pool, &kept, &[1, 2], &hyb);
        assert_eq!((d.loss, d.kept_group, d.top20), (Loss::RankedBelowK, true, false));
        let d = diagnose(&[60], pool, &kept, &nav, &hyb);
        assert_eq!((d.loss, d.in_pool, d.kept_group), (Loss::WrongGroup, true, false));
        let d = diagnose(&[500], pool, &kept, &nav, &[500]);
        assert_eq!((d.loss, d.in_pool, d.hybrid20, d.union20), (Loss::WrongProject, false, true, true));
    }

    #[test]
    fn blend_weight_zero_is_hybrid_and_one_is_nav() {
        let nav = vec![5, 6, 7];
        let hyb = vec![1, 2, 3];
        assert_eq!(blend(&nav, &hyb, 0.0)[..3], [1, 2, 3]);
        assert_eq!(blend(&nav, &hyb, 1.0)[..3], [5, 6, 7]);
        let mid = blend(&[1, 5], &[5, 1], 0.5);
        assert_eq!(mid.len(), 2);
    }
}
