//! Deterministic cue extraction — fills 5 of 7 [`CueBundle`] dimensions without an LLM.
//!
//! The two LLM-classified dimensions (`phase`, `user_state`) are left `None` here;
//! they're populated by a future `cue_classify` module via Haiku.
//!
//! Used at three points:
//! 1. **Write time** — over the record's payload + retrieval_text to populate
//!    the stored bundle.
//! 2. **Retrieval time** — over the current query + active session context to
//!    produce the query bundle for conjunctive scoring.
//! 3. **Backfill** — over existing records that pre-date cue-bundle support.
//!
//! Grounding: Tulving & Thomson (1973) encoding specificity — recall improves
//! when retrieval cues overlap with cues present at encoding. Capturing rich
//! situational features at write time and the same features at retrieval time
//! makes that overlap computable.

use std::collections::{HashMap, HashSet};

use crate::{Cue, CueBundle, CueDimension, CueValue, SessionArc};

/// Tokens carrying no retrieval signal. Mirrors the list used by
/// `fieldcraft-retrieval::retrieval::STOP_WORDS` and extends it with a few
/// extra interrogatives common in retrieval queries.
const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "are", "was", "have", "has", "had", "can",
    "will", "would", "could", "should", "not", "but", "its", "all", "any", "our", "your", "their",
    "also", "you", "they", "them", "what", "when", "where", "which", "how", "why", "who",
];

/// Known tool names — used for exact-token matching (case-insensitive).
/// Order is irrelevant; deduplication happens in [`extract_tools_involved`].
const KNOWN_TOOLS: &[&str] = &[
    "cargo",
    "git",
    "npm",
    "yarn",
    "pnpm",
    "make",
    "docker",
    "kubectl",
    "helm",
    "terraform",
    "rustc",
    "rustup",
    "pytest",
    "jest",
    "vitest",
    "ruff",
    "mypy",
    "eslint",
    "prettier",
    "bash",
    "zsh",
    "ssh",
    "curl",
    "wget",
    "rg",
    "fd",
    "sqlite3",
];

/// Top-level deterministic extractor. Fills five dimensions; leaves `phase` and
/// `user_state` as `None` (they require LLM classification).
///
/// * `text` — primary text to mine for tools and distinctive terms
///   (e.g. record payload + title; or query text + recent reasoning).
/// * `file_paths` — explicit file paths present at encoding / retrieval.
/// * `turn_count` — None when unknown (e.g. backfilling a record without
///   session context); otherwise bucketed via [`SessionArc::from_turn_count`].
pub fn extract_deterministic(
    text: &str,
    file_paths: &[String],
    turn_count: Option<u32>,
) -> CueBundle {
    let domain = extract_domain(file_paths);
    let tools_involved = extract_tools_involved(text);
    let active_files = file_paths.to_vec();
    let distinctive_terms = extract_distinctive_terms(text, 8);
    let mut bundle = CueBundle {
        phase: None,
        user_state: None,
        domain: domain.clone(),
        tools_involved: tools_involved.clone(),
        active_files: active_files.clone(),
        distinctive_terms: distinctive_terms.clone(),
        session_arc: turn_count.map(SessionArc::from_turn_count),
        cues: Vec::new(),
    };

    if let Some(domain) = domain {
        add_simple_cue(
            &mut bundle,
            CueDimension::Domain,
            &domain,
            "domain",
            "deterministic",
        );
    }
    for tool in tools_involved {
        add_simple_cue(
            &mut bundle,
            CueDimension::Technical,
            &tool,
            "tool",
            "deterministic",
        );
    }
    for file in active_files {
        add_simple_cue(
            &mut bundle,
            CueDimension::File,
            &file,
            "path",
            "deterministic",
        );
    }
    for term in distinctive_terms {
        add_simple_cue(
            &mut bundle,
            CueDimension::Symbol,
            &term,
            "distinctive_term",
            "deterministic",
        );
    }

    bundle
}

fn add_simple_cue(
    bundle: &mut CueBundle,
    dimension: CueDimension,
    raw: &str,
    kind: &str,
    source: &str,
) {
    let canonical = normalize_cue_value(raw);
    if canonical.len() < 2 {
        return;
    }
    if bundle
        .cues
        .iter()
        .any(|cue| cue.dimension == dimension && cue.value.canonical == canonical)
    {
        return;
    }
    bundle.cues.push(Cue::new(
        dimension,
        CueValue::new(raw, canonical).with_kind(kind),
        0.8,
        source,
    ));
}

