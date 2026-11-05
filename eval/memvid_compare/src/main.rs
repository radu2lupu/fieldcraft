//! CLI for the Memvid vs Fieldcraft comparison. See README.md for the run commands.

use chrono::{DateTime, Utc};
use memvid_compare::arms::{self, Fts, RANK_DEPTH, VectorSet, fc_ranks, gemma_ranks, guard_and_trim, ids_of};
use memvid_compare::data::{self, Corpus, EMBED_DIM, Query, Window};
use memvid_compare::guard::parse_ts;
use memvid_compare::metrics::{self, QueryScore};
use memvid_compare::report;
use memvid_compare::{embed, gemma, mv};
use serde_json::json;
use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Instant;

fn dir() -> PathBuf {
    data::eval_dir()
}
fn snapshot() -> PathBuf {
    data::snapshot_path()
}
fn mv2_path() -> PathBuf {
    dir().join("lessons.mv2")
}
fn cache_path() -> PathBuf {
    data::query_vector_cache()
}

fn all_queries(conn: &rusqlite::Connection) -> Result<Vec<Query>, String> {
    data::load_queries(conn, Window { since: None, until: None })
}

fn jev_window() -> Result<(DateTime<Utc>, DateTime<Utc>), String> {
    Ok((parse_ts(data::JEV_SINCE)?, parse_ts(data::JEV_UNTIL)?))
}

fn qvectors(queries: &[Query]) -> Result<HashMap<i64, Vec<f32>>, String> {
    let items: Vec<(i64, String)> = queries.iter().map(|q| (q.rid, q.text.clone())).collect();
    embed::embed_missing(&cache_path(), &items, EMBED_DIM)
}

/// Every row either label set needs a ranking for: the full-history rows plus any window row
/// that an earlier retrieval's claim removed from the full-history set.
fn eval_targets(conn: &rusqlite::Connection) -> Result<(Vec<Query>, Vec<Query>, Vec<Query>), String> {
    let all = all_queries(conn)?;
    let (since, until) = jev_window()?;
    let jev = data::load_queries(conn, Window { since: Some(since), until: Some(until) })?;
    let mut targets = all.clone();
    let known: std::collections::HashSet<i64> = targets.iter().map(|q| q.rid).collect();
    targets.extend(jev.iter().filter(|q| !known.contains(&q.rid)).cloned());
    Ok((all, jev, targets))
}

fn cmd_labels() -> Result<(), String> {
    let conn = data::open_snapshot(&snapshot())?;
    let all = all_queries(&conn)?;
    let (since, until) = jev_window()?;
    let win = data::load_queries(&conn, Window { since: Some(since), until: Some(until) })?;
    let win: Vec<&Query> = win.iter().collect();
    let sessions = |qs: &[&Query]| qs.iter().map(|q| q.session.as_str()).collect::<std::collections::HashSet<_>>().len();
    let all_refs: Vec<&Query> = all.iter().collect();
    println!("full history : {} queries, {} sessions, {} (session, lesson) pairs", all.len(), sessions(&all_refs), data::cited_pair_count(&all));
    let pairs: usize = win.iter().map(|q| q.cited.len()).sum();
    println!("jev window   : {} queries, {} sessions, {} (session, lesson) pairs  [{} .. {})", win.len(), sessions(&win), pairs, data::JEV_SINCE, data::JEV_UNTIL);
    println!("jev README   : 865 queries, 424 sessions, 2,370 distinct candidate lessons");
    println!(
        "recovered    : {} queries, {} sessions, {} pairs (README rows reproduced {}; pair count is a row's cited-lesson list, not the row count)",
        win.len(),
        sessions(&win),
        pairs,
        if win.len() == 865 && sessions(&win) == 424 { "exactly" } else { "approximately" }
    );
    Ok(())
}

fn cmd_embed() -> Result<(), String> {
    let conn = data::open_snapshot(&snapshot())?;
    let all = all_queries(&conn)?;
    let m = qvectors(&all)?;
    println!("query vectors cached: {} of {} queries ({})", all.iter().filter(|q| m.contains_key(&q.rid)).count(), all.len(), cache_path().display());
    Ok(())
}

