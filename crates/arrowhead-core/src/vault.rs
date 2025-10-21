//! Vault operations module.
//!
//! The `Vault` type is responsible for resolving vault-relative paths and
//! performing lightweight validation of an Obsidian vault before handing work to
//! other subsystems. Full I/O heavy operations (reading files, indexing, etc.)
//! live in dedicated modules so they can be unit tested independently.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeZone, Utc};
use regex::Regex;
use serde_json::Value;

use crate::types::VaultPaths;
use crate::{MetadataMap, NoteId, NoteRecord};

/// Configuration values required to initialise a [`Vault`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultConfig {
    /// Absolute path to the root of the Obsidian vault.
    pub root: PathBuf,
    /// Optional custom attachments directory relative to `root`.
    pub attachments_dir: Option<PathBuf>,
    /// Name of the internal Arrowhead directory (defaults to `.arrowhead`).
    pub arrowhead_dir_name: String,
}

impl VaultConfig {
    /// Create a new configuration using the provided root directory.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            attachments_dir: None,
            arrowhead_dir_name: ".arrowhead".to_string(),
        }
    }

    /// Resolve the absolute path for the attachments directory.
    pub fn resolve_attachments_dir(&self) -> Option<PathBuf> {
        self.attachments_dir.as_ref().map(|dir| self.root.join(dir))
    }

    /// Resolve the absolute path for the Arrowhead working directory.
    pub fn resolve_arrowhead_dir(&self) -> PathBuf {
        self.root.join(&self.arrowhead_dir_name)
    }
}

/// Lightweight accessor for an Obsidian vault.
#[derive(Debug, Clone)]
pub struct Vault {
    paths: Arc<VaultPaths>,
}

impl Vault {
    /// Create a new [`Vault`] from configuration.
    pub fn new(config: VaultConfig) -> Result<Self> {
        if config.root.as_os_str().is_empty() {
            bail!("vault root must not be empty");
        }

        let root = fs::canonicalize(&config.root)
            .with_context(|| format!("unable to resolve vault root {}", config.root.display()))?;

        if !root.is_dir() {
            bail!("{} is not a directory", root.display());
        }

        let arrowhead_dir = config.resolve_arrowhead_dir();
        let attachments_dir = config.resolve_attachments_dir();

        Ok(Self {
            paths: Arc::new(VaultPaths::new(root, arrowhead_dir, attachments_dir)),
        })
    }

    /// Access the resolved vault paths.
    pub fn paths(&self) -> &VaultPaths {
        &self.paths
    }

