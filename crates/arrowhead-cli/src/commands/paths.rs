//! Shared filesystem helpers for workspace-aware commands.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use arrowhead_core::vault::normalise_relative_path;

/// Resolve a user-provided path so it stays within the vault root.
pub fn resolve_relative_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let candidate = if path.is_absolute() {
        path.strip_prefix(root)
            .with_context(|| {
                format!(
                    "path {} must be inside the workspace {}",
                    path.display(),
                    root.display()
                )
            })?
            .to_path_buf()
    } else {
        path.to_path_buf()
    };

    normalise_relative_path(&candidate).ok_or_else(|| {
        anyhow!(
            "path {} cannot be resolved relative to the workspace (parent directories are not allowed)",
            path.display()
        )
    })
}

/// Convert a resolved path into a normalised string suitable for serialization.
pub fn relative_path_string(root: &Path, path: &Path) -> Result<String> {
    let relative = resolve_relative_path(root, path)?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}