fn cmd_build(lift_cap: bool) -> Result<(), String> {
    let conn = data::open_snapshot(&snapshot())?;
    let corpus = data::load_corpus(&conn, false)?;
    let stats = mv::build(&corpus, &mv2_path(), lift_cap)?;
    let size = std::fs::metadata(mv2_path()).map_err(|e| e.to_string())?.len();
    println!(
        "{}",
        json!({"op": "mv_ingest", "lift_cap": lift_cap, "lessons": corpus.len(), "with_vectors": stats.with_vectors,
               "put_s": stats.put.as_secs_f64(), "commit_s": stats.commit.as_secs_f64(),
               "total_s": stats.total.as_secs_f64(), "mv2_bytes": size})
    );
    Ok(())
}

/// Load the corpus only (no indexing): baseline for the RSS of `build`, which holds the same copy.
fn cmd_load_only() -> Result<(), String> {
    let conn = data::open_snapshot(&snapshot())?;
    let t = Instant::now();
    let corpus = data::load_corpus(&conn, false)?;
    println!("{}", json!({"op": "load_only", "lessons": corpus.len(), "load_s": t.elapsed().as_secs_f64()}));
    Ok(())
}

/// The same data (id, created_at, text, vector + FTS5 index) in a fresh SQLite file, for a size
/// comparison with the `.mv2`; the production snapshot carries many more columns and indexes.
fn cmd_sqlite_build() -> Result<(), String> {
    let conn = data::open_snapshot(&snapshot())?;
    let corpus = data::load_corpus(&conn, false)?;
    let path = dir().join("minimal.sqlite3");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let out = rusqlite::Connection::open(&path).map_err(|e| e.to_string())?;
    let t = Instant::now();
    out.execute_batch(
        "CREATE TABLE lessons(id TEXT PRIMARY KEY, created_at TEXT NOT NULL, retrieval_text TEXT NOT NULL, dense_vector BLOB);
         CREATE VIRTUAL TABLE lessons_fts USING fts5(retrieval_text, content='lessons', content_rowid='rowid');
         BEGIN;",
    )
    .map_err(|e| e.to_string())?;
    {
        let mut ins = out
            .prepare("INSERT INTO lessons(id, created_at, retrieval_text, dense_vector) VALUES (?1, ?2, ?3, ?4)")
            .map_err(|e| e.to_string())?;
        for i in 0..corpus.len() {
            let blob: Option<Vec<u8>> = corpus.has_vec[i].then(|| corpus.vector(i).iter().flat_map(|f| f.to_le_bytes()).collect());
            ins.execute(rusqlite::params![corpus.ids[i], corpus.created[i].to_rfc3339(), corpus.texts[i], blob])
                .map_err(|e| e.to_string())?;
        }
    }
    out.execute_batch(
        "INSERT INTO lessons_fts(rowid, retrieval_text) SELECT rowid, retrieval_text FROM lessons; COMMIT; VACUUM;",
    )
    .map_err(|e| e.to_string())?;
    drop(out);
    let size = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
    println!("{}", json!({"op": "sqlite_ingest", "lessons": corpus.len(), "total_s": t.elapsed().as_secs_f64(), "sqlite_bytes": size}));
    Ok(())
}

fn digest<T: Hash>(v: &T) -> String {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    format!("{:016x}", h.finish())
}

const ARMS: [&str; 10] = ["fc-bm25", "fc-dense", "fc-hybrid", "fc-served", "fc-dense-gemma", "fc-hybrid-gemma", "fc-hybrid-both", "mv-lex", "mv-vec", "mv-hybrid"];

/// Each Gemma arm against its Gemini twin (same candidate set, same fusion).
const GEMMA_TWINS: [(&str, &str); 2] = [("fc-dense-gemma", "fc-dense"), ("fc-hybrid-gemma", "fc-hybrid")]; 

type Rankings = HashMap<&'static str, Vec<String>>;

