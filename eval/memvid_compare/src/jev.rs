//! TypeSafe Jev client for decision-model retrieval: request builders (shared-context Noul per
//! candidate, one Choice over a shortlist), response parsing, a request-hash disk cache and a
//! bounded-parallelism, retrying caller. The API key is read from the macOS Keychain and is never
//! logged or written anywhere.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub const MODEL: &str = "jev-latest";
pub const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
/// Input price, USD per million tokens (docs.typesafe.ai/models.md; output tokens are free).
pub const PRICE_PER_MTOK: f64 = 0.042;
/// Candidates per request; the paper finds no loss from requests of 50 or more.
pub const MAX_PER_REQUEST: usize = 100;
pub const PARALLELISM: usize = 8;
const MAX_ATTEMPTS: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Project,
    Group,
    Lesson,
}

impl Level {
    fn question(self, i: usize) -> String {
        match self {
            Level::Project => format!("Is a lesson that would help with `task` likely to be in the project described by `candidates[{i}]`?"),
            Level::Group => format!("Is a lesson that would help with `task` likely to be in the group described by `candidates[{i}]`?"),
            Level::Lesson => format!(
                "Would knowing the lesson in `candidates[{i}]` change what the engineer does, avoids, checks, or mentions while doing `task`? Yes if it records a decision, pitfall, constraint or procedure that applies. No if it only shares a topic, tool or project name."
            ),
        }
    }
    fn noun(self) -> &'static str {
        match self {
            Level::Project => "Project",
            Level::Group => "Group",
            Level::Lesson => "Lesson",
        }
    }
}

/// Split `n` candidates into the fewest requests of at most `max`, as equal as possible
/// (101 -> 51 + 50), so no request is needlessly small.
pub fn balanced_chunks(n: usize, max: usize) -> Vec<std::ops::Range<usize>> {
    if n == 0 {
        return Vec::new();
    }
    let parts = n.div_ceil(max);
    let (base, extra) = (n / parts, n % parts);
    let mut out = Vec::with_capacity(parts);
    let mut start = 0;
    for p in 0..parts {
        let len = base + usize::from(p < extra);
        out.push(start..start + len);
        start += len;
    }
    out
}

fn numbered(level: Level, descs: &[String]) -> Value {
    Value::Array(descs.iter().enumerate().map(|(i, d)| json!({"n": i, "description": format!("{} {i}: {d}", level.noun())})).collect())
}

/// One request: shared state (task + numbered candidates), one Noul question per candidate.
pub fn noul_request(level: Level, task: &str, descs: &[String]) -> Value {
    let questions: serde_json::Map<String, Value> =
        (0..descs.len()).map(|i| (format!("c{i}"), json!({"type": "noul", "instructions": level.question(i)}))).collect();
    json!({"model": MODEL, "state": {"task": task, "candidates": numbered(level, descs)}, "questions": questions})
}

/// One Choice question over a shortlist (options `c0..`), as in the paper's final stage.
pub fn choice_request(task: &str, descs: &[String]) -> Value {
    let criteria: serde_json::Map<String, Value> = (0..descs.len()).map(|i| (format!("c{i}"), json!(format!("Lesson {i}")))).collect();
    json!({
        "model": MODEL,
        "state": {"task": task, "candidates": numbered(Level::Lesson, descs)},
        "questions": {"pick": {"type": "choice", "instructions": "Which lesson is most relevant to `task`?", "criteria": criteria}}
    })
}

/// P(yes) per candidate from a Noul response; `None` where an answer is missing or malformed.
pub fn parse_noul(resp: &Value, n: usize) -> Vec<Option<f64>> {
    (0..n).map(|i| resp.pointer(&format!("/answers/c{i}/noul")).and_then(Value::as_f64)).collect()
}

/// Probability per option from a Choice response; `None` unless every option is present.
pub fn parse_choice(resp: &Value, n: usize) -> Option<Vec<f64>> {
    let probs = resp.pointer("/answers/pick/probabilities")?;
    (0..n).map(|i| probs.get(format!("c{i}")).and_then(Value::as_f64)).collect()
}

pub fn request_hash(body: &Value) -> String {
    let mut h = Sha256::new();
    h.update(body.to_string().as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, Default)]
pub struct CallOutcome {
    pub resp: Option<Value>,
    pub in_tokens: u64,
    pub latency_ms: u64,
    pub cached: bool,
}

