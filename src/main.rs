mod app;
mod codex_rpc;
mod ui;
mod usage;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "comon", version, about = "Codex usage + limits TUI")]
struct Args {
    /// Override path to the Codex CLI binary (default: `codex` in PATH).
    #[arg(long)]
    codex_bin: Option<String>,

    /// Override CODEX_HOME (default: $CODEX_HOME or ~/.codex).
    #[arg(long)]
    codex_home: Option<PathBuf>,

    /// Working directory to launch `codex app-server` in (default: current directory).
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Number of days to scan for local usage (clamped to 1..=90).
    #[arg(long, default_value_t = 30)]
    usage_days: u32,

    /// Periodic refresh interval for usage stats (seconds).
    #[arg(long, default_value_t = 300)]
    refresh_usage_secs: u64,

    /// Periodic refresh interval for limits/credits (seconds).
    #[arg(long, default_value_t = 60)]
    refresh_limits_secs: u64,

    /// Max size (MiB) of a single session `.jsonl` file to scan.
    #[arg(long, default_value_t = 256)]
    max_session_file_mib: u64,

    /// Max total size (MiB) to scan across session files.
    #[arg(long, default_value_t = 256)]
    max_session_total_mib: u64,

    /// Max number of session files to scan per refresh.
    #[arg(long, default_value_t = 10_000)]
    max_session_files: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let cwd = args
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let codex_home = usage::resolve_codex_home(args.codex_home.clone())
        .context("Unable to resolve CODEX_HOME")?;

    let max_session_total_bytes = args
        .max_session_total_mib
        .max(1)
        .saturating_mul(1024 * 1024);
    let max_session_file_bytes = args
        .max_session_file_mib
        .max(1)
        .saturating_mul(1024 * 1024)
        .min(max_session_total_bytes);
    let usage_scan_limits = usage::ScanLimits {
        max_session_file_bytes,
        max_session_total_bytes,
        max_session_files_scanned: args.max_session_files.max(1),
    };

    let config = app::Config {
        codex_bin: args.codex_bin.clone(),
        codex_home,
        cwd,
        usage_days: args.usage_days.clamp(1, 90),
        refresh_usage_secs: args.refresh_usage_secs.max(30),
        refresh_limits_secs: args.refresh_limits_secs.max(10),
        usage_scan_limits,
    };

    app::run(config).await
}
