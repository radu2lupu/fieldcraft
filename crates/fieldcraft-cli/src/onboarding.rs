//! `fieldcraft setup` (interactive onboarding) and `fieldcraft doctor`
//! (non-interactive checklist).
//!
//! Setup collects the choices that used to live in install.sh flags and file
//! comments (store mode, embedding key, distiller), stores the key in
//! `~/.fieldcraft/secrets.env`, hands the wiring (service, token, hooks,
//! instructions) to install.sh, then runs the doctor so a new user ends with
//! a verified install instead of a silent half-working one.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;

use super::{
    default_fieldcraft_env_file, default_fieldcraft_repo, fieldcraft_auth_token,
    fieldcraft_config_value, fieldcraft_server_url, is_loopback_fieldcraft_url, run_checked,
};

const EMBED_KEYS: [&str; 2] = ["GEMINI_API_KEY", "GOOGLE_API_KEY"];
const START_MARKER: &str = "<!-- fieldcraft:start -->";
const HOOK_SCRIPTS: [&str; 2] = ["session-start-query.sh", "stop-feedback.sh"];
const DISTILLERS: [&str; 4] = ["none", "codex", "grok", "claude"];

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    Grok,
}

impl Agent {
    const ALL: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Grok];

    fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Grok => "Grok",
        }
    }

    fn cli(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Grok => "grok",
        }
    }

    fn config_dir(self, home: &Path) -> PathBuf {
        home.join(match self {
            Agent::Claude => ".claude",
            Agent::Codex => ".codex",
            Agent::Grok => ".grok",
        })
    }

    /// Where install.sh merges the hook fragments for this agent.
    fn hooks_config(self, home: &Path) -> PathBuf {
        match self {
            Agent::Claude => home.join(".claude/settings.json"),
            Agent::Codex => home.join(".codex/hooks.json"),
            Agent::Grok => home.join(".grok/hooks/fieldcraft.json"),
        }
    }

    /// Where install.sh injects the markered instruction block.
    fn instructions_file(self, home: &Path) -> PathBuf {
        match self {
            Agent::Claude => home.join(".claude/CLAUDE.md"),
            Agent::Codex => home.join(".codex/AGENTS.md"),
            Agent::Grok => home.join(".grok/rules/fieldcraft.md"),
        }
    }
}

/// Agents present on this machine: CLI on PATH or config directory exists.
pub fn detect_agents(home: &Path) -> Vec<Agent> {
    detect_agents_with(home, on_path)
}

fn detect_agents_with(home: &Path, cli_present: impl Fn(&str) -> bool) -> Vec<Agent> {
    Agent::ALL
        .iter()
        .copied()
        .filter(|agent| cli_present(agent.cli()) || agent.config_dir(home).is_dir())
        .collect()
}

/// True when a Claude/Codex/Grok hooks file runs `script` under any event.
pub fn hooks_config_has(config: &serde_json::Value, script: &str) -> bool {
    let Some(events) = config.get("hooks").and_then(|h| h.as_object()) else {
        return false;
    };
    events.values().any(|entries| {
        entries.as_array().is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(|hooks| hooks.as_array())
                    .is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("command")
                                .and_then(|command| command.as_str())
                                .is_some_and(|command| command.contains(script))
                        })
                    })
            })
        })
    })
}

// ---------------------------------------------------------------------------
// Secrets file helpers
// ---------------------------------------------------------------------------

/// Set `key=value` in the secrets file, replacing an existing line for `key`
/// and leaving every other line untouched. Creates the file owner-only.
pub fn write_secret(path: &Path, key: &str, value: &str) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let line = format!("export {key}={}", shell_quote(value));
    let mut replaced = false;
    let mut out: Vec<String> = existing
        .lines()
        .map(|raw| {
            let trimmed = raw.trim_start();
            let body = trimmed.strip_prefix("export ").unwrap_or(trimmed);
            if !trimmed.starts_with('#') && body.split_once('=').is_some_and(|(k, _)| k.trim() == key)
            {
                replaced = true;
                line.clone()
            } else {
                raw.to_string()
            }
        })
        .collect();
    if existing.is_empty() {
        out.push("# Fieldcraft secrets. Owner-only. install.sh never rewrites this file.".to_string());
    }
    if !replaced {
        out.push(line);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, out.join("\n") + "\n")
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(unix)]
fn world_or_group_readable(path: &Path) -> Option<bool> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o077 != 0)
}

