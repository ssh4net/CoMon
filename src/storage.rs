use anyhow::{Context, Result};
use std::path::Path;

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    ensure_directory_not_symlink(path, "directory")?;
    std::fs::create_dir_all(path)
        .with_context(|| format!("Unable to create directory {}", path.display()))?;
    ensure_directory_not_symlink(path, "directory")?;
    set_permissions(path, 0o700)
        .with_context(|| format!("Unable to set permissions on {}", path.display()))?;
    Ok(())
}

pub fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    ensure_regular_file_or_missing(path, "file")?;
    std::fs::write(path, bytes).with_context(|| format!("Unable to write {}", path.display()))?;
    set_permissions(path, 0o600)
        .with_context(|| format!("Unable to set permissions on {}", path.display()))?;
    Ok(())
}

pub fn enforce_private_file_if_exists(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                anyhow::bail!(
                    "Refusing to use file {}: symlink is not allowed",
                    path.display()
                );
            }
            if !ft.is_file() {
                anyhow::bail!(
                    "Refusing to use file {}: expected regular file",
                    path.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("Unable to inspect {}", path.display()));
        }
    }
    set_permissions(path, 0o600)
        .with_context(|| format!("Unable to set permissions on {}", path.display()))?;
    Ok(())
}

fn ensure_directory_not_symlink(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                anyhow::bail!(
                    "Refusing to use {} {}: symlink is not allowed",
                    label,
                    path.display()
                );
            }
            if !ft.is_dir() {
                anyhow::bail!(
                    "Refusing to use {} {}: expected directory",
                    label,
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Unable to inspect {} {}", label, path.display()))
        }
    }
}

fn ensure_regular_file_or_missing(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                anyhow::bail!(
                    "Refusing to use {} {}: symlink is not allowed",
                    label,
                    path.display()
                );
            }
            if !ft.is_file() {
                anyhow::bail!(
                    "Refusing to use {} {}: expected regular file",
                    label,
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Unable to inspect {} {}", label, path.display()))
        }
    }
}

#[cfg(unix)]
fn set_permissions(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_permissions(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}
