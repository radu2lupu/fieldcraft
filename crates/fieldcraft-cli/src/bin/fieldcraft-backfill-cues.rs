//! `fieldcraft-backfill-cues` — one-shot backfill of `cue_bundle` and `cue_index`
//! across the entire fieldcraft knowledge store.
//!
//! Phase 1 of cue-bundle retrieval — see `docs/plans/cue-bundle-retrieval.md`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use fieldcraft_storage::{backfill_all, fieldcraft_db_path, open_catalog};

#[derive(Parser, Debug)]
#[command(
    name = "fieldcraft-backfill-cues",
    about = "Populate cue_bundle + cue_index for existing knowledge records"
)]
struct Cli {
    /// Re-extract bundles for records that already have one.
    #[arg(long)]
    force: bool,

    /// Path to a specific catalog DB.
    /// Defaults to `~/.fieldcraft/memory.sqlite3` (honors `FIELDCRAFT_HOME`).
    #[arg(long)]
    db: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = cli.db.unwrap_or_else(fieldcraft_db_path);
    println!("Opening catalog at {}", path.display());

    let mut conn = open_catalog(&path).context("open_catalog")?;
    let stats = backfill_all(&mut conn, cli.force).context("backfill_all")?;
    println!(
        "Backfill complete — considered: {}, updated: {}, skipped: {}",
        stats.considered, stats.updated, stats.skipped
    );
    Ok(())
}