fn cmd_eval(out_dir: PathBuf, skip_mv: bool, with_gemma: bool) -> Result<(), String> {
    let conn = data::open_snapshot(&snapshot())?;
    let corpus = data::load_corpus(&conn, false)?;
    // Rankings are needed for every row in either set (see eval_targets).
    let (all, jev, targets) = eval_targets(&conn)?;
    let vecs = qvectors(&targets)?;
    let gem = if with_gemma { Some(load_gemma(&corpus, &targets)?) } else { None };
    let fts = Fts::new(&conn, &corpus);
    let mut mvi = if skip_mv { None } else { Some(mv::open(&corpus, &mv2_path())?) };
    let idx = corpus.index_of();

    let mut rankings: HashMap<i64, Rankings> = HashMap::with_capacity(targets.len());
    let mut mv_errors: HashMap<String, usize> = HashMap::new();
    let mut leaks = 0usize;
    let mut thin = 0usize;
    let started = Instant::now();
    for (n, q) in targets.iter().enumerate() {
        let qvec = &vecs[&q.rid];
        let fc = fc_ranks(&corpus, &fts, q, qvec)?;
        let gr = gem.as_ref().map(|g| gemma_ranks(&corpus, &g.set(), q, &g.qvecs[&q.rid], &fc));
        if fc.visible < 10 {
            thin += 1;
        }
        let mut r: Rankings = HashMap::new();
        r.insert("fc-bm25", fc.bm25);
        r.insert("fc-dense", fc.dense);
        r.insert("fc-hybrid", fc.hybrid);
        r.insert("fc-served", fc.served.clone());
        if let Some(gr) = gr {
            r.insert("fc-dense-gemma", gr.dense);
            r.insert("fc-hybrid-gemma", gr.hybrid);
            r.insert("fc-hybrid-both", gr.both);
        }
        if let Some(m) = mvi.as_mut() {
            let mut run = |name: &str, res: Result<Vec<usize>, String>| -> Vec<usize> {
                res.unwrap_or_else(|e| {
                    *mv_errors.entry(format!("{name}: {e}")).or_default() += 1;
                    Vec::new()
                })
            };
            let lex = run("mv-lex", m.lex(&q.text));
            let vecs_raw = run("mv-vec", m.vec(qvec));
            r.insert("mv-lex", ids_of(&corpus, &guard_and_trim(&lex, &corpus, q.at, RANK_DEPTH)));
            r.insert("mv-vec", ids_of(&corpus, &guard_and_trim(&vecs_raw, &corpus, q.at, RANK_DEPTH)));
            r.insert("mv-hybrid", ids_of(&corpus, &arms::hybrid(&corpus, q.at, &lex, &vecs_raw)));
        }
        // Time-leakage check over every guarded arm (fc-served is the logged production order).
        for (arm, ranked) in &r {
            if *arm == "fc-served" {
                continue;
            }
            leaks += ranked.iter().filter(|id| corpus.created[idx[id.as_str()]] > q.at).count();
        }
        rankings.insert(q.rid, r);
        if (n + 1) % 500 == 0 {
            eprintln!("eval {}/{} ({:.0}s)", n + 1, targets.len(), started.elapsed().as_secs_f64());
        }
    }
    println!("eval loop: {:.1}s over {} queries ({} full-history, {} label-window); future-lesson leaks: {leaks}; queries with <10 visible lessons: {thin}", started.elapsed().as_secs_f64(), targets.len(), all.len(), jev.len());
    if !mv_errors.is_empty() {
        println!("memvid errors (query counts):");
        for (e, c) in &mv_errors {
            println!("  {c} x {e}");
        }
    }
    for arm in ["fc-bm25", "fc-dense", "fc-hybrid"] {
        let d: Vec<&Vec<String>> = all.iter().map(|q| &rankings[&q.rid][arm]).collect();
        println!("digest {arm}: {}", digest(&d));
    }

    let sets: [(&str, Vec<&Query>); 2] = [
        ("jev-window", jev.iter().collect()),
        ("full-history", all.iter().collect()),
    ];
    let arm_names: Vec<&str> = ARMS.iter().copied().filter(|a| rankings[&all[0].rid].contains_key(a)).collect();
    let kinds = lesson_kinds(&conn)?;
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let mut md = String::new();
    let mut rows_out = String::new();
    for (set_name, rows) in &sets {
        let scores: HashMap<&str, Vec<QueryScore>> = arm_names
            .iter()
            .map(|a| (*a, rows.iter().map(|q| metrics::score_query(&rankings[&q.rid][a], &q.cited)).collect()))
            .collect();
        md.push_str(&report::mean_table(set_name, rows.len(), &arm_names, &scores));
        md.push_str(&report::delta_table(&report::deltas(&arm_names, &scores, "fc-hybrid"), "fc-hybrid"));
        if gem.is_some() {
            md.push_str(&gemma_section(set_name, rows, &scores, &kinds));
        }
        if *set_name == "jev-window" {
            for q in rows {
                let ranks: serde_json::Map<String, serde_json::Value> = arm_names
                    .iter()
                    .map(|a| (a.to_string(), json!(metrics::best_rank(&rankings[&q.rid][a], &q.cited))))
                    .collect();
                rows_out.push_str(&format!("{}\n", json!({"rid": q.rid, "cited": q.cited.len(), "best_rank": ranks})));
            }
        }
    }
    std::fs::write(out_dir.join("summary.md"), &md).map_err(|e| e.to_string())?;
    std::fs::write(out_dir.join("per_query_ranks.jsonl"), rows_out).map_err(|e| e.to_string())?;
    println!("{md}");
    Ok(())
}

