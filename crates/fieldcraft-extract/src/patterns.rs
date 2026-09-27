use regex::Regex;
use std::sync::OnceLock;

// ── Test runner patterns ──────────────────────────────────────────────────────

static TEST_RUNNER_RE: OnceLock<Regex> = OnceLock::new();
pub fn test_runner_re() -> &'static Regex {
    TEST_RUNNER_RE.get_or_init(|| {
        Regex::new(
            r"(?mi)(^test result:|running \d+ test|\bPASSED\b|\bFAILED\b|\bFAIL\b|\bPASS\b|^ok$|^FAILED$|\.\.\.ok|\.\.\.FAILED|pytest|npm test|jest|rspec|go test|cargo test|tests passed|tests failed|\d+ passed;\s*\d+ failed|\d+ assertions?)"
        ).unwrap()
    })
}

static TEST_FAILURE_LINE_RE: OnceLock<Regex> = OnceLock::new();
pub fn test_failure_line_re() -> &'static Regex {
    TEST_FAILURE_LINE_RE.get_or_init(|| {
        Regex::new(
            r"(?mi)(FAILED|FAIL|error|panicked|assertion.*failed|expected.*found|stack backtrace|thread.*panicked|Error:|× |✗ |not ok \d)"
        ).unwrap()
    })
}

// ── Compiler output patterns ──────────────────────────────────────────────────

static COMPILER_OUTPUT_RE: OnceLock<Regex> = OnceLock::new();
pub fn compiler_output_re() -> &'static Regex {
    COMPILER_OUTPUT_RE.get_or_init(|| {
        Regex::new(
            r"(?m)(^error(\[E\d+\])?:|^warning(\[.*?\])?:|^\s+-->\s+\S+:\d+:\d+|ld: error:|undefined reference to|type error TS\d+|error TS\d+|\.ts\(\d+,\d+\): error)"
        ).unwrap()
    })
}

static COMPILER_ERROR_LINE_RE: OnceLock<Regex> = OnceLock::new();
pub fn compiler_error_line_re() -> &'static Regex {
    COMPILER_ERROR_LINE_RE
        .get_or_init(|| Regex::new(r"(?m)(^error|^warning|^\s+-->\s|^\s+\|)").unwrap())
}

// ── Build progress patterns ───────────────────────────────────────────────────

static BUILD_PROGRESS_RE: OnceLock<Regex> = OnceLock::new();
pub fn build_progress_re() -> &'static Regex {
    BUILD_PROGRESS_RE.get_or_init(|| {
        Regex::new(
            r"(?m)(^\s+Compiling |^\s+Downloading |^\s+Resolving |^npm warn |^npm notice |added \d+ packages|packages are looking for funding|Blocking waiting for file lock)"
        ).unwrap()
    })
}

// ── ANSI escape codes ─────────────────────────────────────────────────────────

static ANSI_RE: OnceLock<Regex> = OnceLock::new();
pub fn ansi_re() -> &'static Regex {
    ANSI_RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*[mGKHF]").unwrap())
}

// ── Agent narration patterns ──────────────────────────────────────────────────

static NARRATION_RE: OnceLock<Regex> = OnceLock::new();
pub fn narration_re() -> &'static Regex {
    NARRATION_RE.get_or_init(|| {
        Regex::new(
            r"(?mi)^(Let me (read|check|look|examine|explore|see|find|search|verify|try|run|take a look)|I('ll| will) (now |)(read|check|look|examine|explore|search|run|try|take|start|begin|use|call|need)|I need to |I'm going to |Now (let me|I'll)|First,? (let me|I'll)|Next,? (let me|I'll)|Let's (start|begin|look|check|try)|Looking at |Checking |Reading |Running |Let me now )"
        ).unwrap()
    })
}

// ── Decision-signal keywords (preserve these) ────────────────────────────────

static DECISION_RE: OnceLock<Regex> = OnceLock::new();
pub fn decision_re() -> &'static Regex {
    DECISION_RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(because|instead|root cause|chose|discovered|the fix|the issue|decided|therefore|this means|the reason|that's why|which means)\b"
        ).unwrap()
    })
}

// ── Output type detection ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum OutputType {
    TestRunner,
    CompilerOutput,
    BuildProgress,
    FileContent,
    Generic,
}

/// Detect the type of tool output content by pattern matching on the content.
pub fn detect_output_type(content: &str) -> OutputType {
    let stripped = ansi_re().replace_all(content, "");
    let s = stripped.as_ref();

    // Order matters: build progress first (pure "Compiling" lines with no errors),
    // then compiler errors, then test runner output.
    if build_progress_re().is_match(s) {
        return OutputType::BuildProgress;
    }
    if compiler_output_re().is_match(s) {
        return OutputType::CompilerOutput;
    }
    if test_runner_re().is_match(s) {
        return OutputType::TestRunner;
    }
    OutputType::Generic
}

/// Strip ANSI escape sequences from a string.
pub fn strip_ansi(s: &str) -> String {
    ansi_re().replace_all(s, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_test_runner() {
        assert_eq!(
            detect_output_type(
                "running 5 tests\ntest foo ... ok\ntest result: ok. 5 passed; 0 failed"
            ),
            OutputType::TestRunner
        );
    }

    #[test]
    fn test_detect_compiler_output() {
        assert_eq!(
            detect_output_type(
                "error[E0308]: mismatched types\n  --> src/main.rs:10:5\n   |\n10 |     \"hello\"\n   |     ^^^^^^^ expected `i32`, found `&str`"
            ),
            OutputType::CompilerOutput
        );
    }

    #[test]
    fn test_detect_build_progress() {
        assert_eq!(
            detect_output_type("   Compiling regex v1.10.0\n   Compiling serde v1.0.0"),
            OutputType::BuildProgress
        );
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("\x1b[31mred text\x1b[0m"), "red text");
    }

    #[test]
    fn test_narration_match() {
        assert!(narration_re().is_match("Let me read the file to understand the structure."));
        assert!(narration_re().is_match("I'll now check the configuration."));
    }

    #[test]
    fn test_decision_match() {
        assert!(decision_re().is_match("We chose this approach because it avoids the lock."));
        assert!(decision_re().is_match("The root cause was the missing index."));
    }
}