pub fn normalize_cue_value(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '-' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Attach a project/app/repo identity cue. The raw value is preserved, while
/// the canonical and alias forms are normalized for fuzzy matching in
/// `cue_index`.
pub fn add_identity_cue(bundle: &mut CueBundle, raw: &str, source: &str) {
    let canonical = normalize_identity(raw);
    if canonical.len() < 3 {
        return;
    }
    if bundle
        .cues
        .iter()
        .any(|cue| cue.dimension == CueDimension::Identity && cue.value.canonical == canonical)
    {
        return;
    }
    let aliases = identity_aliases(raw, &canonical);
    let value = CueValue::new(raw, canonical)
        .with_kind("project_identity")
        .with_aliases(aliases);
    bundle
        .cues
        .push(Cue::new(CueDimension::Identity, value, 0.9, source));
}

pub fn normalize_identity(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn identity_aliases(raw: &str, canonical: &str) -> Vec<String> {
    let mut aliases = std::collections::BTreeSet::new();
    let lower = raw.to_lowercase();
    for alias in [
        lower.replace('_', "-"),
        lower.replace('-', "_"),
        canonical.to_string(),
    ] {
        if alias.len() >= 3 && alias != canonical {
            aliases.insert(alias);
        }
    }
    if let Some(stem) = canonical.strip_suffix("ios")
        && !stem.is_empty()
    {
        aliases.insert(format!("{stem}_ios"));
        aliases.insert(format!("{stem}-ios"));
    }
    aliases.into_iter().collect()
}

/// Pick the most frequent top-level path component across `file_paths`.
/// Rewrites `crates/<name>/...` to `<name>` (with `-` → `_`) so paths under
/// the workspace come out as `fieldcraft_retrieval`, `fieldcraft_core`, etc.
///
/// Returns `None` when no paths are supplied or none contain a parseable root.
pub fn extract_domain(file_paths: &[String]) -> Option<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for path in file_paths {
        if let Some(comp) = path_to_domain(path) {
            *counts.entry(comp).or_insert(0) += 1;
        }
    }
    // Stable tiebreak: highest count, then alphabetically first.
    let mut entries: Vec<(String, usize)> = counts.into_iter().collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries.into_iter().next().map(|(d, _)| d)
}

fn path_to_domain(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    if parts[0] == "crates" && parts.len() >= 2 {
        return Some(parts[1].replace('-', "_"));
    }
    Some(parts[0].to_string())
}

/// Detect known tool names mentioned in `text`. Tokenization is alphanumeric;
/// matching is case-insensitive against [`KNOWN_TOOLS`]. Result is sorted for
/// determinism.
pub fn extract_tools_involved(text: &str) -> Vec<String> {
    let tokens: HashSet<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect();
    let mut tools: Vec<String> = KNOWN_TOOLS
        .iter()
        .filter(|t| tokens.contains(**t))
        .map(|s| (*s).to_string())
        .collect();
    tools.sort();
    tools
}

/// Extract up to `top_k` distinctive terms in first-encountered order.
///
/// "Distinctive" is approximated heuristically: at least 3 characters, not a
/// stop word, and matches at least one of:
/// - CamelCase / PascalCase (has both upper and lower case letters)
/// - SCREAMING_SNAKE_CASE (all upper plus underscore)
/// - All-upper acronym (≥3 chars, all upper)
/// - Alphanumeric with digit (e.g. `arXiv2406`, `H100`, `BGE` covered above)
///
/// This is a v1 heuristic — proper TF-IDF against the corpus is a future
/// upgrade. The current approximation captures most code-like and
/// technical-paper-like tokens in practice.
pub fn extract_distinctive_terms(text: &str, top_k: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for tok in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if !is_distinctive(tok) {
            continue;
        }
        let s = tok.to_string();
        if seen.insert(s.clone()) {
            out.push(s);
            if out.len() >= top_k {
                break;
            }
        }
    }
    out
}