/// Query latency (and cold open) for one engine over the first `n` label-window queries.
fn cmd_qbench(engine: &str, n: usize) -> Result<(), String> {
    let conn = data::open_snapshot(&snapshot())?;
    let all = all_queries(&conn)?;
    let (since, until) = jev_window()?;
    let qs: Vec<&Query> = all.iter().filter(|q| q.at >= since && q.at < until).take(n).collect();
    let vecs = qvectors(&all)?;
    let light = engine == "mv";
    let corpus: Corpus = data::load_corpus(&conn, light)?;
    let mut lat: Vec<(&str, Vec<f64>)> = Vec::new();
    let timed = |f: &mut dyn FnMut() -> Result<(), String>| -> Result<f64, String> {
        let t = Instant::now();
        f()?;
        Ok(t.elapsed().as_secs_f64() * 1000.0)
    };
    let mut report = json!({"op": "qbench", "engine": engine, "queries": qs.len()});
    match engine {
        "mv" => {
            let t = Instant::now();
            let mut m = mv::open(&corpus, &mv2_path())?;
            let open_ms = t.elapsed().as_secs_f64() * 1000.0;
            // Opening is lazy: the first query of each kind loads that index. Report it separately.
            let first_q = qs[0];
            let t = Instant::now();
            m.lex(&first_q.text)?;
            m.vec(&vecs[&first_q.rid])?;
            let first_ms = t.elapsed().as_secs_f64() * 1000.0;
            report["cold_open_ms"] = json!(open_ms);
            report["first_queries_ms"] = json!(first_ms);
            let (mut a, mut c) = (Vec::new(), Vec::new());
            for q in &qs {
                a.push(timed(&mut || m.lex(&q.text).map(drop))?);
                c.push(timed(&mut || m.vec(&vecs[&q.rid]).map(drop))?);
            }
            lat.push(("mv-lex", a));
            lat.push(("mv-vec", c));
        }
        "fc" => {
            let fts = Fts::new(&conn, &corpus);
            let (mut a, mut b) = (Vec::new(), Vec::new());
            for q in &qs {
                a.push(timed(&mut || fts.top(&q.text, arms::OVERFETCH).map(drop))?);
                b.push(timed(&mut || {
                    arms::dense_top(&corpus, &vecs[&q.rid], q.at, arms::OVERFETCH);
                    Ok(())
                })?);
            }
            lat.push(("fc-bm25", a));
            lat.push(("fc-dense", b));
        }
        other => return Err(format!("engine must be mv or fc, got {other}")),
    }
    let mut lats = serde_json::Map::new();
    for (name, mut v) in lat {
        v.sort_by(|a, b| a.total_cmp(b));
        lats.insert(name.to_string(), json!({"p50_ms": metrics::percentile(&v, 50.0), "p95_ms": metrics::percentile(&v, 95.0)}));
    }
    report["latency"] = serde_json::Value::Object(lats);
    println!("{report}");
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let result = match args.first().map(String::as_str) {
        Some("labels") => cmd_labels(),
        Some("embed") => cmd_embed(),
        Some("build") => cmd_build(args.iter().any(|a| a == "--lift-cap")),
        Some("load-only") => cmd_load_only(),
        Some("sqlite-build") => cmd_sqlite_build(),
        Some("eval") => cmd_eval(PathBuf::from(flag("--out").unwrap_or_else(|| "out".into())), args.iter().any(|a| a == "--skip-mv"), args.iter().any(|a| a == "--gemma")),
        Some("gemma-embed") => cmd_gemma_embed(
            &flag("--url").unwrap_or_else(|| "http://127.0.0.1:7778".into()),
            flag("--workers").and_then(|v| v.parse().ok()).unwrap_or(6),
            flag("--batch").and_then(|v| v.parse().ok()).unwrap_or(4),
            flag("--server-pid").and_then(|v| v.parse().ok()),
        ),
        Some("gemma-sanity") => cmd_gemma_sanity(flag("--n").and_then(|v| v.parse().ok()).unwrap_or(200)),
        Some("qbench") => cmd_qbench(&flag("--engine").unwrap_or_else(|| "mv".into()), flag("--n").and_then(|v| v.parse().ok()).unwrap_or(300)),
        _ => Err("usage: memvid_compare <labels|embed|build|load-only|sqlite-build|eval [--out DIR] [--skip-mv] [--gemma]|gemma-embed [--url U --workers N --batch N --server-pid P]|gemma-sanity [--n N]|qbench --engine mv|fc [--n N]>".into()),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------------------------
// Gemma G4 gate
// ---------------------------------------------------------------------------------------------

/// Cached Gemma vectors aligned to the corpus, plus the query vectors by retrieval id.
struct GemmaEval {
    dim: usize,
    vectors: Vec<f32>,
    candidates: Vec<bool>,
    qvecs: HashMap<i64, Vec<f32>>,
}

impl GemmaEval {
    fn set(&self) -> VectorSet<'_> {
        VectorSet { dim: self.dim, vectors: &self.vectors, candidates: &self.candidates }
    }
}

fn gemma_fingerprint() -> Result<String, String> {
    Ok(gemma::fingerprint(&gemma::file_sha256(&gemma::default_model_path())?))
}

fn load_gemma(corpus: &Corpus, targets: &[Query]) -> Result<GemmaEval, String> {
    let fp = gemma_fingerprint()?;
    let docs = gemma::VectorCache::open(&gemma::gemma_dir(), &fp, "docs")?;
    let queries = gemma::VectorCache::open(&gemma::gemma_dir(), &fp, "queries")?;
    let aligned = gemma::align(&docs, &corpus.ids, &corpus.has_vec);
    let mut qvecs = HashMap::with_capacity(targets.len());
    for q in targets {
        let r = queries.map.get(&q.rid.to_string()).ok_or_else(|| format!("no Gemma query vector for retrieval {} (run gemma-embed)", q.rid))?;
        qvecs.insert(q.rid, r.vector.clone());
    }
    let (with_gemini, with_gemma, cand) = (
        corpus.has_vec.iter().filter(|b| **b).count(),
        aligned.has.iter().filter(|b| **b).count(),
        aligned.candidates.iter().filter(|b| **b).count(),
    );
    println!("gemma fingerprint {fp}: lessons {} | with Gemini vector {with_gemini} | with Gemma vector {with_gemma} | dense candidate set (both) {cand}", corpus.len());
    Ok(GemmaEval { dim: gemma::DIM, vectors: aligned.vectors, candidates: aligned.candidates, qvecs })
}

fn lesson_kinds(conn: &rusqlite::Connection) -> Result<HashMap<String, String>, String> {
    let mut stmt = conn.prepare("SELECT id, kind FROM knowledge").map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
    rows.collect::<Result<HashMap<_, _>, _>>().map_err(|e| e.to_string())
}

fn word_bucket(text: &str) -> &'static str {
    match text.split_whitespace().count() {
        0..=3 => "1-3 words",
        4..=8 => "4-8 words",
        _ => "9+ words",
    }
}

