//! Map the cosine thresholds calibrated for one embedding model (tag A) onto
//! another (tag B), on a copy of a real store. Read-only.
//!
//! ```text
//! cargo run --release -p fieldcraft-retrieval --example calibrate_cosine -- \
//!   --db /path/to/copy/memory.sqlite3 \
//!   --tag-a gemini:models/gemini-embedding-001:768 \
//!   --tag-b gemma:embeddinggemma-2-Q8_0:768 [--sample 2000] [--self-queries 200]
//! ```
//!
//! Method: for a seeded sample of serving records, cosine to every other
//! serving record under both models. A threshold t_A maps to the t_B that
//! admits the same share of those pairs (quantile matching), and the report
//! shows how much the two pair sets above the thresholds agree. Known
//! duplicates (records superseded by a merge) give a second check: their
//! cosine to the canonical record under each model. `--self-queries N`
//! embeds N records' own text as queries with tag B's provider (must be the
//! configured local one) and reports query-vs-own-document cosine, the number
//! the resurrection threshold depends on.

use std::collections::HashMap;
use std::path::PathBuf;

use fieldcraft_storage::KnowledgeRepository;

const BINS: usize = 4000; // cosine in [-1, 1], 0.0005 wide

fn bin(c: f32) -> usize {
    (((c.clamp(-1.0, 1.0) + 1.0) / 2.0) * (BINS as f32 - 1.0)).round() as usize
}