    /// Ensure the Arrowhead working directory exists inside the vault.
    pub fn ensure_arrowhead_dirs(&self) -> Result<()> {
        if let Some(parent) = self.paths.arrowhead_dir.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create parent directory {}", parent.display())
            })?;
        }

        fs::create_dir_all(&self.paths.arrowhead_dir).with_context(|| {
            format!(
                "failed to create arrowhead directory {}",
                self.paths.arrowhead_dir.display()
            )
        })?;

        Ok(())
    }

    /// Returns the absolute path to a note relative to the vault root.
    pub fn note_path<P: AsRef<Path>>(&self, relative: P) -> PathBuf {
        self.paths.root.join(relative)
    }

    /// List all markdown note identifiers in the vault.
    pub fn list_note_ids(&self) -> Result<Vec<NoteId>> {
        let mut ids = BTreeMap::new();

        for path in self.list_markdown_paths()? {
            let note_id = derive_note_id(&path)?;
            if ids.insert(note_id.clone(), path).is_some() {
                bail!("duplicate note identifier detected: {note_id}");
            }
        }

        Ok(ids.into_keys().collect())
    }

    /// List all markdown note paths relative to the vault root.
    pub fn list_markdown_paths(&self) -> Result<Vec<PathBuf>> {
        collect_markdown_files(
            &self.paths.root,
            Some(&self.paths.arrowhead_dir),
            self.paths.attachments_dir.as_deref(),
        )
    }

    /// Load a note by its identifier.
    pub fn load_note(&self, note_id: &str) -> Result<NoteRecord> {
        let relative_path = self
            .list_markdown_paths()?
            .into_iter()
            .find(|path| derive_note_id(path).map_or(false, |id| id == note_id))
            .with_context(|| format!("note {note_id} not found in vault"))?;

        let absolute_path = self.note_path(&relative_path);
        let raw = fs::read_to_string(&absolute_path)
            .with_context(|| format!("failed to read note {}", absolute_path.display()))?;

        let (frontmatter_str, body) = split_frontmatter(&raw);
        let metadata = parse_frontmatter(frontmatter_str)
            .with_context(|| format!("invalid frontmatter in note {}", relative_path.display()))?;

        let title = derive_title(&metadata, body);

        let file_meta = fs::metadata(&absolute_path)
            .with_context(|| format!("failed to stat note {}", absolute_path.display()))?;
        let file_modified_at =
            system_time_to_utc(file_meta.modified().unwrap_or_else(|_| SystemTime::now()))?;
        let created_at = file_meta
            .created()
            .ok()
            .and_then(|time| system_time_to_utc(time).ok());

        Ok(NoteRecord {
            id: note_id.to_string(),
            title,
            metadata,
            content: body.to_string(),
            relative_path,
            file_modified_at,
            created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("test-vault")
    }

    fn build_vault() -> Vault {
        let root = fixture_root();
        Vault::new(VaultConfig::new(root)).expect("fixture vault should initialise")
    }

    #[test]
    fn list_note_ids_returns_expected_entries() {
        let vault = build_vault();
        let mut ids = vault.list_note_ids().expect("listing notes succeeds");
        ids.sort();

        assert!(ids.contains(&"2024-01-15".to_string()));
        assert!(ids.contains(&"Photography Equipment".to_string()));
        assert!(ids.contains(&"Edge Case - No Frontmatter".to_string()));
    }

    #[test]
    fn load_note_parses_frontmatter_and_body() {
        let vault = build_vault();
        let note = vault
            .load_note("2024-01-15")
            .expect("should load existing note");

        assert_eq!(note.id, "2024-01-15");
        assert_eq!(note.title.as_deref(), Some("January 15, 2024"));
        assert_eq!(note.relative_path, PathBuf::from("2024-01-15.md"));
        assert!(note.metadata.contains_key("category"));
        assert!(note.metadata.contains_key("tags"));
        assert!(note.content.contains("Today was a productive day."));
        assert!(!note.content.contains("category: journal"));

        let file_modified_at = chrono::Utc.timestamp_opt(0, 0).earliest().unwrap();
        assert!(note.file_modified_at > file_modified_at);
    }

    #[test]
    fn load_note_handles_notes_without_frontmatter() {
        let vault = build_vault();
        let note = vault
            .load_note("Edge Case - No Frontmatter")
            .expect("note without frontmatter should load");

        assert!(note.metadata.is_empty());
        assert!(note.content.starts_with("# Note Without Frontmatter"));
    }

    #[test]
    fn load_note_handles_empty_frontmatter() {
        let vault = build_vault();
        let note = vault
            .load_note("Edge Case - Empty Frontmatter")
            .expect("note with empty frontmatter should load");

        assert!(note.metadata.is_empty());
        assert!(note.content.contains("# Note With Empty Frontmatter"));
    }
}

fn collect_markdown_files(
    root: &Path,
    arrowhead_dir: Option<&Path>,
    attachments_dir: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();

    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed to read directory {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;

            if file_type.is_dir() {
                if Some(path.as_path()) == arrowhead_dir {
                    continue;
                }

                if attachments_dir
                    .map(|attachments| path.starts_with(attachments))
                    .unwrap_or(false)
                {
                    continue;
                }

                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with('.') {
                        continue;
                    }
                    if name.eq_ignore_ascii_case("attachments") {
                        continue;
                    }
                }

                stack.push(path);
            } else if file_type.is_file() && is_markdown(&path) {
                let relative = path
                    .strip_prefix(root)
                    .with_context(|| {
                        format!("failed to compute relative path for {}", path.display())
                    })?
                    .to_path_buf();
                files.push(relative);
            }
        }
    }

    files.sort();
    Ok(files)
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

fn derive_note_id(path: &Path) -> Result<NoteId> {
    let mut without_ext = path.to_path_buf();
    without_ext.set_extension("");
    let id = without_ext
        .to_string_lossy()
        .replace("\\", "/")
        .trim()
        .to_string();

    if id.is_empty() {
        bail!("note path {:?} does not produce a valid identifier", path);
    }

    Ok(id)
}

fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    static FRONTMATTER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)\A---\r?\n(.*?)\r?\n---\r?\n?").expect("valid frontmatter regex")
    });

    if let Some(captures) = FRONTMATTER_RE.captures(raw) {
        let frontmatter = captures.get(1).map(|m| m.as_str());
        let body_start = captures.get(0).map(|m| m.end()).unwrap_or(0);
        let body = &raw[body_start..];
        (frontmatter, body)
    } else {
        (None, raw)
    }
}

fn parse_frontmatter(frontmatter: Option<&str>) -> Result<MetadataMap> {
    let mut metadata = MetadataMap::default();

    if let Some(frontmatter) = frontmatter {
        if frontmatter.trim().is_empty() {
            return Ok(metadata);
        }

        let value: Value =
            serde_yaml::from_str(frontmatter).context("failed to parse YAML frontmatter")?;

        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    metadata.insert(key, value);
                }
            }
            Value::Null => {}
            other => {
                bail!("frontmatter must be a mapping, found {other}");
            }
        }
    }

    Ok(metadata)
}

fn derive_title(metadata: &MetadataMap, body: &str) -> Option<String> {
    if let Some(Value::String(title)) = metadata.get("title") {
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            let heading = trimmed.trim_start_matches('#').trim();
            if !heading.is_empty() {
                return Some(heading.to_string());
            }
        }
    }

    None
}

fn system_time_to_utc(time: SystemTime) -> Result<DateTime<Utc>> {
    let duration = time
        .duration_since(UNIX_EPOCH)
        .context("system time is before UNIX_EPOCH")?;
    let nanos = duration.subsec_nanos();
    let quantised_nanos = (nanos / 1_000) * 1_000;
    Utc.timestamp_opt(duration.as_secs() as i64, quantised_nanos)
        .single()
        .context("invalid timestamp returned by system clock")
}