#[cfg(not(unix))]
fn world_or_group_readable(_path: &Path) -> Option<bool> {
    None
}

/// One embedContent call with a candidate key, without touching the process
/// environment or the secrets cache.
async fn test_gemini_key(key: &str) -> Result<usize> {
    let base = std::env::var("FIELDCRAFT_EMBED_GEMINI_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string());
    let model = std::env::var("FIELDCRAFT_EMBED_GEMINI_MODEL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "models/gemini-embedding-001".to_string());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let resp = client
        .post(format!("{}/{model}:embedContent", base.trim_end_matches('/')))
        .header("x-goog-api-key", key)
        .json(&serde_json::json!({
            "model": model,
            "content": { "parts": [{ "text": "fieldcraft setup" }] },
            "outputDimensionality": 768
        }))
        .send()
        .await
        .context("embedding request failed")?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    if !status.is_success() {
        let message = body["error"]["message"]
            .as_str()
            .unwrap_or("no error message")
            .to_string();
        anyhow::bail!("HTTP {status}: {message}");
    }
    body["embedding"]["values"]
        .as_array()
        .map(Vec::len)
        .filter(|n| *n > 0)
        .context("response had no embedding values")
}

// ---------------------------------------------------------------------------
// Doctor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
}

#[derive(Debug, Default, Serialize)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
}

impl DoctorReport {
    fn push(&mut self, name: &str, level: Level, detail: impl Into<String>) {
        self.checks.push(Check {
            name: name.to_string(),
            level,
            detail: detail.into(),
        });
    }

    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.level == Level::Fail)
    }

    pub fn print(&self) {
        for check in &self.checks {
            let tag = match check.level {
                Level::Ok => "ok  ",
                Level::Warn => "warn",
                Level::Fail => "FAIL",
                Level::Skip => "skip",
            };
            println!("  [{tag}] {:<18} {}", check.name, check.detail);
        }
        let fails = self.checks.iter().filter(|c| c.level == Level::Fail).count();
        let warns = self.checks.iter().filter(|c| c.level == Level::Warn).count();
        println!();
        if fails == 0 {
            println!("All checks passed ({warns} warning(s)).");
        } else {
            println!("{fails} check(s) failed, {warns} warning(s).");
        }
    }
}

enum Mode {
    NoServer,
    LocalService(String),
    Remote(String),
}

fn current_mode() -> Mode {
    match fieldcraft_server_url() {
        None => Mode::NoServer,
        Some(url) if is_loopback_fieldcraft_url(&url) => Mode::LocalService(url),
        Some(url) => Mode::Remote(url),
    }
}

