//! Repository maintenance. Run with `cargo xtask <command>`.

mod bench;
mod policy;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enforce the asset policy: the repository holds only small text files
    /// we wrote. Checks tracked and untracked-but-not-ignored files.
    Policy {
        /// Check only what is staged for commit (used by the pre-commit hook).
        #[arg(long)]
        staged: bool,
    },
    /// Run the Windows hotspot benchmark through the WSL queue and GPU reservation.
    BenchHot {
        #[arg(value_enum)]
        mode: bench::Mode,
        #[arg(long)]
        runs: Option<usize>,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Prebuilt checkout or target directory for each arm of an A/B run.
        #[arg(long, requires = "b")]
        a: Option<PathBuf>,
        #[arg(long, requires = "a")]
        b: Option<PathBuf>,
    },
    /// Compare two hotspot results.json files.
    BenchCompare { before: PathBuf, after: PathBuf },
    /// Streaming numeric backend used by the Windows benchmark driver.
    #[command(hide = true)]
    BenchStats {
        #[arg(long)]
        compare: bool,
        #[arg(long, conflicts_with = "compare")]
        paired: bool,
    },
    /// Point git at .githooks/ so every commit runs the policy check.
    InstallHooks,
}

fn main() -> Result<()> {
    let root = workspace_root();
    match Cli::parse().command {
        Cmd::Policy { staged } => policy::run(&root, staged),
        Cmd::BenchHot {
            mode,
            runs,
            out,
            a,
            b,
        } => bench::hot(&root, mode, runs, out, a.zip(b)),
        Cmd::BenchCompare { before, after } => bench::compare(&root, &before, &after),
        Cmd::BenchStats { compare, paired } => bench::statistics(compare, paired),
        Cmd::InstallHooks => {
            let status = Command::new("git")
                .args(["config", "core.hooksPath", ".githooks"])
                .current_dir(&root)
                .status()
                .context("running git")?;
            anyhow::ensure!(
                status.success(),
                "git config failed (is this a git repository?)"
            );
            println!("hooks installed: commits now run `cargo xtask policy --staged`");
            Ok(())
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}