/// Counting semaphore bounding concurrent HTTP calls.
struct Sem {
    free: Mutex<usize>,
    cv: Condvar,
}

impl Sem {
    fn acquire(&self) {
        let mut g = self.free.lock().unwrap();
        while *g == 0 {
            g = self.cv.wait(g).unwrap();
        }
        *g -= 1;
    }
    fn release(&self) {
        *self.free.lock().unwrap() += 1;
        self.cv.notify_one();
    }
}

pub struct Jev {
    key: String,
    client: reqwest::blocking::Client,
    cache_dir: PathBuf,
    sem: Sem,
    /// Input tokens of uncached calls so far (the dollars actually spent).
    pub spent_tokens: AtomicU64,
    pub failed_calls: AtomicU64,
    pub retries: AtomicU64,
    budget_tokens: u64,
    pub over_budget: AtomicBool,
}

pub fn keychain_key() -> Result<String, String> {
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", "fieldcraft.jev-api-key", "-w"])
        .output()
        .map_err(|e| format!("security: {e}"))?;
    if !out.status.success() {
        return Err("Keychain service fieldcraft.jev-api-key not readable".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Jev {
    pub fn new(cache_dir: PathBuf, max_usd: f64) -> Result<Self, String> {
        std::fs::create_dir_all(&cache_dir).map_err(|e| e.to_string())?;
        Ok(Self {
            key: keychain_key()?,
            client: reqwest::blocking::Client::builder().timeout(Duration::from_secs(120)).build().map_err(|e| e.to_string())?,
            cache_dir,
            sem: Sem { free: Mutex::new(PARALLELISM), cv: Condvar::new() },
            spent_tokens: AtomicU64::new(0),
            failed_calls: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            budget_tokens: (max_usd / PRICE_PER_MTOK * 1e6) as u64,
            over_budget: AtomicBool::new(false),
        })
    }

    fn cache_path(&self, hash: &str) -> PathBuf {
        self.cache_dir.join(&hash[..2]).join(format!("{hash}.json"))
    }

    pub fn spent_usd(&self) -> f64 {
        self.spent_tokens.load(Ordering::Relaxed) as f64 / 1e6 * PRICE_PER_MTOK
    }

    /// Cached or live call. A failed call (after retries) returns `resp: None`; it is never cached.
    pub fn call(&self, body: &Value) -> CallOutcome {
        let hash = request_hash(body);
        let path = self.cache_path(&hash);
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(v) = serde_json::from_str::<Value>(&text)
            && v.get("resp").is_some()
        {
            return CallOutcome {
                in_tokens: v["in_tokens"].as_u64().unwrap_or(0),
                latency_ms: v["latency_ms"].as_u64().unwrap_or(0),
                resp: v.get("resp").cloned(),
                cached: true,
            };
        }
        if self.spent_tokens.load(Ordering::Relaxed) >= self.budget_tokens {
            self.over_budget.store(true, Ordering::Relaxed);
            return CallOutcome::default();
        }
        self.sem.acquire();
        let out = self.call_live(body);
        self.sem.release();
        match &out.resp {
            Some(resp) => {
                self.spent_tokens.fetch_add(out.in_tokens, Ordering::Relaxed);
                let rec = json!({"resp": resp, "in_tokens": out.in_tokens, "latency_ms": out.latency_ms});
                let _ = std::fs::create_dir_all(path.parent().unwrap());
                let tmp = path.with_extension(format!("tmp{}", std::process::id()));
                if std::fs::write(&tmp, rec.to_string()).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
            None => {
                self.failed_calls.fetch_add(1, Ordering::Relaxed);
            }
        }
        out
    }

    fn call_live(&self, body: &Value) -> CallOutcome {
        for attempt in 0..MAX_ATTEMPTS {
            let t = Instant::now();
            let sent = self.client.post(ENDPOINT).bearer_auth(&self.key).json(body).send();
            let latency_ms = t.elapsed().as_millis() as u64;
            if let Ok(r) = sent {
                    let status = r.status();
                    if status.is_success() {
                        if let Ok(v) = r.json::<Value>()
                            && v.get("answers").is_some()
                        {
                            let in_tokens = v.pointer("/usage/input_tokens").and_then(Value::as_u64).unwrap_or(0);
                            return CallOutcome { resp: Some(v), in_tokens, latency_ms, cached: false };
                        }
                    } else if !(status.as_u16() == 429 || status.as_u16() == 529 || status.is_server_error()) {
                        return CallOutcome::default(); // 4xx other than 429: not transient
                    }
            }
            self.retries.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis((500u64 << attempt).min(20_000)));
        }
        CallOutcome::default()
    }
}

/// Cost and latency of one judged level (one or more parallel requests).
#[derive(Debug, Clone, Default)]
pub struct LevelStat {
    pub calls: u32,
    pub cached: u32,
    pub in_tokens: u64,
    pub max_latency_ms: u64,
    pub failed_calls: u32,
    pub missing_answers: u32,
}

/// Per-query operations record: levels run one after another, requests within a level in parallel.
#[derive(Debug, Clone, Default)]
pub struct Ops {
    pub levels: Vec<LevelStat>,
}

impl Ops {
    pub fn calls(&self) -> u32 {
        self.levels.iter().map(|l| l.calls).sum()
    }
    pub fn in_tokens(&self) -> u64 {
        self.levels.iter().map(|l| l.in_tokens).sum()
    }
    pub fn failed_calls(&self) -> u32 {
        self.levels.iter().map(|l| l.failed_calls).sum()
    }
    pub fn missing_answers(&self) -> u32 {
        self.levels.iter().map(|l| l.missing_answers).sum()
    }
    /// Sequential critical path: sum over levels of the slowest request in each.
    pub fn critical_path_ms(&self) -> u64 {
        self.levels.iter().map(|l| l.max_latency_ms).sum()
    }
    pub fn push(&mut self, l: LevelStat) {
        self.levels.push(l);
    }
}

fn stat_of(outs: &[CallOutcome], missing: u32) -> LevelStat {
    LevelStat {
        calls: outs.len() as u32,
        cached: outs.iter().filter(|o| o.cached).count() as u32,
        in_tokens: outs.iter().map(|o| o.in_tokens).sum(),
        max_latency_ms: outs.iter().map(|o| o.latency_ms).max().unwrap_or(0),
        failed_calls: outs.iter().filter(|o| o.resp.is_none()).count() as u32,
        missing_answers: missing,
    }
}

/// Judge every candidate of one level with shared-context Noul requests of at most
/// `MAX_PER_REQUEST` candidates, run in parallel. Missing answers score 0.0 and are counted.
pub fn judge_noul(jev: &Jev, level: Level, task: &str, descs: &[String]) -> (Vec<f64>, LevelStat) {
    let chunks = balanced_chunks(descs.len(), MAX_PER_REQUEST);
    let outs: Vec<CallOutcome> = std::thread::scope(|s| {
        let handles: Vec<_> =
            chunks.iter().map(|r| s.spawn(move || jev.call(&noul_request(level, task, &descs[r.clone()])))).collect();
        handles.into_iter().map(|h| h.join().expect("jev worker panicked")).collect()
    });
    let mut scores = Vec::with_capacity(descs.len());
    let mut missing = 0u32;
    for (r, o) in chunks.iter().zip(&outs) {
        let parsed = o.resp.as_ref().map(|v| parse_noul(v, r.len())).unwrap_or_else(|| vec![None; r.len()]);
        for p in parsed {
            if p.is_none() {
                missing += 1;
            }
            scores.push(p.unwrap_or(0.0));
        }
    }
    (scores, stat_of(&outs, missing))
}

/// One Choice request over a shortlist. `None` probabilities when the call or parse failed.
pub fn judge_choice(jev: &Jev, task: &str, descs: &[String]) -> (Option<Vec<f64>>, LevelStat) {
    let out = jev.call(&choice_request(task, descs));
    let probs = out.resp.as_ref().and_then(|v| parse_choice(v, descs.len()));
    let missing = u32::from(probs.is_none());
    (probs, stat_of(&[out], missing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descs(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("d{i}")).collect()
    }

    #[test]
    fn chunks_are_at_most_max_balanced_and_cover_everything() {
        for n in [0usize, 1, 50, 99, 100, 101, 150, 199, 200, 201, 1106, 1500] {
            let c = balanced_chunks(n, 100);
            assert_eq!(c.iter().map(|r| r.len()).sum::<usize>(), n);
            assert!(c.iter().all(|r| r.len() <= 100));
            if let (Some(max), Some(min)) = (c.iter().map(|r| r.len()).max(), c.iter().map(|r| r.len()).min()) {
                assert!(max - min <= 1, "n={n} unbalanced {max} vs {min}");
            }
            for w in c.windows(2) {
                assert_eq!(w[0].end, w[1].start);
            }
        }
        assert_eq!(balanced_chunks(101, 100), vec![0..51, 51..101]);
        assert!(balanced_chunks(0, 100).is_empty());
    }

    #[test]
    fn noul_request_shares_one_state_and_asks_one_question_per_candidate() {
        let r = noul_request(Level::Lesson, "fix the build", &descs(3));
        assert_eq!(r["model"], MODEL);
        assert_eq!(r["state"]["task"], "fix the build");
        let cands = r["state"]["candidates"].as_array().unwrap();
        assert_eq!(cands.len(), 3);
        assert_eq!(cands[2]["description"], "Lesson 2: d2");
        let qs = r["questions"].as_object().unwrap();
        assert_eq!(qs.len(), 3);
        for i in 0..3 {
            let q = &qs[&format!("c{i}")];
            assert_eq!(q["type"], "noul");
            assert!(q["instructions"].as_str().unwrap().contains(&format!("candidates[{i}]")));
        }
        // Only one `state`: the task is not repeated per question.
        assert_eq!(r.as_object().unwrap().len(), 3);
    }

    #[test]
    fn group_and_project_questions_use_their_own_wording() {
        let g = noul_request(Level::Group, "t", &descs(1));
        assert!(g["questions"]["c0"]["instructions"].as_str().unwrap().contains("likely to be in the group"));
        let p = noul_request(Level::Project, "t", &descs(1));
        assert!(p["questions"]["c0"]["instructions"].as_str().unwrap().contains("in the project"));
        assert_eq!(p["state"]["candidates"][0]["description"], "Project 0: d0");
    }

    #[test]
    fn split_requests_renumber_candidates_locally() {
        let all = descs(120);
        let chunks = balanced_chunks(all.len(), MAX_PER_REQUEST);
        assert_eq!(chunks.len(), 2);
        let r = noul_request(Level::Lesson, "t", &all[chunks[1].clone()]);
        assert_eq!(r["state"]["candidates"][0]["description"], "Lesson 0: d60");
        assert_eq!(r["questions"].as_object().unwrap().len(), 60);
    }

    #[test]
    fn choice_request_has_one_option_per_shortlisted_lesson() {
        let r = choice_request("t", &descs(20));
        let q = &r["questions"]["pick"];
        assert_eq!(q["type"], "choice");
        assert_eq!(q["instructions"], "Which lesson is most relevant to `task`?");
        assert_eq!(q["criteria"].as_object().unwrap().len(), 20);
        assert_eq!(q["criteria"]["c19"], "Lesson 19");
    }

    #[test]
    fn parse_noul_reads_answers_and_flags_missing() {
        let resp = json!({"answers": {"c0": {"type": "noul", "noul": 0.9}, "c2": {"type": "noul", "noul": 0.1}}, "usage": {"input_tokens": 5}});
        assert_eq!(parse_noul(&resp, 3), vec![Some(0.9), None, Some(0.1)]);
    }

    #[test]
    fn parse_choice_needs_every_option() {
        let ok = json!({"answers": {"pick": {"type": "choice", "choice": "c1", "probabilities": {"c0": 0.25, "c1": 0.75}}}});
        assert_eq!(parse_choice(&ok, 2), Some(vec![0.25, 0.75]));
        assert_eq!(parse_choice(&ok, 3), None);
        assert_eq!(parse_choice(&json!({}), 1), None);
    }

    #[test]
    fn request_hash_is_stable_and_sensitive() {
        let a = noul_request(Level::Group, "t", &descs(2));
        assert_eq!(request_hash(&a), request_hash(&a.clone()));
        assert_ne!(request_hash(&a), request_hash(&noul_request(Level::Group, "u", &descs(2))));
        assert_eq!(request_hash(&a).len(), 64);
    }

    #[test]
    fn ops_critical_path_sums_level_maxima() {
        let mut o = Ops::default();
        o.push(LevelStat { calls: 3, in_tokens: 10, max_latency_ms: 400, ..Default::default() });
        o.push(LevelStat { calls: 1, in_tokens: 5, max_latency_ms: 100, ..Default::default() });
        assert_eq!((o.calls(), o.in_tokens(), o.critical_path_ms()), (4, 15, 500));
    }
}
