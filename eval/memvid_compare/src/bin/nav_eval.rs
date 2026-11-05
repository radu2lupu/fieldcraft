//! Tree navigation with a decision model vs Fieldcraft's hybrid retrieval.
//! `nav_eval run [--set jev|both] [--limit N] [--out DIR] [--max-usd X]`; see
//! docs/plans/navigation-arm.md. Reuses the memvid_compare snapshot, labels, guard, hybrid arm,
//! metrics and bootstrap; adds only the navigation arms, their blends and diagnostics.

use memvid_compare::arms::{self, Fts, fc_ranks};
use memvid_compare::data::{self, EMBED_DIM, Query, Window};
use memvid_compare::jev::{Jev, Ops, PRICE_PER_MTOK};
use memvid_compare::meta::{self, LessonMeta};
use memvid_compare::metrics::{self, QueryScore};
use memvid_compare::nav::{self, Corpus as NavCorpus, Diag, Loss, Mode};
use memvid_compare::{embed, report};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

const ROOT: &str = "/Volumes/Development SSD/fieldcraft-nav-eval";
const WORKERS: usize = 8;
const WEIGHT_GRID: [f32; 11] = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0];

/// Arms that call Jev, with the arm whose ranking they extend (for cumulative cost/latency).
const API_ARMS: [(&str, Option<&str>); 7] = [
    ("jev-rerank-hybrid100", None),
    ("nav-project", None),
    ("nav-project+mc", Some("nav-project")),
    ("nav-project+dense20", Some("nav-project")),
    ("nav-global", None),
    ("nav-global+mc", Some("nav-global")),
    ("nav-global+dense20", Some("nav-global")),
];

