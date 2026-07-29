pub(crate) mod catalog;
pub(crate) mod scan;
pub(crate) mod tui;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) sessions_dir: PathBuf,
}

pub(crate) fn print_sessions_dir(
    codex_home: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
) -> Result<()> {
    let config = build_config(codex_home, sessions_dir)?;
    println!("{}", config.sessions_dir.display());
    Ok(())
}

pub(crate) fn build_config(
    codex_home: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
) -> Result<Config> {
    let sessions_dir = resolve_sessions_dir(codex_home, sessions_dir)?;
    Ok(Config { sessions_dir })
}

pub(crate) fn build_browser(config: &Config) -> Result<tui::BrowserState> {
    let catalog = scan::build_catalog(&config.sessions_dir)?;
    Ok(tui::BrowserState::new(catalog))
}

fn resolve_sessions_dir(
    codex_home: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = sessions_dir {
        return validate_dir(&path, "--sessions-dir");
    }

    let codex_home = resolve_codex_home(codex_home)
        .context("Unable to resolve CODEX_HOME (default: ~/.codex)")?;
    Ok(codex_home.join("sessions"))
}

fn resolve_codex_home(override_home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = override_home {
        return Some(path);
    }

    if let Ok(value) = std::env::var("CODEX_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    if let Ok(value) = std::env::var("HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join(".codex"));
        }
    }

    if let Ok(value) = std::env::var("USERPROFILE") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join(".codex"));
        }
    }

    None
}

fn validate_dir(path: &Path, label: &str) -> Result<PathBuf> {
    let meta = std::fs::metadata(path).with_context(|| format!("{label} does not exist"))?;
    if !meta.is_dir() {
        anyhow::bail!("{label} must be a directory");
    }
    Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}