pub async fn run_doctor(offline: bool) -> DoctorReport {
    let mut report = DoctorReport::default();
    let home = home_dir();

    // Binaries.
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".to_string());
    report.push(
        "binaries",
        Level::Ok,
        format!("fieldcraft {} at {exe}", env!("CARGO_PKG_VERSION")),
    );
    let server_bin = home.join(".cargo/bin/fieldcraft-server");
    if server_bin.is_file() || on_path("fieldcraft-server") {
        report.push("server-binary", Level::Ok, "fieldcraft-server installed");
    } else {
        report.push(
            "server-binary",
            Level::Warn,
            "fieldcraft-server not found in ~/.cargo/bin (needed to run a local service)",
        );
    }

    // Config.
    let env_file = default_fieldcraft_env_file();
    let mode = current_mode();
    if !env_file.exists() {
        report.push(
            "config",
            Level::Fail,
            format!("{} missing; run `fieldcraft setup`", env_file.display()),
        );
    } else {
        let detail = match &mode {
            Mode::NoServer => "no server URL configured; commands use the local catalog".to_string(),
            Mode::LocalService(url) => format!("local service at {url}"),
            Mode::Remote(url) => format!("shared server at {url}"),
        };
        report.push("config", Level::Ok, detail);
    }
    let token = fieldcraft_auth_token();
    match (&mode, &token) {
        (Mode::NoServer, _) => {}
        (_, Some(_)) => report.push("auth-token", Level::Ok, "token present in ~/.fieldcraft/env"),
        (_, None) => report.push(
            "auth-token",
            Level::Fail,
            "server configured but no FIELDCRAFT_AUTH_TOKEN; run `fieldcraft setup`",
        ),
    }

    // Secrets and the embedding key.
    let secrets_path = fieldcraft_core::secrets::secrets_file_path();
    if secrets_path.exists() {
        match world_or_group_readable(&secrets_path) {
            Some(true) => report.push(
                "secrets-file",
                Level::Warn,
                format!("{} is readable by others; chmod 600", secrets_path.display()),
            ),
            _ => report.push("secrets-file", Level::Ok, secrets_path.display().to_string()),
        }
    } else {
        report.push(
            "secrets-file",
            Level::Warn,
            format!("{} does not exist", secrets_path.display()),
        );
    }
    let key = fieldcraft_core::secrets::lookup_any(&EMBED_KEYS);
    match (&key, &mode) {
        (Some((_, source)), _) => report.push(
            "embedding-key",
            Level::Ok,
            format!("gemini key from {source:?}"),
        ),
        (None, Mode::Remote(_)) => report.push(
            "embedding-key",
            Level::Warn,
            "no gemini key on this machine (only the server needs one)",
        ),
        (None, _) => report.push(
            "embedding-key",
            Level::Fail,
            "no GEMINI_API_KEY in env or secrets.env: retrieval is keyword-only and merge, resurrection and dream stay inert",
        ),
    }

    // Live embedding call.
    if offline {
        report.push("embedding-call", Level::Skip, "offline");
    } else if key.is_some() {
        let call = tokio::task::spawn_blocking(|| {
            fieldcraft_retrieval::encode_dense(&["fieldcraft doctor".to_string()])
        });
        match tokio::time::timeout(Duration::from_secs(25), call).await {
            Ok(Ok(Ok(vectors))) => report.push(
                "embedding-call",
                Level::Ok,
                format!("gemini answered ({} dims)", vectors.first().map_or(0, Vec::len)),
            ),
            Ok(Ok(Err(err))) => report.push("embedding-call", Level::Fail, err),
            Ok(Err(err)) => report.push("embedding-call", Level::Fail, err.to_string()),
            Err(_) => report.push("embedding-call", Level::Fail, "timed out after 25s"),
        }
    } else {
        report.push("embedding-call", Level::Skip, "no key");
    }

    // Server.
    let server_url = match &mode {
        Mode::NoServer => None,
        Mode::LocalService(url) | Mode::Remote(url) => Some(url.clone()),
    };
    match (&server_url, offline) {
        (None, _) => report.push("server", Level::Skip, "no server configured"),
        (Some(_), true) => report.push("server", Level::Skip, "offline"),
        (Some(url), false) => check_server(&mut report, url, token.as_deref()).await,
    }

    // Agents, hooks, instructions.
    let agents = detect_agents(&home);
    if agents.is_empty() {
        report.push(
            "agents",
            Level::Warn,
            "no Claude Code, Codex or Grok found (CLI on PATH or config dir)",
        );
    } else {
        let names: Vec<&str> = agents.iter().map(|a| a.name()).collect();
        report.push("agents", Level::Ok, names.join(", "));
    }
    let hooks_dir = home.join(".fieldcraft/hooks");
    let missing: Vec<&str> = HOOK_SCRIPTS
        .iter()
        .copied()
        .filter(|s| !hooks_dir.join(s).is_file())
        .collect();
    if missing.is_empty() {
        report.push("hook-scripts", Level::Ok, hooks_dir.display().to_string());
    } else {
        report.push(
            "hook-scripts",
            Level::Fail,
            format!("missing in {}: {}", hooks_dir.display(), missing.join(", ")),
        );
    }
    for agent in &agents {
        let config = agent.hooks_config(&home);
        let name = format!("hooks:{}", agent.cli());
        let parsed = std::fs::read_to_string(&config)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        match parsed {
            Some(json) if HOOK_SCRIPTS.iter().all(|s| hooks_config_has(&json, s)) => {
                report.push(&name, Level::Ok, config.display().to_string());
            }
            Some(_) => report.push(
                &name,
                Level::Fail,
                format!("{} lacks the fieldcraft hooks; rerun install", config.display()),
            ),
            None => report.push(
                &name,
                Level::Fail,
                format!("{} missing or not JSON", config.display()),
            ),
        }
        let instructions = agent.instructions_file(&home);
        let name = format!("instructions:{}", agent.cli());
        let has_block = std::fs::read_to_string(&instructions)
            .map(|text| text.contains(START_MARKER))
            .unwrap_or(false);
        if has_block {
            report.push(&name, Level::Ok, instructions.display().to_string());
        } else {
            report.push(
                &name,
                Level::Fail,
                format!("{} has no fieldcraft block; rerun install", instructions.display()),
            );
        }
    }

    // Distiller.
    let distiller = fieldcraft_config_value("FIELDCRAFT_DISTILLER")
        .map(|v| v.trim().to_lowercase())
        .unwrap_or_default();
    match distiller.as_str() {
        "none" => report.push("distiller", Level::Ok, "off"),
        "codex" | "grok" | "claude" => {
            if on_path(&distiller) {
                report.push("distiller", Level::Ok, format!("{distiller} (on PATH)"))
            } else {
                report.push(
                    "distiller",
                    Level::Fail,
                    format!("FIELDCRAFT_DISTILLER={distiller} but `{distiller}` is not on PATH"),
                )
            }
        }
        "" | "auto" => report.push(
            "distiller",
            Level::Warn,
            "auto: first of codex, grok, claude found; runs it with approvals bypassed on transcripts",
        ),
        other => report.push("distiller", Level::Fail, format!("unknown distiller `{other}`")),
    }

    // Query round trip through the real command path.
    if offline {
        report.push("query", Level::Skip, "offline");
    } else {
        check_query_round_trip(&mut report).await;
    }

    report
}

