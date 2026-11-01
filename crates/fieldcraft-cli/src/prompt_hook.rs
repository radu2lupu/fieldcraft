//! `fieldcraft hook prompt-submit`: Claude Code UserPromptSubmit hook.
//!
//! Asks Jev (TypeSafe System One) whether notes from earlier sessions would
//! change how the engineer handles this prompt, retrieves lessons through the
//! same path as `fieldcraft query`, and injects at most three new ones as
//! additionalContext. It never blocks the prompt: every failure is logged to
//! the state dir and the process exits 0 with no output.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const JEV_URL: &str = "https://api.typesafe.ai/v1/systemone";
const JEV_MODEL: &str = "jev-latest";
const KEYCHAIN_SERVICE: &str = "fieldcraft.jev-api-key";
const JEV_TIMEOUT: Duration = Duration::from_millis(1500);
const HOOK_BUDGET: Duration = Duration::from_secs(3);
const MAX_INJECT: usize = 3;
/// Retrieve more than we inject so already-injected lessons can be skipped.
const RETRIEVE_POOL: usize = 10;
const PROMPT_STATE_CHARS: usize = 2000;
const PROMPT_QUERY_CHARS: usize = 1000;
const ASSISTANT_CHARS: usize = 600;
const SUMMARY_CHARS: usize = 300;
const DEFAULT_THRESHOLD: f64 = 0.4;
const STATE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
const TRANSCRIPT_TAIL_BYTES: u64 = 512 * 1024;

const RECALL_INSTRUCTIONS: &str = "Would notes from earlier coding sessions on this project (past decisions, bugs, constraints, procedures) change what the engineer does, avoids, checks, or mentions while handling `prompt`? Yes: the request touches a non-obvious design decision, known pitfall, convention, or recurring problem where prior experience would help. No: it is routine, self-contained, a short acknowledgement or chat, or fully answerable from the code and tools in front of the engineer.";

// ---------------------------------------------------------------------------
// Mode and threshold: the one place they are parsed.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GateMode {
    Shadow,
    Enforce,
    Off,
}

impl GateMode {
    fn as_str(self) -> &'static str {
        match self {
            GateMode::Shadow => "shadow",
            GateMode::Enforce => "enforce",
            GateMode::Off => "off",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateConfig {
    pub mode: GateMode,
    pub threshold: f64,
}

impl GateConfig {
    /// Unknown or missing mode means shadow; a threshold that is not a number
    /// in [0, 1] means 0.4.
    pub fn parse(mode: Option<&str>, threshold: Option<&str>) -> Self {
        let mode = match mode.map(|m| m.trim().to_ascii_lowercase()).as_deref() {
            Some("enforce") => GateMode::Enforce,
            Some("off") => GateMode::Off,
            _ => GateMode::Shadow,
        };
        let threshold = threshold
            .and_then(|t| t.trim().parse::<f64>().ok())
            .filter(|t| (0.0..=1.0).contains(t))
            .unwrap_or(DEFAULT_THRESHOLD);
        Self { mode, threshold }
    }
}

// ---------------------------------------------------------------------------
// Hook input
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct HookInput {
    #[serde(default)]
    pub prompt: String,
    #[serde(default, alias = "sessionId")]
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default, alias = "transcriptPath")]
    pub transcript_path: String,
}

