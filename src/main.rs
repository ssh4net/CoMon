mod app;
mod codex_rpc;
mod ui;
mod usage;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

fn validate_dir(path: &std::path::Path, label: &str) -> Result<PathBuf> {
    let meta = std::fs::metadata(path).with_context(|| format!("{label} does not exist"))?;
    if !meta.is_dir() {
        anyhow::bail!("{label} must be a directory");
    }
    // Best-effort canonicalization to normalize `..` and symlinks.
    Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn detect_git_root(start: &std::path::Path) -> Option<PathBuf> {
    // Heuristic: treat a directory as a "project" if it's inside a git work tree.
    // Walk upwards looking for `.git` (directory or file for worktrees/submodules).
    let mut cur = start;
    for _ in 0..64 {
        let git = cur.join(".git");
        if std::fs::metadata(&git).is_ok() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
    None
}

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

    /// Filter usage stats to a specific project/workspace path.
    ///
    /// If `--cwd` is not provided, this also becomes the default working directory for
    /// launching `codex app-server`.
    #[arg(long, alias = "workspace")]
    project: Option<PathBuf>,

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

    let launch_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Determine a candidate directory to infer the "project" from:
    // - Explicit `--project` wins
    // - Else infer from `--cwd` (user intent: run in that repo)
    // - Else infer from current directory
    let cwd_override = args
        .cwd
        .clone()
        .map(|p| validate_dir(&p, "--cwd"))
        .transpose()?;
    let project_override = args
        .project
        .clone()
        .map(|p| validate_dir(&p, "--project"))
        .transpose()?;

    let project_candidate = project_override
        .as_deref()
        .or(cwd_override.as_deref())
        .unwrap_or(launch_dir.as_path());

    // Only treat something as a "project" if it is inside a git work tree.
    let project =
        detect_git_root(project_candidate).map(|p| std::fs::canonicalize(&p).unwrap_or(p));

    // `cwd` controls where `codex app-server` is launched.
    let cwd = cwd_override
        .or_else(|| project.clone())
        .unwrap_or_else(|| launch_dir.clone());
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
        workspace_path: project,
        usage_days: args.usage_days.clamp(1, 90),
        refresh_usage_secs: args.refresh_usage_secs.max(30),
        refresh_limits_secs: args.refresh_limits_secs.max(10),
        usage_scan_limits,
    };

    app::run(config).await
}