async fn check_server(report: &mut DoctorReport, url: &str, token: Option<&str>) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            report.push("server", Level::Fail, err.to_string());
            return;
        }
    };
    match client.get(format!("{url}/health")).send().await {
        Ok(resp) if resp.status().is_success() => {
            report.push("server", Level::Ok, format!("{url}/health ok"))
        }
        Ok(resp) => {
            report.push("server", Level::Fail, format!("{url}/health returned {}", resp.status()));
            return;
        }
        Err(err) => {
            report.push("server", Level::Fail, format!("{url}/health: {err}"));
            return;
        }
    }
    let Some(token) = token else {
        return;
    };
    match client
        .get(format!("{url}/status?project=global"))
        .bearer_auth(token)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            report.push("server-auth", Level::Ok, "token accepted by /status");
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            match body.get("embedding_key_configured").and_then(|v| v.as_bool()) {
                Some(true) => report.push("server-embeddings", Level::Ok, "server has an embedding key"),
                Some(false) => report.push(
                    "server-embeddings",
                    Level::Fail,
                    "server has NO embedding key: its retrieval is keyword-only",
                ),
                None => report.push(
                    "server-embeddings",
                    Level::Warn,
                    "server predates this check; upgrade it or read its startup log",
                ),
            }
        }
        Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => report.push(
            "server-auth",
            Level::Fail,
            "token rejected (401); re-enroll with `fieldcraft setup`",
        ),
        Ok(resp) => report.push(
            "server-auth",
            Level::Warn,
            format!("/status returned {}", resp.status()),
        ),
        Err(err) => report.push("server-auth", Level::Fail, err.to_string()),
    }
}

async fn check_query_round_trip(report: &mut DoctorReport) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            report.push("query", Level::Fail, err.to_string());
            return;
        }
    };
    let run = tokio::process::Command::new(exe)
        .args(["query", "--limit", "1", "fieldcraft doctor round trip"])
        .output();
    match tokio::time::timeout(Duration::from_secs(45), run).await {
        Ok(Ok(output)) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let notes = stdout.matches("<remembered_note").count();
            report.push(
                "query",
                Level::Ok,
                format!("`fieldcraft query` round trip ok ({notes} note(s) returned)"),
            );
        }
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let last = stderr.lines().last().unwrap_or("no output").to_string();
            report.push(
                "query",
                Level::Fail,
                format!("`fieldcraft query` exited {}: {last}", output.status),
            );
        }
        Ok(Err(err)) => report.push("query", Level::Fail, err.to_string()),
        Err(_) => report.push("query", Level::Fail, "`fieldcraft query` timed out after 45s"),
    }
}