/// Slash commands and empty prompts are never gated or retrieved.
pub fn should_skip_prompt(prompt: &str) -> bool {
    let p = prompt.trim();
    p.is_empty() || p.starts_with('/')
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// Jev
// ---------------------------------------------------------------------------

#[async_trait]
#[allow(clippy::double_must_use)] // async_trait false positive on newer clippy
pub trait JevClient: Send + Sync {
    /// Probability in [0, 1] that notes from earlier sessions would help.
    async fn recall_probability(&self, state: Value) -> Result<f64>;
}

pub fn build_jev_request(state: Value) -> Value {
    json!({
        "model": JEV_MODEL,
        "state": state,
        "questions": {"recall": {"type": "noul", "instructions": RECALL_INSTRUCTIONS}},
    })
}

pub fn jev_state(
    prompt: &str,
    project: &str,
    already_queried: bool,
    last_assistant: &str,
) -> Value {
    json!({
        "prompt": truncate_chars(prompt, PROMPT_STATE_CHARS),
        "project": project,
        "already_queried_this_session": already_queried,
        "last_assistant_message": truncate_chars(last_assistant, ASSISTANT_CHARS),
    })
}

/// Parse `{"answers":{"recall":{"type":"noul","noul":0.66}}}`.
pub fn parse_jev_response(body: &str) -> Result<f64> {
    let v: Value = serde_json::from_str(body).context("jev response is not json")?;
    let p = v["answers"]["recall"]["noul"]
        .as_f64()
        .context("jev response has no answers.recall.noul number")?;
    if !(0.0..=1.0).contains(&p) {
        anyhow::bail!("jev probability {p} out of range");
    }
    Ok(p)
}

pub struct HttpJev {
    pub url: String,
    pub key: String,
}

#[async_trait]
impl JevClient for HttpJev {
    async fn recall_probability(&self, state: Value) -> Result<f64> {
        let client = reqwest::Client::builder().timeout(JEV_TIMEOUT).build()?;
        let resp = client
            .post(&self.url)
            .bearer_auth(&self.key)
            .json(&build_jev_request(state))
            .send()
            .await
            .context("jev request failed")?;
        let status = resp.status();
        let body = resp.text().await.context("jev body read failed")?;
        if !status.is_success() {
            anyhow::bail!("jev returned {status}");
        }
        parse_jev_response(&body)
    }
}

/// Keychain first, then FIELDCRAFT_JEV_API_KEY. The key is never logged.
pub fn load_jev_key(env_value: Option<String>) -> Option<String> {
    let service = std::env::var("FIELDCRAFT_JEV_KEYCHAIN_SERVICE")
        .unwrap_or_else(|_| KEYCHAIN_SERVICE.to_string());
    let from_keychain = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", &service, "-w"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    from_keychain
        .or(env_value)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

// ---------------------------------------------------------------------------
// Transcript, query text
// ---------------------------------------------------------------------------

/// Text of the most recent assistant message in a Claude Code transcript
/// (JSONL). Reads only the tail of the file.
pub fn last_assistant_text(transcript_path: &str) -> String {
    let Ok(mut f) = fs::File::open(transcript_path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    last_assistant_text_from_jsonl(&String::from_utf8_lossy(&buf))
}

pub fn last_assistant_text_from_jsonl(jsonl: &str) -> String {
    for line in jsonl.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let is_assistant = v["type"] == "assistant" || v["message"]["role"] == "assistant";
        if !is_assistant {
            continue;
        }
        let content = &v["message"]["content"];
        let text = match content {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter(|p| p["type"] == "text")
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !text.trim().is_empty() {
            return text;
        }
    }
    String::new()
}

/// File paths and code identifiers cheaply found in the prompt.
pub fn extract_hints(prompt: &str) -> Vec<String> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?x)
            [\w.~-]*/[\w./-]+                                  # path with a slash
            | \b[\w-]+\.(?:rs|sh|py|go|ts|tsx|js|json|toml|md|sql|ya?ml|swift)\b  # file name
            | \b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b                # snake_case
            | \b[A-Z][a-z0-9]+(?:[A-Z][a-z0-9]+)+\b            # CamelCase
            | \bFIELDCRAFT_[A-Z_]+\b
            ",
        )
        .expect("static regex")
    });
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in RE.find_iter(prompt) {
        let t = m
            .as_str()
            .trim_matches(|c| c == '.' || c == ',' || c == '/');
        if t.len() >= 3 && seen.insert(t.to_string()) {
            out.push(t.to_string());
            if out.len() >= 12 {
                break;
            }
        }
    }
    out
}

pub fn build_query_text(prompt: &str) -> String {
    let base = truncate_chars(prompt.trim(), PROMPT_QUERY_CHARS);
    let hints = extract_hints(prompt);
    if hints.is_empty() {
        base
    } else {
        format!("{base}\nReferenced: {}", hints.join(" "))
    }
}

// ---------------------------------------------------------------------------
// Session state and dedupe
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionState {
    #[serde(default)]
    pub injected: Vec<String>,
}

fn safe_session_component(session_id: &str) -> String {
    session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(128)
        .collect()
}