/// Paired verdicts of each Gemma arm against its Gemini twin, the exploratory both-arm against
/// the hybrid twin, and mean MRR deltas by aggregate query bucket (no text).
fn gemma_section(set_name: &str, rows: &[&Query], scores: &report::Scores, kinds: &HashMap<String, String>) -> String {
    let mut md = format!("\n#### G4: Gemma vs Gemini twin, {set_name} (n = {})\n", rows.len());
    for (gem, twin) in GEMMA_TWINS.iter().copied().chain([("fc-hybrid-both", "fc-hybrid")]) {
        let d = report::deltas(&[gem, twin], scores, twin);
        md.push_str(&report::delta_table(&d, twin));
        md.push_str(&report::verdict_table(&d, twin));
        let (_, ci) = &d[0];
        let g4 = if ci[4].2 < 0.0 { "FAIL (MRR below twin beyond the CI)" } else { "pass" };
        md.push_str(&format!("\nMRR verdict {gem} vs {twin}: {} -> {g4}\n", report::verdict(ci[4], gem, twin)));
    }
    // Buckets: mean MRR(gemma) - MRR(twin) per bucket.
    for (gem, twin) in GEMMA_TWINS {
        for (label, key) in [("query length", 0usize), ("kind of first cited lesson", 1)] {
            let mut acc: std::collections::BTreeMap<String, (usize, f64, f64)> = std::collections::BTreeMap::new();
            for (i, q) in rows.iter().enumerate() {
                let bucket = if key == 0 { word_bucket(&q.text).to_string() } else { kinds.get(&q.cited[0]).cloned().unwrap_or_else(|| "unknown".into()) };
                let e = acc.entry(bucket).or_default();
                e.0 += 1;
                e.1 += scores[gem][i].rr;
                e.2 += scores[twin][i].rr;
            }
            md.push_str(&format!("\nMRR by {label}, {gem} vs {twin}:\n\n| bucket | n | {gem} | {twin} | delta |\n|---|---|---|---|---|\n"));
            for (b, (n, g, t)) in &acc {
                let n_f = *n as f64;
                md.push_str(&format!("| {b} | {n} | {:.3} | {:.3} | {:+.3} |\n", g / n_f, t / n_f, (g - t) / n_f));
            }
        }
    }
    md
}