fn is_distinctive(token: &str) -> bool {
    if token.len() < 3 {
        return false;
    }
    if STOP_WORDS.contains(&token.to_lowercase().as_str()) {
        return false;
    }
    let has_upper = token.chars().any(|c| c.is_uppercase());
    let has_lower = token.chars().any(|c| c.is_lowercase());
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    let has_underscore = token.contains('_');
    let all_upper = !has_lower && has_upper;
    let camel = has_upper && has_lower;
    let screaming = all_upper && has_underscore;
    let acronym = all_upper && token.len() >= 3;
    let alphanum_with_digit = has_digit && (has_upper || has_lower);
    camel || screaming || acronym || alphanum_with_digit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_picks_first_path_component() {
        let paths = vec!["docs/plans/x.md".to_string()];
        assert_eq!(extract_domain(&paths), Some("docs".to_string()));
    }

    #[test]
    fn domain_rewrites_crates_to_crate_name() {
        let paths = vec!["crates/fieldcraft-retrieval/src/retrieval.rs".to_string()];
        assert_eq!(
            extract_domain(&paths),
            Some("fieldcraft_retrieval".to_string())
        );
    }

    #[test]
    fn domain_picks_most_frequent_when_paths_disagree() {
        let paths = vec![
            "crates/fieldcraft-core/src/lib.rs".to_string(),
            "crates/fieldcraft-core/src/cue_extract.rs".to_string(),
            "docs/plan.md".to_string(),
        ];
        assert_eq!(extract_domain(&paths), Some("fieldcraft_core".to_string()));
    }

    #[test]
    fn domain_returns_none_on_empty_input() {
        assert_eq!(extract_domain(&[]), None);
    }

    #[test]
    fn domain_handles_leading_slash() {
        let paths = vec!["/usr/local/bin/fieldcraft".to_string()];
        assert_eq!(extract_domain(&paths), Some("usr".to_string()));
    }

    #[test]
    fn tools_involved_detects_known_tools() {
        let text = "Ran cargo build, then git commit, then docker run";
        let tools = extract_tools_involved(text);
        assert!(tools.contains(&"cargo".to_string()));
        assert!(tools.contains(&"git".to_string()));
        assert!(tools.contains(&"docker".to_string()));
    }

    #[test]
    fn tools_involved_case_insensitive() {
        let tools = extract_tools_involved("Cargo Build with GIT");
        assert!(tools.contains(&"cargo".to_string()));
        assert!(tools.contains(&"git".to_string()));
    }

    #[test]
    fn tools_involved_ignores_unknown_words() {
        assert!(extract_tools_involved("nothing interesting here").is_empty());
    }

    #[test]
    fn tools_involved_is_sorted_deterministically() {
        let tools = extract_tools_involved("git docker cargo");
        assert_eq!(tools, vec!["cargo", "docker", "git"]);
    }

    #[test]
    fn distinctive_terms_captures_camelcase_and_screaming() {
        let text = "P0_WEIGHTS controls the KnowledgeRecord scoring via RRF";
        let terms = extract_distinctive_terms(text, 8);
        assert!(terms.iter().any(|t| t == "P0_WEIGHTS"));
        assert!(terms.iter().any(|t| t == "KnowledgeRecord"));
        assert!(terms.iter().any(|t| t == "RRF"));
    }

    #[test]
    fn distinctive_terms_caps_at_top_k() {
        let text = "AlphaOne BetaTwo GammaThree DeltaFour EpsilonFive ZetaSix";
        let terms = extract_distinctive_terms(text, 3);
        assert_eq!(terms.len(), 3);
    }

    #[test]
    fn distinctive_terms_preserves_first_seen_order() {
        let text = "ZetaSix AlphaOne BetaTwo";
        let terms = extract_distinctive_terms(text, 3);
        assert_eq!(terms, vec!["ZetaSix", "AlphaOne", "BetaTwo"]);
    }

    #[test]
    fn distinctive_terms_filters_stop_words_and_short_tokens() {
        let text = "the and for with abc xy z";
        assert!(extract_distinctive_terms(text, 8).is_empty());
    }

    #[test]
    fn distinctive_terms_deduplicates_within_text() {
        let text = "KnowledgeRecord and KnowledgeRecord again and KnowledgeRecord too";
        let terms = extract_distinctive_terms(text, 8);
        assert_eq!(terms.iter().filter(|t| *t == "KnowledgeRecord").count(), 1);
    }

    #[test]
    fn extract_deterministic_fills_5_of_7_dimensions() {
        let text = "Fixed P0_WEIGHTS bug in retrieval.rs using cargo test";
        let files = vec!["crates/fieldcraft-retrieval/src/retrieval.rs".to_string()];
        let bundle = extract_deterministic(text, &files, Some(7));
        assert!(bundle.phase.is_none(), "phase is LLM-only");
        assert!(bundle.user_state.is_none(), "user_state is LLM-only");
        assert_eq!(bundle.domain, Some("fieldcraft_retrieval".to_string()));
        assert!(bundle.tools_involved.contains(&"cargo".to_string()));
        assert_eq!(bundle.active_files.len(), 1);
        assert!(bundle.distinctive_terms.iter().any(|t| t == "P0_WEIGHTS"));
        assert_eq!(bundle.session_arc, Some(SessionArc::Mid));
    }

    #[test]
    fn extract_deterministic_with_no_turn_count_yields_none_session_arc() {
        let bundle = extract_deterministic("x", &[], None);
        assert!(bundle.session_arc.is_none());
    }

    #[test]
    fn identity_cue_preserves_separator_aliases() {
        let mut bundle = CueBundle::default();
        add_identity_cue(&mut bundle, "ohapotato_ios", "test");

        let cue = bundle.cues.first().expect("identity cue");
        assert_eq!(cue.dimension, CueDimension::Identity);
        assert_eq!(cue.value.canonical, "ohapotatoios");
        assert!(cue.value.aliases.contains(&"ohapotato_ios".to_string()));
        assert!(cue.value.aliases.contains(&"ohapotato-ios".to_string()));
    }
}