pub fn session_state_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir.join(format!(
        "prompt-hook-{}.json",
        safe_session_component(session_id)
    ))
}

pub fn load_session_state(path: &Path) -> Option<SessionState> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

pub fn save_session_state(path: &Path, state: &SessionState) -> Result<()> {
    fs::write(path, serde_json::to_string(state)?)?;
    Ok(())
}

/// Drop `prompt-hook-*.json` files not touched for 7 days.
pub fn prune_state_files(state_dir: &Path, now: SystemTime) {
    let Ok(rd) = fs::read_dir(state_dir) else {
        return;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("prompt-hook-") && name.ends_with(".json")) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > STATE_MAX_AGE);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Lesson {
    pub id: String,
    pub title: String,
    pub content: String,
}

/// First `max` lessons whose id is not in `already`, with no repeats.
pub fn select_new(lessons: &[Lesson], already: &[String], max: usize) -> Vec<Lesson> {
    let mut seen: HashSet<&str> = already.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for l in lessons {
        if out.len() >= max {
            break;
        }
        if !l.id.is_empty() && seen.insert(l.id.as_str()) {
            out.push(l.clone());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Lesson text without the <remembered_note> wrapper that `fieldcraft query`
/// puts around each lesson, flattened to one line and capped.
pub fn lesson_body(content: &str) -> String {
    let flat = content
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("<remembered_note")
                && !l.starts_with("</remembered_note")
                && *l != "Previously remembered:"
        })
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&flat, SUMMARY_CHARS)
}

pub fn format_context(lessons: &[Lesson]) -> String {
    let mut s = String::from(
        "FIELDCRAFT MEMORY (retrieved by the prompt hook for this prompt). Notes from earlier sessions that may apply. When you act on one, mention its id inline as `lesson:<id>`; if one is wrong, run `fieldcraft feedback <id> --outcome contradicted`.\n",
    );
    for l in lessons {
        let body = lesson_body(&l.content);
        let text = if body.is_empty() {
            l.title.trim()
        } else {
            &body
        };
        s.push_str(&format!("\n- lesson:{} {}", l.id, text));
    }
    s
}

pub fn hook_output_json(additional_context: &str) -> String {
    json!({"hookSpecificOutput": {
        "hookEventName": "UserPromptSubmit",
        "additionalContext": additional_context,
    }})
    .to_string()
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Retrieved,
    Skipped,
    Failopen,
}

#[derive(Debug, Serialize)]
pub struct LogRecord {
    pub ts: String,
    pub session_id: String,
    pub project: String,
    pub mode: String,
    pub jev_p: Option<f64>,
    pub jev_ms: u64,
    pub decision: Decision,
    pub injected_ids: Vec<String>,
    pub retrieval_ms: u64,
    pub prompt_chars: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What the hook prints (if anything) plus its log line.
pub struct Outcome {
    pub stdout: Option<String>,
    pub record: LogRecord,
}

/// Retrieval seam: returns candidate lessons for the query text.
#[async_trait]
#[allow(clippy::double_must_use)]
pub trait Retriever: Send + Sync {
    async fn retrieve(&self, query: &str, limit: usize) -> Result<Vec<Lesson>>;
}

pub struct Pipeline<'a> {
    pub cfg: GateConfig,
    pub jev: Option<&'a dyn JevClient>,
    pub retriever: &'a dyn Retriever,
    pub state_dir: &'a Path,
}