struct QRes {
    qi: usize,
    arms: Vec<(&'static str, Vec<usize>, Ops)>,
    diags: Vec<(&'static str, Diag)>,
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn run_query(jev: &Jev, c: &NavCorpus, q: &Query, hybrid: &[usize], cited: &[usize], qi: usize) -> QRes {
    let mut arms: Vec<(&'static str, Vec<usize>, Ops)> = Vec::new();
    let mut diags = Vec::new();
    let one = |s| Ops { levels: vec![s] };
    let (rr, st) = nav::rerank(jev, c, &q.text, &hybrid[..hybrid.len().min(100)], &[]);
    arms.push(("jev-rerank-hybrid100", rr, one(st)));
    for (mode, base) in [(Mode::Project, "nav-project"), (Mode::Global, "nav-global")] {
        let run = nav::navigate(jev, c, q, mode);
        let in_pool = |i: usize| match mode {
            Mode::Project => c.meta[i].project == q.project_id || c.meta[i].global,
            Mode::Global => run.kept_projects.contains(&c.meta[i].project),
        };
        diags.push((base, nav::diagnose(cited, in_pool, &run.kept_members, &run.ranking, hybrid)));
        let (mc, st_mc) = nav::apply_choice(jev, c, &q.text, &run.ranking);
        let (un, st_un) = nav::rerank_union(jev, c, &q.text, &run.ranking, hybrid);
        let (mc_name, un_name) = if mode == Mode::Project { ("nav-project+mc", "nav-project+dense20") } else { ("nav-global+mc", "nav-global+dense20") };
        arms.push((base, run.ranking, run.ops));
        arms.push((mc_name, mc, one(st_mc)));
        arms.push((un_name, un, one(st_un)));
    }
    QRes { qi, arms, diags }
}

fn ids(corpus: &data::Corpus, idx: &[usize]) -> Vec<String> {
    arms::ids_of(corpus, idx)
}

fn mrr_of(ranking: &[usize], cited: &[usize]) -> f64 {
    ranking.iter().position(|i| cited.contains(i)).map_or(0.0, |p| 1.0 / (p + 1) as f64)
}

/// Blend weight (on the nav list) maximising MRR over `rows`.
fn tune_weight(rows: &[usize], nav_r: &[Vec<usize>], hyb: &[Vec<usize>], cited: &[Vec<usize>]) -> f32 {
    let mut best = (f64::MIN, 0.5f32);
    for w in WEIGHT_GRID {
        let m: f64 = rows.iter().map(|&r| mrr_of(&nav::blend(&nav_r[r], &hyb[r], w), &cited[r])).sum::<f64>() / rows.len().max(1) as f64;
        if m > best.0 {
            best = (m, w);
        }
    }
    best.1
}

fn pct(n: usize, d: usize) -> String {
    format!("{:.1}% ({n}/{d})", 100.0 * n as f64 / d.max(1) as f64)
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    metrics::percentile(v, p)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("run") {
        eprintln!("usage: nav_eval run [--set jev|both] [--limit N] [--out DIR] [--max-usd X]");
        std::process::exit(1);
    }
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let root = PathBuf::from(ROOT);
    let out_dir = arg(args, "--out").map(PathBuf::from).unwrap_or_else(|| root.join("out"));
    let set = arg(args, "--set").unwrap_or_else(|| "jev".into());
    let limit: Option<usize> = arg(args, "--limit").and_then(|v| v.parse().ok());
    let max_usd: f64 = arg(args, "--max-usd").and_then(|v| v.parse().ok()).unwrap_or(14.0);
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;

    let conn = data::open_snapshot(&data::snapshot_path())?;
    let corpus = data::load_corpus(&conn, false)?;
    let meta: Vec<LessonMeta> = meta::load_meta(&conn, &corpus)?;
    let all = data::load_queries(&conn, Window { since: None, until: None })?;
    let jev_rows = data::load_queries(&conn, data::jev_window()?)?;
    let jev_rids: HashSet<i64> = jev_rows.iter().map(|q| q.rid).collect();
    let full_rids: HashSet<i64> = all.iter().map(|q| q.rid).collect();
    let mut targets: Vec<Query> = match set.as_str() {
        "jev" => jev_rows.clone(),
        "both" => {
            let mut t = all.clone();
            t.extend(jev_rows.iter().filter(|q| !full_rids.contains(&q.rid)).cloned());
            t.sort_by_key(|q| q.rid);
            t
        }
        other => return Err(format!("--set must be jev or both, got {other}")),
    };
    if let Some(n) = limit {
        let total = targets.len();
        targets = (0..n.min(total)).map(|i| targets[i * total / n.min(total)].clone()).collect();
    }
    let in_jev: Vec<bool> = targets.iter().map(|q| jev_rids.contains(&q.rid)).collect();
    let in_full: Vec<bool> = targets.iter().map(|q| full_rids.contains(&q.rid)).collect();
    let items: Vec<(i64, String)> = targets.iter().map(|q| (q.rid, q.text.clone())).collect();
    let vecs = embed::embed_missing(&data::query_vector_cache(), &items, EMBED_DIM)?;
    let idx = corpus.index_of();

    // fc-hybrid and fc-served (existing arms) for every target.
    let fts = Fts::new(&conn, &corpus);
    let started = Instant::now();
    let mut hybrid: Vec<Vec<usize>> = Vec::new();
    let mut served: Vec<Vec<String>> = Vec::new();
    for q in &targets {
        let fc = fc_ranks(&corpus, &fts, q, &vecs[&q.rid])?;
        hybrid.push(fc.hybrid.iter().filter_map(|id| idx.get(id.as_str()).copied()).collect());
        served.push(fc.served);
    }
    let cited: Vec<Vec<usize>> = targets.iter().map(|q| q.cited.iter().filter_map(|id| idx.get(id.as_str()).copied()).collect()).collect();
    eprintln!("fc ranks for {} queries in {:.0}s", targets.len(), started.elapsed().as_secs_f64());

    // Navigation arms (live Jev calls, cached on disk).
    let jev = Jev::new(root.join("cache"), max_usd)?;
    let c = NavCorpus { meta: &meta, texts: &corpus.texts };
    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<QRes>> = Mutex::new(Vec::new());
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..WORKERS {
            s.spawn(|| {
                loop {
                    let qi = next.fetch_add(1, Ordering::Relaxed);
                    if qi >= targets.len() || jev.over_budget.load(Ordering::Relaxed) {
                        break;
                    }
                    let r = run_query(&jev, &c, &targets[qi], &hybrid[qi], &cited[qi], qi);
                    let mut d = done.lock().unwrap();
                    d.push(r);
                    if d.len().is_multiple_of(100) {
                        eprintln!("nav {}/{} ({:.0}s, ${:.3} spent)", d.len(), targets.len(), t0.elapsed().as_secs_f64(), jev.spent_usd());
                    }
                }
            });
        }
    });
    if jev.over_budget.load(Ordering::Relaxed) {
        return Err(format!("budget of ${max_usd} reached after ${:.3}; stopped without a report", jev.spent_usd()));
    }
    let wall = t0.elapsed().as_secs_f64();
    let mut results = done.into_inner().unwrap();
    results.sort_by_key(|r| r.qi);
    let n = targets.len();

    // Rankings per arm, indexed by query position.
    let mut rank_idx: HashMap<&str, Vec<Vec<usize>>> = HashMap::new();
    let mut ops: HashMap<&str, Vec<Ops>> = HashMap::new();
    let mut diags: HashMap<&str, Vec<Diag>> = HashMap::new();
    for r in &results {
        for (name, ranking, o) in &r.arms {
            rank_idx.entry(name).or_default().push(ranking.clone());
            ops.entry(name).or_default().push(o.clone());
        }
        for (name, d) in &r.diags {
            diags.entry(name).or_default().push(*d);
        }
    }
    rank_idx.insert("fc-hybrid", hybrid.clone());

    // Offline blends. Weight tuned on odd-indexed jev-window queries, evaluated on even ones,
    // and the reverse (cross-fit), so every query is scored with a weight it did not tune.
    let jev_pos: Vec<usize> = (0..n).filter(|&i| in_jev[i]).collect();
    let odd: Vec<usize> = jev_pos.iter().enumerate().filter(|(k, _)| k % 2 == 1).map(|(_, &i)| i).collect();
    let even: Vec<usize> = jev_pos.iter().enumerate().filter(|(k, _)| k % 2 == 0).map(|(_, &i)| i).collect();
    let mut blend_md = String::from("\n### Blend weight tuning (jev-window; weight is on the nav list, 1 - w on fc-hybrid; tuned by MRR)\n\n| nav arm | w tuned on odd | w tuned on even |\n|---|---|---|\n");
    let mut rrf_names: Vec<(&str, &str, &str)> = Vec::new();
    for (base, rrf50, rrfw) in [("nav-project", "rrf50-nav-project", "rrf-tuned-nav-project"), ("nav-global", "rrf50-nav-global", "rrf-tuned-nav-global")] {
        let nav_r = &rank_idx[base];
        let w_odd = if odd.is_empty() { 0.5 } else { tune_weight(&odd, nav_r, &hybrid, &cited) };
        let w_even = if even.is_empty() { 0.5 } else { tune_weight(&even, nav_r, &hybrid, &cited) };
        blend_md.push_str(&format!("| {base} | {w_odd:.1} | {w_even:.1} |\n"));
        let mut plain = Vec::new();
        let mut cross = Vec::new();
        for i in 0..n {
            plain.push(nav::blend(&nav_r[i], &hybrid[i], 0.5));
            let w = if even.contains(&i) { w_odd } else { w_even };
            cross.push(nav::blend(&nav_r[i], &hybrid[i], w));
        }
        rank_idx.insert(rrf50, plain);
        rank_idx.insert(rrfw, cross);
        rrf_names.push((base, rrf50, rrfw));
    }

    let order: Vec<&str> = vec![
        "fc-hybrid", "fc-served", "jev-rerank-hybrid100", "nav-project", "nav-project+mc", "nav-global", "nav-global+mc",
        "rrf50-nav-project", "rrf-tuned-nav-project", "rrf50-nav-global", "rrf-tuned-nav-global", "nav-project+dense20", "nav-global+dense20",
    ];
    let served_ids: HashMap<usize, Vec<String>> = served.into_iter().enumerate().collect();

    let mut md = String::new();
    md.push_str(&format!(
        "# Navigation vs hybrid: run report\n\nqueries run: {n} (set={set}, limit={limit:?}); Jev model {}, ${PRICE_PER_MTOK}/M input tokens.\n",
        memvid_compare::jev::MODEL
    ));
    let scores_for = |rows: &[usize]| -> report::Scores<'_> {
        order
            .iter()
            .map(|a| {
                let v: Vec<QueryScore> = rows
                    .iter()
                    .map(|&i| {
                        let ranked = if *a == "fc-served" { served_ids[&i].clone() } else { ids(&corpus, &rank_idx[a][i]) };
                        metrics::score_query(&ranked, &targets[i].cited)
                    })
                    .collect();
                (*a, v)
            })
            .collect()
    };
    let full_pos: Vec<usize> = (0..n).filter(|&i| in_full[i]).collect();
    let mut sets: Vec<(&str, Vec<usize>)> = vec![("jev-window", jev_pos.clone())];
    if set == "both" {
        sets.push(("full-history", full_pos));
    }
    for (name, rows) in &sets {
        let sc = scores_for(rows);
        md.push_str(&report::mean_table(name, rows.len(), &order, &sc));
        let d = report::deltas(&order, &sc, "fc-hybrid");
        md.push_str(&report::delta_table(&d, "fc-hybrid"));
        md.push_str(&report::verdict_table(&d, "fc-hybrid"));
    }
    // Halves of the jev window.
    md.push_str(&blend_md);
    for (label, rows) in [("odd-indexed half", &odd), ("even-indexed half", &even)] {
        if rows.is_empty() {
            continue;
        }
        let sc = scores_for(rows);
        md.push_str(&report::mean_table(&format!("jev-window, {label}"), rows.len(), &order, &sc));
    }

    // Complementarity and pruning diagnostics (jev-window).
    md.push_str("\n### Pruning diagnostics (jev-window; a stage counts when any cited lesson of the query reaches it)\n");
    for (base, _, _) in &rrf_names {
        let rows: Vec<&Diag> = jev_pos.iter().map(|&i| &diags[base][i]).collect();
        let m = rows.len();
        let cnt = |f: &dyn Fn(&Diag) -> bool| rows.iter().filter(|d| f(d)).count();
        let pool_label = if *base == "nav-project" { "(a0) cited lesson in the pool (own project + global)" } else { "(a0) cited lesson in the 3 kept projects" };
        md.push_str(&format!(
            "\n**{base}** (n = {m})\n\n| stage | share of queries |\n|---|---|\n| {pool_label} | {} |\n| (a) cited lesson in a kept group | {} |\n| (b) cited lesson in the final top-20 | {} |\n| (c) cited lesson in fc-hybrid top-20 | {} |\n| (d) in the union of (b) and (c) | {} |\n| only nav finds it (b, not c) | {} |\n| only hybrid finds it (c, not b) | {} |\n",
            pct(cnt(&|d| d.in_pool), m), pct(cnt(&|d| d.kept_group), m), pct(cnt(&|d| d.top20), m),
            pct(cnt(&|d| d.hybrid20), m), pct(cnt(&|d| d.union20), m),
            pct(cnt(&|d| d.top20 && !d.hybrid20), m), pct(cnt(&|d| !d.top20 && d.hybrid20), m)
        ));
        let loss = |l: Loss| cnt(&|d| d.loss == l);
        md.push_str(&format!(
            "\nWhere the cited lesson is lost:\n\n| outcome | queries |\n|---|---|\n| found in final top-20 | {} |\n| lost: ranked below k (kept group, not top-20) | {} |\n| lost: wrong group (in pool, group pruned) | {} |\n| lost: wrong project (not in pool) | {} |\n",
            pct(loss(Loss::Found), m), pct(loss(Loss::RankedBelowK), m), pct(loss(Loss::WrongGroup), m), pct(loss(Loss::WrongProject), m)
        ));
    }

    // Operations: incremental per arm; cumulative adds the arm it extends.
    md.push_str("\n### Operations (per query; tokens counted whether or not served from cache)\n\n| arm | calls/q | input tokens/q | $/q | latency median s | latency p95 s | failed calls | missing answers |\n|---|---|---|---|---|---|---|---|\n");
    let mut total_tokens = 0u64;
    for (arm, base) in API_ARMS {
        let o = &ops[arm];
        let b = base.map(|b| &ops[b]);
        let tok: u64 = o.iter().map(Ops::in_tokens).sum();
        total_tokens += tok;
        let calls: u32 = o.iter().map(Ops::calls).sum();
        let mut lat: Vec<f64> = (0..o.len()).map(|i| (o[i].critical_path_ms() + b.map_or(0, |b| b[i].critical_path_ms())) as f64 / 1000.0).collect();
        let (med, p95) = (percentile(&mut lat.clone(), 50.0), percentile(&mut lat, 95.0));
        let label = base.map_or_else(|| arm.to_string(), |b| format!("{arm} (adds to {b}; latency is cumulative)"));
        md.push_str(&format!(
            "| {label} | {:.1} | {:.0} | ${:.5} | {med:.2} | {p95:.2} | {} | {} |\n",
            calls as f64 / n as f64, tok as f64 / n as f64, tok as f64 / n as f64 / 1e6 * PRICE_PER_MTOK,
            o.iter().map(Ops::failed_calls).sum::<u32>(), o.iter().map(Ops::missing_answers).sum::<u32>()
        ));
    }
    let per_q_usd = total_tokens as f64 / n as f64 / 1e6 * PRICE_PER_MTOK;
    md.push_str(&format!(
        "\nAll Jev arms together: {:.0} input tokens and ${per_q_usd:.5} per query. Extrapolation at this per-query mean: 865 queries = ${:.2}, 2,708 queries = ${:.2}.\nThis run: wall {wall:.0}s with {WORKERS} parallel calls; spent ${:.4} on uncached calls; failed calls after retries {}; transient retries {}.\n",
        total_tokens as f64 / n as f64, per_q_usd * 865.0, per_q_usd * 2708.0, jev.spent_usd(),
        jev.failed_calls.load(Ordering::Relaxed), jev.retries.load(Ordering::Relaxed)
    ));

    let tag = format!("{set}{}", limit.map(|l| format!("-n{l}")).unwrap_or_default());
    std::fs::write(out_dir.join(format!("report-{tag}.md")), &md).map_err(|e| e.to_string())?;
    let mut rows = String::new();
    for i in 0..n {
        let best: serde_json::Map<String, serde_json::Value> =
            order.iter().map(|a| (a.to_string(), json!(if *a == "fc-served" { metrics::best_rank(&served_ids[&i], &targets[i].cited) } else { metrics::best_rank(&ids(&corpus, &rank_idx[a][i]), &targets[i].cited) }))).collect();
        rows.push_str(&format!("{}\n", json!({"rid": targets[i].rid, "cited": targets[i].cited.len(), "best_rank": best})));
    }
    std::fs::write(out_dir.join(format!("per_query-{tag}.jsonl")), rows).map_err(|e| e.to_string())?;
    println!("{md}");
    Ok(())
}