fn bin_value(b: usize) -> f32 {
    b as f32 / (BINS as f32 - 1.0) * 2.0 - 1.0
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn load(repo: &KnowledgeRepository, tag: &str) -> HashMap<String, Vec<f32>> {
    let mut stmt = repo
        .connection()
        .prepare("SELECT knowledge_id, vector FROM knowledge_vectors WHERE tag = ?1")
        .unwrap();
    stmt.query_map([tag], |row| {
        let blob: Vec<u8> = row.get(1)?;
        let mut v: Vec<f32> = blob
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        normalize(&mut v);
        Ok((row.get::<_, String>(0)?, v))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = PathBuf::from(arg(&args, "--db").expect("--db"));
    let tag_a = arg(&args, "--tag-a").expect("--tag-a");
    let tag_b = arg(&args, "--tag-b").expect("--tag-b");
    let sample: usize = arg(&args, "--sample").map_or(2000, |s| s.parse().unwrap());
    let self_queries: usize = arg(&args, "--self-queries").map_or(0, |s| s.parse().unwrap());

    let repo = KnowledgeRepository::open_reader(&db).expect("open copy");
    let serving: Vec<(String, String)> = {
        let mut stmt = repo
            .connection()
            .prepare(
                "SELECT id, retrieval_text FROM knowledge
                 WHERE state IN ('active','provisional') AND (tier = 'active' OR tier IS NULL)
                 ORDER BY rowid",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let va = load(&repo, &tag_a);
    let vb = load(&repo, &tag_b);
    let both: Vec<&(String, String)> = serving
        .iter()
        .filter(|(id, _)| va.contains_key(id) && vb.contains_key(id))
        .collect();
    eprintln!(
        "serving {} | with {tag_a}: {} | with {tag_b}: {} | both: {}",
        serving.len(),
        serving.iter().filter(|(id, _)| va.contains_key(id)).count(),
        serving.iter().filter(|(id, _)| vb.contains_key(id)).count(),
        both.len()
    );

    // Seeded sample (xorshift over indices).
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut pick: Vec<usize> = (0..both.len()).collect();
    for i in (1..pick.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        pick.swap(i, (state % (i as u64 + 1)) as usize);
    }
    pick.truncate(sample.min(both.len()));

    let mut hist_a = vec![0u64; BINS];
    let mut hist_b = vec![0u64; BINS];
    let mut top1_a = Vec::new();
    let mut top1_b = Vec::new();
    // Pairs at or above a generous floor, with both cosines, for agreement.
    let mut high: Vec<(f32, f32)> = Vec::new();
    for &i in &pick {
        let (id_i, _) = both[i];
        let (ai, bi) = (&va[id_i], &vb[id_i]);
        let (mut best_a, mut best_b) = (-1.0f32, -1.0f32);
        for (j, (id_j, _)) in both.iter().enumerate() {
            if i == j {
                continue;
            }
            let ca = dot(ai, &va[id_j]);
            let cb = dot(bi, &vb[id_j]);
            hist_a[bin(ca)] += 1;
            hist_b[bin(cb)] += 1;
            best_a = best_a.max(ca);
            best_b = best_b.max(cb);
            if ca >= 0.70 || cb >= 0.70 {
                high.push((ca, cb));
            }
        }
        top1_a.push(best_a);
        top1_b.push(best_b);
    }
    let total: u64 = hist_a.iter().sum();
    let share_at_or_above =
        |hist: &[u64], t: f32| -> f64 { hist[bin(t)..].iter().sum::<u64>() as f64 / total as f64 };
    let threshold_for_share = |hist: &[u64], share: f64| -> f32 {
        let mut acc = 0u64;
        for b in (0..BINS).rev() {
            acc += hist[b];
            if acc as f64 / total as f64 >= share {
                return bin_value(b);
            }
        }
        -1.0
    };
    let quantile = |xs: &mut Vec<f32>, q: f64| -> f32 {
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        xs[((xs.len() as f64 - 1.0) * q).round() as usize]
    };

    println!(
        "sampled records: {} x {} others = {total} pairs per model",
        pick.len(),
        both.len() - 1
    );
    println!(
        "nearest-neighbour cosine p10/p50/p90  A: {:.3}/{:.3}/{:.3}  B: {:.3}/{:.3}/{:.3}",
        quantile(&mut top1_a, 0.1),
        quantile(&mut top1_a, 0.5),
        quantile(&mut top1_a, 0.9),
        quantile(&mut top1_b, 0.1),
        quantile(&mut top1_b, 0.5),
        quantile(&mut top1_b, 0.9)
    );
    for (name, t_a) in [
        ("dream_min", 0.80f32),
        ("near_twin_min / resurrection_min", 0.85),
        ("duplicate_min", 0.92),
    ] {
        let share = share_at_or_above(&hist_a, t_a);
        let t_b = threshold_for_share(&hist_b, share);
        let above_a = high.iter().filter(|(a, _)| *a >= t_a).count();
        let above_b = high.iter().filter(|(_, b)| *b >= t_b).count();
        let both_above = high.iter().filter(|(a, b)| *a >= t_a && *b >= t_b).count();
        println!(
            "{name}: A {t_a:.3} -> B {t_b:.3} (share {:.5}%); pairs above A {above_a}, above B {above_b}, both {both_above} (B keeps {:.0}% of A's, {:.0}% of B's are A's)",
            share * 100.0,
            100.0 * both_above as f64 / above_a.max(1) as f64,
            100.0 * both_above as f64 / above_b.max(1) as f64,
        );
    }

    // Known duplicates: superseded record -> its canonical record.
    let dupes: Vec<(String, String)> = {
        let mut stmt = repo
            .connection()
            .prepare(
                "SELECT id, superseded_by FROM knowledge
                 WHERE state = 'superseded' AND superseded_by IS NOT NULL",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let mut dup_a = Vec::new();
    let mut dup_b = Vec::new();
    for (d, c) in &dupes {
        if let (Some(a1), Some(a2), Some(b1), Some(b2)) =
            (va.get(d), va.get(c), vb.get(d), vb.get(c))
        {
            dup_a.push(dot(a1, a2));
            dup_b.push(dot(b1, b2));
        }
    }
    if !dup_a.is_empty() {
        println!(
            "known merged duplicates with both vectors: {} | cosine p10/p50 A: {:.3}/{:.3} B: {:.3}/{:.3}",
            dup_a.len(),
            quantile(&mut dup_a.clone(), 0.1),
            quantile(&mut dup_a.clone(), 0.5),
            quantile(&mut dup_b.clone(), 0.1),
            quantile(&mut dup_b.clone(), 0.5)
        );
    }

    if self_queries > 0 {
        let spec = fieldcraft_core::embedding::EmbeddingSpec::from_env().unwrap();
        assert_eq!(
            spec.tag().as_deref(),
            Some(tag_b.as_str()),
            "--self-queries embeds with the configured provider, which must be tag B"
        );
        let texts: Vec<String> = pick
            .iter()
            .take(self_queries)
            .map(|&i| both[i].1.clone())
            .collect();
        let queries = fieldcraft_retrieval::encode_with_spec(
            &spec,
            fieldcraft_retrieval::TextRole::Query,
            &texts,
        )
        .expect("embed self-queries");
        let mut sims: Vec<f32> = pick
            .iter()
            .take(self_queries)
            .zip(queries)
            .map(|(&i, mut q)| {
                normalize(&mut q);
                dot(&q, &vb[&both[i].0])
            })
            .collect();
        println!(
            "self-queries ({}): query-vs-own-document cosine under B p10/p50/p90 {:.3}/{:.3}/{:.3}",
            sims.len(),
            quantile(&mut sims, 0.1),
            quantile(&mut sims, 0.5),
            quantile(&mut sims, 0.9)
        );
    }
}