impl Pipeline<'_> {
    pub async fn run(&self, input: &HookInput, project: &str) -> Outcome {
        let mut rec = LogRecord {
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            session_id: input.session_id.clone(),
            project: project.to_string(),
            mode: self.cfg.mode.as_str().to_string(),
            jev_p: None,
            jev_ms: 0,
            decision: Decision::Retrieved,
            injected_ids: vec![],
            retrieval_ms: 0,
            prompt_chars: input.prompt.chars().count(),
            error: None,
        };
        let state_path = session_state_path(self.state_dir, &input.session_id);
        let prior = load_session_state(&state_path);
        let already_queried = prior.is_some();
        let mut state = prior.unwrap_or_default();

        if self.cfg.mode != GateMode::Off {
            match self.jev {
                None => {
                    rec.decision = Decision::Failopen;
                    rec.error = Some("no jev api key".into());
                }
                Some(jev) => {
                    let last = last_assistant_text(&input.transcript_path);
                    let jev_state = jev_state(&input.prompt, project, already_queried, &last);
                    let t = Instant::now();
                    let res = jev.recall_probability(jev_state).await;
                    rec.jev_ms = t.elapsed().as_millis() as u64;
                    match res {
                        Ok(p) => rec.jev_p = Some(p),
                        Err(e) => {
                            rec.decision = Decision::Failopen;
                            rec.error = Some(format!("{e:#}"));
                        }
                    }
                }
            }
        }

        if self.cfg.mode == GateMode::Enforce && rec.jev_p.is_some_and(|p| p < self.cfg.threshold) {
            rec.decision = Decision::Skipped;
            return Outcome {
                stdout: None,
                record: rec,
            };
        }

        let t = Instant::now();
        let res = self
            .retriever
            .retrieve(&build_query_text(&input.prompt), RETRIEVE_POOL)
            .await;
        rec.retrieval_ms = t.elapsed().as_millis() as u64;
        let candidates = match res {
            Ok(c) => c,
            Err(e) => {
                rec.decision = Decision::Failopen;
                rec.error = Some(format!("retrieval: {e:#}"));
                return Outcome {
                    stdout: None,
                    record: rec,
                };
            }
        };

        let fresh = select_new(&candidates, &state.injected, MAX_INJECT);
        rec.injected_ids = fresh.iter().map(|l| l.id.clone()).collect();
        if !input.session_id.is_empty() {
            // Record the query even when nothing new was injected, so the next
            // prompt sees already_queried_this_session = true.
            state.injected.extend(rec.injected_ids.iter().cloned());
            if let Err(e) = save_session_state(&state_path, &state) {
                rec.error.get_or_insert(format!("state write: {e}"));
            }
        }
        let stdout = (!fresh.is_empty()).then(|| hook_output_json(&format_context(&fresh)));
        Outcome {
            stdout,
            record: rec,
        }
    }
}