pub async fn cmd_doctor(json: bool, offline: bool) -> Result<()> {
    let report = run_doctor(offline).await;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("fieldcraft doctor");
        report.print();
    }
    if report.failed() {
        anyhow::bail!("doctor found failing checks");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

/// Minimal prompts over stdin; with `yes` every prompt returns its default.
struct Prompter {
    yes: bool,
}

impl Prompter {
    fn ask(&self, prompt: &str, default: Option<&str>) -> Result<String> {
        if self.yes {
            return Ok(default.unwrap_or("").to_string());
        }
        match default {
            Some(d) if !d.is_empty() => print!("{prompt} [{d}]: "),
            _ => print!("{prompt}: "),
        }
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        let line = line.trim();
        Ok(if line.is_empty() {
            default.unwrap_or("").to_string()
        } else {
            line.to_string()
        })
    }

    fn choose(&self, prompt: &str, options: &[&str], default: usize) -> Result<usize> {
        if self.yes {
            return Ok(default);
        }
        println!("{prompt}");
        for (i, option) in options.iter().enumerate() {
            println!("  {}) {option}", i + 1);
        }
        loop {
            let answer = self.ask("Choice", Some(&default.to_string()))?;
            match answer.parse::<usize>() {
                Ok(n) if (1..=options.len()).contains(&n) => return Ok(n),
                _ => println!("  enter a number between 1 and {}", options.len()),
            }
        }
    }

    fn confirm(&self, prompt: &str, default: bool) -> Result<bool> {
        if self.yes {
            return Ok(default);
        }
        let answer = self.ask(
            &format!("{prompt} {}", if default { "[Y/n]" } else { "[y/N]" }),
            None,
        )?;
        Ok(match answer.to_lowercase().as_str() {
            "" => default,
            "y" | "yes" => true,
            _ => false,
        })
    }

    fn secret(&self, prompt: &str) -> Result<String> {
        if self.yes {
            return Ok(String::new());
        }
        let value = rpassword::prompt_password(format!("{prompt}: "))?;
        Ok(value.trim().to_string())
    }
}

enum StoreChoice {
    Local,
    Remote {
        url: String,
        enroll_secret: Option<String>,
        auth_token: Option<String>,
    },
}

pub async fn run_setup(repo: Option<PathBuf>, yes: bool) -> Result<()> {
    let repo = repo.unwrap_or_else(default_fieldcraft_repo);
    let install_script = repo.join("install.sh");
    if !install_script.exists() {
        anyhow::bail!(
            "install.sh not found at {}. Pass --repo PATH for the Fieldcraft checkout.",
            install_script.display()
        );
    }
    let ui = Prompter { yes };
    let home = home_dir();

    println!("Fieldcraft setup");
    println!("================");
    println!("Memory for coding agents that keeps score. This wizard configures where lessons are");
    println!("stored, the embedding key, the background distiller, and the agent hooks, then checks");
    println!("that everything works. Re-run it any time; it is safe on an existing install.");
    println!();
    let agents = detect_agents(&home);
    if agents.is_empty() {
        println!("Agents: none detected (Claude Code, Codex or Grok). Hooks will be installed for");
        println!("Claude Code and Codex anyway; they activate when those tools appear.");
    } else {
        let names: Vec<&str> = agents.iter().map(|a| a.name()).collect();
        println!("Agents detected: {}", names.join(", "));
    }
    println!();

    // 1. Store mode.
    let persisted_remote = fieldcraft_config_value("FIELDCRAFT_SERVER_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .filter(|u| !is_loopback_fieldcraft_url(u));
    let default_mode = if persisted_remote.is_some() { 2 } else { 1 };
    let mode_pick = ui.choose(
        "Where should lessons live?",
        &[
            "On this machine (a local service and SQLite file; one user)",
            "On a shared fieldcraft-server (several machines or agents share one store)",
        ],
        default_mode,
    )?;
    let store = if mode_pick == 1 {
        StoreChoice::Local
    } else {
        let url = loop {
            let url = ui.ask("Server URL", persisted_remote.as_deref())?;
            let url = url.trim().trim_end_matches('/').to_string();
            if url.is_empty() {
                println!("  a URL is required, e.g. http://memory-box.local:7777");
                continue;
            }
            match server_health(&url).await {
                Ok(()) => break url,
                Err(err) => {
                    println!("  could not reach {url}/health: {err}");
                    if ui.confirm("Use it anyway?", false)? {
                        break url;
                    }
                }
            }
        };
        let mut auth_token = None;
        let mut enroll_secret = None;
        let same_server = persisted_remote.as_deref() == Some(url.as_str());
        if same_server
            && fieldcraft_auth_token().is_some()
            && ui.confirm("Keep the existing token for this server?", true)?
        {
            auth_token = fieldcraft_auth_token();
        }
        if auth_token.is_none() {
            println!("  New machines enroll with the server's FIELDCRAFT_ENROLL_SECRET (ask its admin),");
            println!("  or paste a token the admin issued.");
            let secret = ui.secret("Enrollment secret (Enter to skip)")?;
            if secret.is_empty() {
                let token = ui.secret("Auth token (Enter to skip)")?;
                if !token.is_empty() {
                    auth_token = Some(token);
                }
            } else {
                enroll_secret = Some(secret);
            }
            if auth_token.is_none() && enroll_secret.is_none() {
                println!("  Continuing without credentials; enrollment will only work from the server itself.");
            }
        }
        StoreChoice::Remote {
            url,
            enroll_secret,
            auth_token,
        }
    };
    println!();

    // 2. Embedding key.
    let secrets_path = fieldcraft_core::secrets::secrets_file_path();
    let existing_key = fieldcraft_core::secrets::lookup_any(&EMBED_KEYS);
    let remote = matches!(store, StoreChoice::Remote { .. });
    let mut key_written = false;
    println!("Embeddings: Fieldcraft ranks lessons with Google Gemini embeddings. Lesson text and");
    println!("queries are sent to that API. Without a key retrieval is keyword-only and the");
    println!("vector-based maintenance does nothing.");
    if remote {
        println!("In shared-server mode only the server needs the key.");
    }
    let want_key = match &existing_key {
        Some((_, source)) => {
            println!("A gemini key is already available (from {source:?}).");
            ui.confirm("Replace it?", false)?
        }
        None => !remote || ui.confirm("Store a gemini key on this machine anyway?", false)?,
    };
    if want_key {
        loop {
            let key = ui.secret("Gemini API key (Enter to skip)")?;
            if key.is_empty() {
                if existing_key.is_none() && !remote {
                    println!("  Skipped. Add it later to {} as `export GEMINI_API_KEY=...`", secrets_path.display());
                }
                break;
            }
            print!("  checking the key... ");
            io::stdout().flush()?;
            match test_gemini_key(&key).await {
                Ok(dims) => {
                    println!("ok ({dims} dims)");
                    write_secret(&secrets_path, "GEMINI_API_KEY", &key)?;
                    println!("  saved to {} (owner-only)", secrets_path.display());
                    key_written = true;
                    break;
                }
                Err(err) => {
                    println!("failed: {err}");
                    if !ui.confirm("Try another key?", true)? {
                        break;
                    }
                }
            }
        }
    }
    println!();

    // 3. Distiller.
    let available: Vec<&str> = DISTILLERS
        .iter()
        .copied()
        .filter(|d| *d == "none" || on_path(d))
        .collect();
    println!("Background distiller: after a session with friction (interrupts, failures, corrections)");
    println!("a coding-agent CLI can review the transcript and write at most one lesson. It runs");
    println!("non-interactively with approvals bypassed, so transcript text can steer it. `none`");
    println!("keeps everything else working; you can still write lessons yourself.");
    let labels: Vec<String> = available
        .iter()
        .map(|d| match *d {
            "none" => "none (recommended until you trust the setup)".to_string(),
            other => format!("{other} (found on PATH)"),
        })
        .collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let current = fieldcraft_config_value("FIELDCRAFT_DISTILLER").unwrap_or_default();
    let default_distiller = available
        .iter()
        .position(|d| *d == current)
        .map(|i| i + 1)
        .unwrap_or(1);
    let distiller = available[ui.choose("Distiller?", &label_refs, default_distiller)? - 1].to_string();
    println!();

    // 4. Summary and install.
    println!("Plan:");
    match &store {
        StoreChoice::Local => println!("  store:      local service on 127.0.0.1:7777, data in ~/.fieldcraft/"),
        StoreChoice::Remote { url, enroll_secret, auth_token } => {
            let cred = if auth_token.is_some() {
                "existing token"
            } else if enroll_secret.is_some() {
                "enroll with secret"
            } else {
                "no credentials"
            };
            println!("  store:      shared server {url} ({cred})");
        }
    }
    println!(
        "  embeddings: {}",
        if key_written {
            "key saved to secrets.env"
        } else if existing_key.is_some() {
            "existing key kept"
        } else {
            "no key (keyword-only retrieval)"
        }
    );
    println!("  distiller:  {distiller}");
    println!("  agents:     hooks and instructions for Claude Code, Codex{}", if agents.contains(&Agent::Grok) { ", Grok" } else { "" });
    println!("  then:       run install.sh for the service, token and hooks, and `fieldcraft doctor`");
    if !ui.confirm("Proceed?", true)? {
        println!("Nothing changed.");
        return Ok(());
    }
    println!();

    let mut install = ProcessCommand::new("bash");
    install
        .arg(&install_script)
        .arg("--yes")
        .arg("--skip-build")
        .arg("--distiller")
        .arg(&distiller);
    if let StoreChoice::Remote {
        url,
        enroll_secret,
        auth_token,
    } = &store
    {
        install.arg("--server-url").arg(url);
        if let Some(token) = auth_token {
            install.arg("--auth-token").arg(token);
        }
        if let Some(secret) = enroll_secret {
            install.env("FIELDCRAFT_ENROLL_SECRET", secret);
        }
    }
    run_checked(&mut install).context("install.sh failed")?;

    // 5. Verify.
    fieldcraft_core::secrets::reload();
    println!();
    println!("Checking the install:");
    let report = run_doctor(false).await;
    report.print();
    println!();
    println!("Next: start a new Claude Code / Codex / Grok session; the SessionStart hook will remind");
    println!("the agent to run `fieldcraft query`. Try it yourself: fieldcraft query \"<what you are about to do>\"");
    println!("Re-check any time with `fieldcraft doctor`.");
    if report.failed() {
        anyhow::bail!("setup finished but the doctor found failing checks (see above)");
    }
    Ok(())
}

async fn server_health(url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()?;
    let resp = client.get(format!("{url}/health")).send().await?;
    if resp.status().is_success() {
        Ok(())
    } else {
        anyhow::bail!("HTTP {}", resp.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_config_detects_commands_under_any_event() {
        let json = serde_json::json!({
            "hooks": {
                "SessionStart": [{"hooks": [{"type": "command", "command": "/x/.fieldcraft/hooks/session-start-query.sh"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "/x/.fieldcraft/hooks/stop-feedback.sh"}]}]
            }
        });
        assert!(hooks_config_has(&json, "session-start-query.sh"));
        assert!(hooks_config_has(&json, "stop-feedback.sh"));
        assert!(!hooks_config_has(&json, "other.sh"));
        assert!(!hooks_config_has(&serde_json::json!({}), "stop-feedback.sh"));
    }

    #[test]
    fn detects_agents_by_cli_or_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
        let found = detect_agents_with(dir.path(), |cli| cli == "grok");
        assert_eq!(found, vec![Agent::Codex, Agent::Grok]);
        assert!(detect_agents_with(dir.path(), |_| false) == vec![Agent::Codex]);
    }

    #[test]
    fn write_secret_creates_replaces_and_preserves_other_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        write_secret(&path, "GEMINI_API_KEY", "first").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("export GEMINI_API_KEY=first\n"));
        assert!(text.starts_with("# Fieldcraft secrets"));

        std::fs::write(&path, "# keep me\nexport OTHER=1\nGEMINI_API_KEY=first\n").unwrap();
        write_secret(&path, "GEMINI_API_KEY", "sec ond'x").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("GEMINI_API_KEY=").count(), 1);
        assert!(text.contains("# keep me\n"));
        assert!(text.contains("export OTHER=1\n"));
        assert!(text.contains("export GEMINI_API_KEY='sec ond'\\''x'\n"));
        assert_eq!(
            fieldcraft_core::secrets::parse_secrets(&text)["GEMINI_API_KEY"],
            "sec ond'x"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn report_fails_only_on_fail_level() {
        let mut report = DoctorReport::default();
        report.push("a", Level::Warn, "w");
        assert!(!report.failed());
        report.push("b", Level::Fail, "f");
        assert!(report.failed());
    }
}