fn rss_kb(pid: u32) -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

/// Embed every query row and every corpus lesson with the local llama-server (resumable).
fn cmd_gemma_embed(url: &str, workers: usize, batch: usize, server_pid: Option<u32>) -> Result<(), String> {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let model = gemma::default_model_path();
    let fp = gemma_fingerprint()?;
    let conn = data::open_snapshot(&snapshot())?;
    let corpus = data::load_corpus(&conn, false)?;
    let (_, _, targets) = eval_targets(&conn)?;
    let emb = gemma::Embedder::new(url)?;
    emb.verify_model()?;
    let mut docs = gemma::VectorCache::open(&gemma::gemma_dir(), &fp, "docs")?;
    let mut queries = gemma::VectorCache::open(&gemma::gemma_dir(), &fp, "queries")?;
    println!("model {} fingerprint {fp}; cached before: {} docs, {} queries", model.display(), docs.map.len(), queries.map.len());
    let peak = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let started = Instant::now();
    let mut out = Vec::new();
    std::thread::scope(|s| {
        if let Some(pid) = server_pid {
            let (peak, stop) = (&peak, &stop);
            s.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    peak.fetch_max(rss_kb(pid), Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
            });
        }
        let qitems: Vec<(String, String)> = targets.iter().map(|q| (q.rid.to_string(), q.text.clone())).collect();
        let ditems: Vec<(String, String)> = corpus.ids.iter().cloned().zip(corpus.texts.iter().cloned()).collect();
        out.push(("queries", qitems.len(), Instant::now(), emb.embed_missing(&mut queries, gemma::Role::Query, &qitems, workers, batch)));
        out.push(("docs", ditems.len(), Instant::now(), emb.embed_missing(&mut docs, gemma::Role::Document, &ditems, workers, batch)));
        stop.store(true, Ordering::Relaxed);
    });
    for (kind, total, _, r) in out {
        let st = r?;
        println!(
            "{}",
            json!({"op": "gemma_embed", "kind": kind, "texts": total, "embedded_now": st.embedded, "failed": st.failed, "clipped": st.clipped, "shrunk": st.shrunk})
        );
    }
    println!("{}", json!({"op": "gemma_embed_total", "elapsed_s": started.elapsed().as_secs_f64(), "workers": workers, "batch": batch, "server_peak_rss_mb": peak.load(Ordering::Relaxed) as f64 / 1024.0}));
    Ok(())
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn top_k(rows: &[usize], vectors: impl Fn(usize) -> Vec<f32>, query: usize, k: usize) -> Vec<usize> {
    let q = vectors(query);
    let mut s: Vec<(f32, usize)> = rows.iter().filter(|&&i| i != query).map(|&i| (dot(&q, &vectors(i)), i)).collect();
    s.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    s.into_iter().take(k).map(|(_, i)| i).collect()
}