pub fn append_log(state_dir: &Path, rec: &LogRecord) {
    let _ = fs::create_dir_all(state_dir);
    if let Ok(line) = serde_json::to_string(rec)
        && let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(state_dir.join("prompt-gate.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

// ---------------------------------------------------------------------------
// Entry point (glue to the rest of the CLI)
// ---------------------------------------------------------------------------

struct CliRetriever {
    project: String,
    session_id: String,
}

#[async_trait]
impl Retriever for CliRetriever {
    async fn retrieve(&self, query: &str, limit: usize) -> Result<Vec<Lesson>> {
        let session = (!self.session_id.is_empty()).then_some(self.session_id.as_str());
        let hits = crate::query_lessons(
            &self.project,
            query,
            session,
            limit,
            false,
            Default::default(),
            fieldcraft_core::KnowledgeTier::Active,
        )
        .await?;
        Ok(hits
            .into_iter()
            .map(|h| Lesson {
                id: h.id,
                title: h.title,
                content: h.content,
            })
            .collect())
    }
}

/// True when the hook must do nothing, using the same markers as the shell hooks.
fn disabled() -> bool {
    if std::env::var("FIELDCRAFT_HOOK_SKIP").as_deref() == Ok("1") {
        return true;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if Path::new(&home).join(".fieldcraft/DISABLED").exists() {
        return true;
    }
    // Ambient FIELDCRAFT_DISABLED is ignored; only the env file counts.
    crate::read_env_file_value(&crate::default_fieldcraft_env_file(), "FIELDCRAFT_DISABLED")
        .ok()
        .flatten()
        .as_deref()
        == Some("1")
}

pub async fn run_prompt_submit() {
    if disabled() {
        return;
    }
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return;
    }
    let Ok(input) = serde_json::from_str::<HookInput>(&raw) else {
        return;
    };
    if should_skip_prompt(&input.prompt) {
        return;
    }

    let state_dir = crate::fieldcraft_state_path("");
    prune_state_files(&state_dir, SystemTime::now());

    let project = if input.cwd.is_empty() {
        crate::derive_project_id()
    } else {
        crate::derive_project_id_from_cwd_path(Path::new(&input.cwd))
    };
    let cfg = GateConfig::parse(
        crate::fieldcraft_config_value("FIELDCRAFT_PROMPT_GATE").as_deref(),
        crate::fieldcraft_config_value("FIELDCRAFT_PROMPT_GATE_THRESHOLD").as_deref(),
    );

    let http_jev = if cfg.mode == GateMode::Off {
        None
    } else {
        load_jev_key(crate::fieldcraft_config_value("FIELDCRAFT_JEV_API_KEY")).map(|key| HttpJev {
            url: std::env::var("FIELDCRAFT_JEV_API_URL").unwrap_or_else(|_| JEV_URL.to_string()),
            key,
        })
    };
    let retriever = CliRetriever {
        project: project.clone(),
        session_id: input.session_id.clone(),
    };
    let pipeline = Pipeline {
        cfg,
        jev: http_jev.as_ref().map(|j| j as &dyn JevClient),
        retriever: &retriever,
        state_dir: &state_dir,
    };

    let outcome = match tokio::time::timeout(HOOK_BUDGET, pipeline.run(&input, &project)).await {
        Ok(o) => o,
        Err(_) => {
            append_log(
                &state_dir,
                &LogRecord {
                    ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    session_id: input.session_id.clone(),
                    project,
                    mode: cfg.mode.as_str().to_string(),
                    jev_p: None,
                    jev_ms: 0,
                    decision: Decision::Failopen,
                    injected_ids: vec![],
                    retrieval_ms: 0,
                    prompt_chars: input.prompt.chars().count(),
                    error: Some("hook budget exceeded".into()),
                },
            );
            return;
        }
    };
    append_log(&state_dir, &outcome.record);
    if let Some(out) = outcome.stdout {
        println!("{out}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn mode_and_threshold_parsing() {
        let d = GateConfig::parse(None, None);
        assert_eq!(d.mode, GateMode::Shadow);
        assert_eq!(d.threshold, 0.4);
        assert_eq!(
            GateConfig::parse(Some("enforce"), None).mode,
            GateMode::Enforce
        );
        assert_eq!(GateConfig::parse(Some(" OFF "), None).mode, GateMode::Off);
        assert_eq!(
            GateConfig::parse(Some("bogus"), None).mode,
            GateMode::Shadow
        );
        assert_eq!(GateConfig::parse(None, Some("0.7")).threshold, 0.7);
        assert_eq!(GateConfig::parse(None, Some("1.5")).threshold, 0.4);
        assert_eq!(GateConfig::parse(None, Some("abc")).threshold, 0.4);
        assert_eq!(GateConfig::parse(None, Some("0")).threshold, 0.0);
    }

    #[test]
    fn jev_response_parsing() {
        let ok = r#"{"model":"jev-1","answers":{"recall":{"type":"noul","noul":0.66}}}"#;
        assert_eq!(parse_jev_response(ok).unwrap(), 0.66);
        for bad in [
            "",
            "not json",
            "{}",
            r#"{"answers":{}}"#,
            r#"{"answers":{"recall":{"type":"noul","noul":"high"}}}"#,
            r#"{"answers":{"recall":{"type":"noul","noul":1.4}}}"#,
        ] {
            assert!(parse_jev_response(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn jev_request_shape_and_truncation() {
        let long = "x".repeat(5000);
        let st = jev_state(&long, "proj", true, &"y".repeat(900));
        assert_eq!(st["prompt"].as_str().unwrap().chars().count(), 2000);
        assert_eq!(
            st["last_assistant_message"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            600
        );
        assert_eq!(st["already_queried_this_session"], true);
        let req = build_jev_request(st);
        assert_eq!(req["model"], "jev-latest");
        assert_eq!(req["questions"]["recall"]["type"], "noul");
    }

    #[test]
    fn dedupe_selection() {
        let mk = |id: &str| Lesson {
            id: id.into(),
            title: id.into(),
            content: String::new(),
        };
        let pool = vec![mk("a"), mk("b"), mk("b"), mk("c"), mk("d"), mk("e")];
        let got = select_new(&pool, &["a".to_string()], 3);
        let ids: Vec<_> = got.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, ["b", "c", "d"]);
        assert!(
            select_new(
                &pool,
                &["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
                3
            )
            .is_empty()
        );
    }

    #[test]
    fn output_shape() {
        let l = Lesson {
            id: "abc123".into(),
            title: "Title here".into(),
            content: "Body text\nmore".into(),
        };
        let v: Value = serde_json::from_str(&hook_output_json(&format_context(&[l]))).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("lesson:abc123 Body text more"));
        assert!(ctx.contains("prompt hook"));
        let wrapped = "<remembered_note id=\"lesson:x\" kind=\"claim\" title=\"t\">\nPreviously remembered:\nReal text.\n</remembered_note>";
        assert_eq!(lesson_body(wrapped), "Real text.");
    }

    #[test]
    fn transcript_last_assistant() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"first"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"x"}]}}"#,
            "\n",
            "garbage line\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"second"},{"type":"text","text":"part"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"again"}}"#,
            "\n",
        );
        assert_eq!(last_assistant_text_from_jsonl(jsonl), "second\npart");
        assert_eq!(last_assistant_text_from_jsonl("nope"), "");
        assert_eq!(last_assistant_text("/nonexistent/transcript"), "");
    }

    #[test]
    fn hints_extracted() {
        let h = extract_hints(
            "change install/bin-hooks/stop-feedback.sh and log_retrieval_entry in QueryHit, see FIELDCRAFT_DISTILLER",
        );
        assert!(h.contains(&"install/bin-hooks/stop-feedback.sh".to_string()));
        assert!(h.contains(&"log_retrieval_entry".to_string()));
        assert!(h.contains(&"QueryHit".to_string()));
        assert!(h.contains(&"FIELDCRAFT_DISTILLER".to_string()));
        assert_eq!(build_query_text("plain words only"), "plain words only");
    }

    #[test]
    fn skip_prompts() {
        assert!(should_skip_prompt(""));
        assert!(should_skip_prompt("   "));
        assert!(should_skip_prompt("/clear"));
        assert!(!should_skip_prompt("fix the bug"));
    }

    #[test]
    fn prune_removes_only_old_hook_state() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("prompt-hook-old.json");
        let other = dir.path().join("distill.log");
        fs::write(&old, "{}").unwrap();
        fs::write(&other, "x").unwrap();
        let future = SystemTime::now() + Duration::from_secs(8 * 24 * 3600);
        prune_state_files(dir.path(), future);
        assert!(!old.exists());
        assert!(other.exists());
        let fresh = dir.path().join("prompt-hook-new.json");
        fs::write(&fresh, "{}").unwrap();
        prune_state_files(dir.path(), SystemTime::now());
        assert!(fresh.exists());
    }

    struct FakeJev(Result<f64, String>);
    #[async_trait]
    impl JevClient for FakeJev {
        async fn recall_probability(&self, _s: Value) -> Result<f64> {
            self.0.clone().map_err(|e| anyhow::anyhow!(e))
        }
    }

    struct FakeRetriever {
        lessons: Result<Vec<Lesson>, String>,
        calls: Mutex<usize>,
    }
    #[async_trait]
    impl Retriever for FakeRetriever {
        async fn retrieve(&self, _q: &str, _l: usize) -> Result<Vec<Lesson>> {
            *self.calls.lock().unwrap() += 1;
            self.lessons.clone().map_err(|e| anyhow::anyhow!(e))
        }
    }

    fn lessons(n: usize) -> Vec<Lesson> {
        (0..n)
            .map(|i| Lesson {
                id: format!("id{i}"),
                title: format!("t{i}"),
                content: format!("c{i}"),
            })
            .collect()
    }

    fn input() -> HookInput {
        HookInput {
            prompt: "change the distiller hook".into(),
            session_id: "s1".into(),
            ..Default::default()
        }
    }

    async fn run(
        mode: GateMode,
        jev: Option<&FakeJev>,
        ret: &FakeRetriever,
        dir: &Path,
    ) -> Outcome {
        Pipeline {
            cfg: GateConfig {
                mode,
                threshold: 0.4,
            },
            jev: jev.map(|j| j as &dyn JevClient),
            retriever: ret,
            state_dir: dir,
        }
        .run(&input(), "proj")
        .await
    }

    #[tokio::test]
    async fn shadow_always_retrieves_and_records_p() {
        let dir = tempfile::tempdir().unwrap();
        let ret = FakeRetriever {
            lessons: Ok(lessons(5)),
            calls: Mutex::new(0),
        };
        let o = run(GateMode::Shadow, Some(&FakeJev(Ok(0.05))), &ret, dir.path()).await;
        assert_eq!(o.record.decision, Decision::Retrieved);
        assert_eq!(o.record.jev_p, Some(0.05));
        assert_eq!(o.record.injected_ids, ["id0", "id1", "id2"]);
        assert!(o.stdout.is_some());
    }

    #[tokio::test]
    async fn enforce_skips_below_threshold_and_retrieves_above() {
        let dir = tempfile::tempdir().unwrap();
        let ret = FakeRetriever {
            lessons: Ok(lessons(2)),
            calls: Mutex::new(0),
        };
        let o = run(GateMode::Enforce, Some(&FakeJev(Ok(0.1))), &ret, dir.path()).await;
        assert_eq!(o.record.decision, Decision::Skipped);
        assert!(o.stdout.is_none());
        assert_eq!(*ret.calls.lock().unwrap(), 0);
        let o = run(GateMode::Enforce, Some(&FakeJev(Ok(0.9))), &ret, dir.path()).await;
        assert_eq!(o.record.decision, Decision::Retrieved);
        assert_eq!(*ret.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn fail_open_paths() {
        let dir = tempfile::tempdir().unwrap();
        let ret = FakeRetriever {
            lessons: Ok(lessons(1)),
            calls: Mutex::new(0),
        };
        // jev error
        let o = run(
            GateMode::Enforce,
            Some(&FakeJev(Err("boom".into()))),
            &ret,
            dir.path(),
        )
        .await;
        assert_eq!(o.record.decision, Decision::Failopen);
        assert_eq!(o.record.jev_p, None);
        assert!(o.stdout.is_some());
        // no key
        let dir2 = tempfile::tempdir().unwrap();
        let o = run(GateMode::Enforce, None, &ret, dir2.path()).await;
        assert_eq!(o.record.decision, Decision::Failopen);
        assert!(o.stdout.is_some());
        // retrieval error: silent, no output
        let bad = FakeRetriever {
            lessons: Err("down".into()),
            calls: Mutex::new(0),
        };
        let o = run(GateMode::Off, None, &bad, dir.path()).await;
        assert_eq!(o.record.decision, Decision::Failopen);
        assert!(o.stdout.is_none());
    }

    #[tokio::test]
    async fn off_never_calls_jev_and_dedupes_across_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let ret = FakeRetriever {
            lessons: Ok(lessons(5)),
            calls: Mutex::new(0),
        };
        let o = run(
            GateMode::Off,
            Some(&FakeJev(Err("must not be called".into()))),
            &ret,
            dir.path(),
        )
        .await;
        assert_eq!(o.record.decision, Decision::Retrieved);
        assert_eq!(o.record.jev_p, None);
        assert!(o.record.error.is_none());
        assert_eq!(o.record.injected_ids, ["id0", "id1", "id2"]);
        let o = run(GateMode::Off, None, &ret, dir.path()).await;
        assert_eq!(o.record.injected_ids, ["id3", "id4"]);
        let o = run(GateMode::Off, None, &ret, dir.path()).await;
        assert!(o.record.injected_ids.is_empty());
        assert!(o.stdout.is_none());
    }

    #[tokio::test]
    async fn http_jev_against_local_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn serve(status: &'static str, body: &'static str) -> String {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                assert!(req.contains("authorization: bearer k"));
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                s.write_all(resp.as_bytes()).await.unwrap();
            });
            format!("http://{addr}/v1/systemone")
        }
        let url = serve(
            "200 OK",
            r#"{"answers":{"recall":{"type":"noul","noul":0.66}}}"#,
        )
        .await;
        let j = HttpJev {
            url,
            key: "k".into(),
        };
        assert_eq!(j.recall_probability(json!({})).await.unwrap(), 0.66);
        let url = serve("401 Unauthorized", r#"{"error":"bad key"}"#).await;
        let j = HttpJev {
            url,
            key: "k".into(),
        };
        assert!(j.recall_probability(json!({})).await.is_err());
        let j = HttpJev {
            url: "http://127.0.0.1:1/x".into(),
            key: "k".into(),
        };
        assert!(j.recall_probability(json!({})).await.is_err());
    }
}
