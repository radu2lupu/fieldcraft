//! Secrets file: `~/.fieldcraft/secrets.env`.
//!
//! API keys used to live only in the process environment, which launchd
//! plists and `install.sh` rewrote; a reinstall silently dropped the
//! embedding key and retrieval degraded to BM25 for weeks. This module gives
//! every Fieldcraft binary one fallback location that installers never touch:
//! a `KEY=VALUE` file (optionally `export KEY=VALUE`, `#` comments, quoted
//! values) that is read once per process.
//!
//! Precedence: a non-empty process environment variable always wins; the
//! file only fills in what the environment lacks. Keep a secret in one place.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// File name inside the data directory.
pub const SECRETS_FILE_NAME: &str = "secrets.env";

/// Where a looked-up secret came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretSource {
    Env,
    File,
}

static PATH_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();
static CACHE: Mutex<Option<BTreeMap<String, String>>> = Mutex::new(None);

/// Forget the cached file contents so the next lookup re-reads the file
/// (used after `fieldcraft setup` writes a key in the same process).
pub fn reload() {
    if let Ok(mut cache) = CACHE.lock() {
        *cache = None;
    }
}

/// Point lookups at a specific file (the server does this from `--data-dir`).
/// Must be called before the first lookup; later calls are ignored.
pub fn set_file_path(path: PathBuf) {
    let _ = PATH_OVERRIDE.set(path);
}

/// Resolution order: `set_file_path`, `FIELDCRAFT_SECRETS_FILE`,
/// `$FIELDCRAFT_DATA_DIR/secrets.env`, `$HOME/.fieldcraft/secrets.env`.
pub fn secrets_file_path() -> PathBuf {
    if let Some(path) = PATH_OVERRIDE.get() {
        return path.clone();
    }
    if let Some(path) = env_non_empty("FIELDCRAFT_SECRETS_FILE") {
        return PathBuf::from(path);
    }
    if let Some(dir) = env_non_empty("FIELDCRAFT_DATA_DIR") {
        return PathBuf::from(dir).join(SECRETS_FILE_NAME);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".fieldcraft")
        .join(SECRETS_FILE_NAME)
}

/// Environment first, then the secrets file. Empty values count as unset.
pub fn lookup(name: &str) -> Option<String> {
    lookup_with_source(name).map(|(value, _)| value)
}

pub fn lookup_with_source(name: &str) -> Option<(String, SecretSource)> {
    if let Some(value) = env_non_empty(name) {
        return Some((value, SecretSource::Env));
    }
    file_value(name).map(|value| (value, SecretSource::File))
}

/// First name in `names` that resolves, in order.
pub fn lookup_any(names: &[&str]) -> Option<(String, SecretSource)> {
    names.iter().find_map(|name| lookup_with_source(name))
}

fn file_value(name: &str) -> Option<String> {
    let mut cache = CACHE.lock().ok()?;
    let values = cache.get_or_insert_with(|| match std::fs::read_to_string(secrets_file_path()) {
        Ok(text) => parse_secrets(&text),
        Err(_) => BTreeMap::new(),
    });
    values.get(name).cloned()
}

/// Parse `KEY=VALUE` lines. Accepts a leading `export `, `#` comments, blank
/// lines, and single- or double-quoted values. Later lines win.
pub fn parse_secrets(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let value = unquote(value.trim());
        if value.is_empty() {
            continue;
        }
        out.insert(key.to_string(), value);
    }
    out
}

/// Strip one pair of matching quotes. Single-quoted values also undo the
/// POSIX `'\''` escape that `install.sh` and `fieldcraft setup` write, so
/// bash `source` and this parser see the same value.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        let inner = &value[1..value.len() - 1];
        if first == b'\'' && last == b'\'' {
            return inner.replace("'\\''", "'");
        }
        if first == b'"' && last == b'"' {
            return inner.to_string();
        }
    }
    value.to_string()
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_export_quotes_comments_and_precedence() {
        let text = r#"
# comment
export GEMINI_API_KEY=first
GEMINI_API_KEY="second"
OTHER='with spaces'
EMPTY=
BAD LINE
=novalue
  export   SPACED = value
"#;
        let map = parse_secrets(text);
        assert_eq!(
            map.get("GEMINI_API_KEY").map(String::as_str),
            Some("second")
        );
        assert_eq!(map.get("OTHER").map(String::as_str), Some("with spaces"));
        assert_eq!(map.get("SPACED").map(String::as_str), Some("value"));
        assert!(!map.contains_key("EMPTY"));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn unquote_only_strips_matching_pairs() {
        assert_eq!(unquote("\"a\""), "a");
        assert_eq!(unquote("'a'"), "a");
        assert_eq!(unquote("\"a'"), "\"a'");
        assert_eq!(unquote("a"), "a");
        // POSIX single-quote escape, as written by install.sh's shell_quote.
        assert_eq!(unquote("'sec ond'\\''x'"), "sec ond'x");
    }
}