/// Cache health and Gemma-vs-Gemini neighbour agreement over random lessons.
fn cmd_gemma_sanity(n: usize) -> Result<(), String> {
    let conn = data::open_snapshot(&snapshot())?;
    let corpus = data::load_corpus(&conn, false)?;
    let fp = gemma_fingerprint()?;
    let docs = gemma::VectorCache::open(&gemma::gemma_dir(), &fp, "docs")?;
    let queries = gemma::VectorCache::open(&gemma::gemma_dir(), &fp, "queries")?;
    let g = gemma::align(&docs, &corpus.ids, &corpus.has_vec);
    let summarize = |name: &str, c: &gemma::VectorCache| {
        let norms: Vec<f64> = c.map.values().map(|r| f64::from(r.raw_norm)).collect();
        let unit: Vec<f64> = c.map.values().map(|r| f64::from(r.vector.iter().map(|x| x * x).sum::<f32>().sqrt())).collect();
        let (lo, hi) = norms.iter().fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(*x), h.max(*x)));
        let mean = norms.iter().sum::<f64>() / norms.len().max(1) as f64;
        let (ulo, uhi) = unit.iter().fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(*x), h.max(*x)));
        let clipped = c.map.values().filter(|r| r.flags & gemma::FLAG_CLIPPED != 0).count();
        let shrunk = c.map.values().filter(|r| r.flags & gemma::FLAG_SHRUNK != 0).count();
        println!(
            "{}",
            json!({"set": name, "vectors": c.map.len(), "raw_norm_min": lo, "raw_norm_mean": mean, "raw_norm_max": hi,
                   "stored_norm_min": ulo, "stored_norm_max": uhi, "clipped_at_char_cap": clipped, "shrunk_after_too_large": shrunk})
        );
    };
    summarize("docs", &docs);
    summarize("queries", &queries);
    let missing = corpus.ids.iter().filter(|id| !docs.map.contains_key(*id)).count();
    println!("lessons without a Gemma vector: {missing} of {}; dense candidate set: {}", corpus.len(), g.candidates.iter().filter(|b| **b).count());
    let rows: Vec<usize> = (0..corpus.len()).filter(|&i| g.candidates[i]).collect();
    let mut rng = metrics::SplitMix64(20_261_101);
    let mut sample = std::collections::BTreeSet::new();
    while sample.len() < n.min(rows.len()) {
        sample.insert(rows[rng.below(rows.len())]);
    }
    let gemini = |i: usize| corpus.vector(i).to_vec();
    let gemma_v = |i: usize| g.vectors[i * gemma::DIM..(i + 1) * gemma::DIM].to_vec();
    let mut overlaps = Vec::new();
    for &i in &sample {
        let a: std::collections::HashSet<usize> = top_k(&rows, gemini, i, 10).into_iter().collect();
        let b: std::collections::HashSet<usize> = top_k(&rows, gemma_v, i, 10).into_iter().collect();
        overlaps.push(a.intersection(&b).count() as f64 / 10.0);
    }
    overlaps.sort_by(|a, b| a.total_cmp(b));
    println!(
        "{}",
        json!({"op": "overlap_at_10", "lessons": overlaps.len(), "mean": overlaps.iter().sum::<f64>() / overlaps.len().max(1) as f64,
               "p10": metrics::percentile(&overlaps, 10.0), "p50": metrics::percentile(&overlaps, 50.0), "p90": metrics::percentile(&overlaps, 90.0)})
    );
    Ok(())
}
