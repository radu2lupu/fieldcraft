pub mod anthropic;
pub mod chunker;
pub mod citation_match;
pub mod compress;
pub mod extract;
pub mod friction;
pub mod openai_batch;
pub mod patterns;
pub mod stage_a;
pub mod stage_b;
pub mod stage_c;
pub mod transcript;
pub mod util;

pub use compress::{CompressedTranscript, CompressionRule, RuleApplication, TranscriptCompressor};
pub use friction::{
    DEFAULT_MIN_FRICTION, DistillDecision, FrictionCounts, FrictionReport, distill_decision,
    env_force_enabled, format_gate_line, score_transcript, score_transcript_path,
};
pub use transcript::{ContentBlock, TranscriptMessage};
