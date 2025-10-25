//! Vault operations module.
//!
//! The `Vault` type is responsible for resolving vault-relative paths and
//! performing lightweight validation of an Obsidian vault before handing work to
//! other subsystems. Full I/O heavy operations (reading files, indexing, etc.)
//! live in dedicated modules so they can be unit tested independently.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, TimeZone, Utc};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::types::VaultPaths;
use crate::{MetadataMap, NoteId, NoteRecord};

/// Lightweight description of a note discovered during vault inventory.
#[derive(Debug, Clone)]
pub struct NoteInventoryEntry {
    /// Unique identifier derived from the file path.
    pub id: NoteId,
    /// Relative path (including extension) from the vault root.
    pub relative_path: PathBuf,
    /// Absolute filesystem path to the note file.
    pub absolute_path: PathBuf,
    /// Filesystem modification timestamp captured during inventory.
    pub file_modified_at: DateTime<Utc>,
    /// Optional filesystem creation timestamp.
    pub created_at: Option<DateTime<Utc>>,
}

/// Cached snapshot of vault inventory for fast lookups.
#[derive(Debug, Clone)]
pub struct InventorySnapshot {
    paths: Arc<VaultPaths>,
    settings: Arc<VaultSettings>,
    arrowhead_relative: PathBuf,
    attachments_relative: Option<PathBuf>,
    entries: Vec<NoteInventoryEntry>,
    by_id: HashMap<String, usize>,
    by_path: HashMap<PathBuf, usize>,
}

impl InventorySnapshot {
    fn new(
        paths: Arc<VaultPaths>,
        settings: Arc<VaultSettings>,
        entries: Vec<NoteInventoryEntry>,
    ) -> Self {
        let mut by_id = HashMap::with_capacity(entries.len());
        let mut by_path = HashMap::with_capacity(entries.len());

        for (index, entry) in entries.iter().enumerate() {
            by_id.insert(entry.id.clone(), index);
            by_path.insert(entry.relative_path.clone(), index);
        }

        let arrowhead_relative = paths
            .arrowhead_dir
            .strip_prefix(&paths.root)
            .unwrap_or_else(|_| Path::new(".arrowhead"))
            .to_path_buf();

        let attachments_relative = paths
            .attachments_dir
            .as_ref()
            .and_then(|dir| dir.strip_prefix(&paths.root).ok())
            .map(Path::to_path_buf);

        Self {
            paths,
            settings,
            arrowhead_relative,
            attachments_relative,
            entries,
            by_id,
            by_path,
        }
    }

    /// Iterate over all inventory entries.
    pub fn entries(&self) -> &[NoteInventoryEntry] {
        &self.entries
    }

    /// Returns a reference to the underlying vault paths.
    pub fn paths(&self) -> &VaultPaths {
        &self.paths
    }

    /// Look up a note inventory entry by note identifier.
    pub fn get_by_id(&self, note_id: &str) -> Option<&NoteInventoryEntry> {
        self.by_id
            .get(note_id)
            .and_then(|index| self.entries.get(*index))
    }

    /// Look up a note inventory entry by relative or absolute filesystem path.
    pub fn get_by_path<P: AsRef<Path>>(&self, path: P) -> Option<&NoteInventoryEntry> {
        let relative = self.normalise_path(path.as_ref())?;
        self.by_path
            .get(&relative)
            .and_then(|index| self.entries.get(*index))
    }

    /// Derive a note identifier for the supplied path if it belongs to the vault.
    pub fn note_id_for_path<P: AsRef<Path>>(&self, path: P) -> Option<NoteId> {
        let relative = self.normalise_path(path.as_ref())?;
        if !is_markdown(&relative) {
            return None;
        }
        derive_note_id(&relative).ok()
    }

    /// Consume the snapshot and return the owned entries.
    pub fn into_entries(self) -> Vec<NoteInventoryEntry> {
        self.entries
    }

    fn normalise_path(&self, path: &Path) -> Option<PathBuf> {
        if path.as_os_str().is_empty() {
            return None;
        }

        let candidate = if path.is_absolute() {
            path.strip_prefix(&self.paths.root).ok()?.to_path_buf()
        } else {
            path.to_path_buf()
        };

        let relative = normalise_relative_path(&candidate)?;
        if self.is_ignored(&relative) {
            return None;
        }
        Some(relative)
    }

    fn is_ignored(&self, relative: &Path) -> bool {
        if relative.starts_with(&self.arrowhead_relative) {
            return true;
        }

        if let Some(attachments) = &self.attachments_relative {
            if relative.starts_with(attachments) {
                return true;
            }
        }

        is_ignored(relative, self.settings.ignored_folders())
    }
}

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

    /// Resolve the absolute path for the Arrowhead working directory.
    pub fn resolve_arrowhead_dir(&self) -> PathBuf {
        self.root.join(&self.arrowhead_dir_name)
    }
}

/// Declarative Obsidian vault settings loaded from `.obsidian`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VaultSettings {
    attachments_folder: Option<PathBuf>,
    ignored_folders: Vec<PathBuf>,
}

impl VaultSettings {
    /// Access the configured attachments folder relative to the vault root.
    pub fn attachments_folder(&self) -> Option<&Path> {
        self.attachments_folder.as_deref()
    }

    /// Paths that should be ignored during indexing, relative to the vault root.
    pub fn ignored_folders(&self) -> &[PathBuf] {
        &self.ignored_folders
    }
}

/// Lightweight accessor for an Obsidian vault.
#[derive(Debug, Clone)]
pub struct Vault {
    paths: Arc<VaultPaths>,
    settings: Arc<VaultSettings>,
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
        let obsidian_dir = root.join(".obsidian");

        let mut settings = load_obsidian_settings(&obsidian_dir);
        if let Some(config_attachments) = &config.attachments_dir {
            if let Some(relative) = normalise_relative_path(config_attachments.as_path()) {
                settings.attachments_folder = Some(relative);
            }
        }

        let attachments_dir = settings
            .attachments_folder()
            .map(|relative| root.join(relative));

        let attachments_dir_display = attachments_dir
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| "<default>".to_string());
        let ignored_paths: Vec<String> = settings
            .ignored_folders()
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        let ignored_summary = if ignored_paths.is_empty() {
            "<none>".to_string()
        } else {
            ignored_paths.join(", ")
        };

        info!(
            root = %root.display(),
            arrowhead_dir = %arrowhead_dir.display(),
            attachments_dir = attachments_dir_display.as_str(),
            ignored_folders = ignored_summary.as_str(),
            "initialised vault configuration"
        );

        Ok(Self {
            paths: Arc::new(VaultPaths::new(
                root,
                arrowhead_dir,
                obsidian_dir,
                attachments_dir,
            )),
            settings: Arc::new(settings),
        })
    }

    /// Access the resolved vault paths.
    pub fn paths(&self) -> &VaultPaths {
        &self.paths
    }

    /// Access the loaded Obsidian settings.
    pub fn settings(&self) -> &VaultSettings {
        &self.settings
    }

    /// Ensure the Arrowhead working directory exists inside the vault.
    pub fn ensure_arrowhead_dirs(&self) -> Result<()> {
        debug!(
            arrowhead_dir = %self.paths.arrowhead_dir.display(),
            "ensuring arrowhead working directories"
        );
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

        info!(
            arrowhead_dir = %self.paths.arrowhead_dir.display(),
            "arrowhead working directories ready"
        );
        Ok(())
    }

    /// Returns the absolute path to a note relative to the vault root.
    pub fn note_path<P: AsRef<Path>>(&self, relative: P) -> PathBuf {
        self.paths.root.join(relative)
    }

    /// Resolve a note identifier to a relative path inside the vault (without extension).
    pub fn relative_path_from_id(&self, note_id: &str) -> Result<PathBuf> {
        normalise_relative_path(Path::new(note_id))
            .ok_or_else(|| anyhow!("invalid note id: {note_id}"))
    }

    /// Resolve a note identifier to an absolute path including the `.md` extension.
    pub fn note_file_path(&self, note_id: &str) -> Result<PathBuf> {
        let mut relative = self.relative_path_from_id(note_id)?;
        relative.set_extension("md");
        Ok(self.note_path(relative))
    }

    /// Write the supplied metadata/body to the given note identifier, creating parent directories.
    pub fn write_note(&self, note_id: &str, metadata: &MetadataMap, body: &str) -> Result<()> {
        let mut relative = self.relative_path_from_id(note_id)?;
        relative.set_extension("md");
        let absolute = self.note_path(&relative);

        info!(
            note_id = note_id,
            path = %absolute.display(),
            "writing note to disk"
        );

        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create note directory {}", parent.display()))?;
        }

        let content = compose_note_content(metadata, body)?;
        fs::write(&absolute, content)
            .with_context(|| format!("failed to write note file {}", absolute.display()))
    }

    /// List all markdown note identifiers in the vault.
    pub fn list_note_ids(&self) -> Result<Vec<NoteId>> {
        let inventory = self.inventory()?;
        Ok(inventory.into_iter().map(|entry| entry.id).collect())
    }

    /// List all markdown note paths relative to the vault root.
    pub fn list_markdown_paths(&self) -> Result<Vec<PathBuf>> {
        collect_markdown_files(
            &self.paths.root,
            Some(&self.paths.arrowhead_dir),
            self.paths.attachments_dir.as_deref(),
            self.settings.ignored_folders(),
        )
    }

    /// Normalise a filesystem path to a vault-relative markdown path.
    pub fn resolve_relative_note_path<P: AsRef<Path>>(&self, path: P) -> Option<PathBuf> {
        let relative = self.normalise_path(path.as_ref())?;
        if !is_markdown(&relative) {
            return None;
        }
        Some(relative)
    }

    /// Normalise a path and return its note identifier alongside the relative path.
    pub fn normalise_note_path<P: AsRef<Path>>(&self, path: P) -> Option<(NoteId, PathBuf)> {
        let relative = self.resolve_relative_note_path(path)?;
        let note_id = derive_note_id(&relative).ok()?;
        Some((note_id, relative))
    }

    /// Derive a note identifier from a filesystem path pointing to a markdown file.
    pub fn note_id_from_path<P: AsRef<Path>>(&self, path: P) -> Option<NoteId> {
        self.normalise_note_path(path).map(|(id, _)| id)
    }

    /// Construct an inventory entry for the supplied path if the note exists.
    pub fn inventory_entry_for_path<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<Option<NoteInventoryEntry>> {
        let (note_id, relative_path) = match self.normalise_note_path(path.as_ref()) {
            Some(value) => value,
            None => return Ok(None),
        };

        let absolute_path = self.note_path(&relative_path);

        let meta = match fs::metadata(&absolute_path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("failed to inspect note {}", absolute_path.display())
                });
            }
        };

        let modified = system_time_to_utc(meta.modified().unwrap_or_else(|_| SystemTime::now()))?;
        let created = meta
            .created()
            .ok()
            .and_then(|time| system_time_to_utc(time).ok());

        Ok(Some(NoteInventoryEntry {
            id: note_id,
            relative_path,
            absolute_path,
            file_modified_at: modified,
            created_at: created,
        }))
    }

    fn normalise_path(&self, path: &Path) -> Option<PathBuf> {
        if path.as_os_str().is_empty() {
            return None;
        }

        let candidate = if path.is_absolute() {
            path.strip_prefix(&self.paths.root).ok()?.to_path_buf()
        } else {
            path.to_path_buf()
        };

        let relative = normalise_relative_path(&candidate)?;

        if let Ok(arrowhead_relative) = self.paths.arrowhead_dir.strip_prefix(&self.paths.root) {
            if relative.starts_with(arrowhead_relative) {
                return None;
            }
        }

        if let Some(attachments_dir) = &self.paths.attachments_dir {
            if let Ok(attachments_relative) = attachments_dir.strip_prefix(&self.paths.root) {
                if relative.starts_with(attachments_relative) {
                    return None;
                }
            }
        }

        if is_ignored(&relative, self.settings.ignored_folders()) {
            return None;
        }

        Some(relative)
    }

    /// Load a note by its identifier.
    pub fn load_note(&self, note_id: &str) -> Result<NoteRecord> {
        let inventory = self.inventory()?;
        let entry = inventory
            .into_iter()
            .find(|entry| entry.id == note_id)
            .with_context(|| format!("note {note_id} not found in vault"))?;
        self.load_note_from_entry(&entry)
    }

    /// Build and cache vault inventory for reuse across operations.
    pub fn inventory_snapshot(&self) -> Result<InventorySnapshot> {
        let entries = self.build_inventory_entries()?;
        Ok(InventorySnapshot::new(
            self.paths.clone(),
            self.settings.clone(),
            entries,
        ))
    }

    /// Build an inventory of all markdown notes without parsing their contents.
    pub fn inventory(&self) -> Result<Vec<NoteInventoryEntry>> {
        Ok(self.inventory_snapshot()?.into_entries())
    }

    fn build_inventory_entries(&self) -> Result<Vec<NoteInventoryEntry>> {
        let mut entries = Vec::new();
        let mut ids = BTreeMap::new();

        debug!(
            root = %self.paths.root.display(),
            ignored = self.settings.ignored_folders().len(),
            "building vault inventory"
        );

        for relative_path in collect_markdown_files(
            &self.paths.root,
            Some(&self.paths.arrowhead_dir),
            self.paths.attachments_dir.as_deref(),
            self.settings.ignored_folders(),
        )? {
            let note_id = derive_note_id(&relative_path)?;
            if ids.contains_key(&note_id) {
                warn!(
                    note_id = %note_id,
                    path = %relative_path.display(),
                    "duplicate note identifier detected during inventory"
                );
                bail!("duplicate note identifier detected: {note_id}");
            }

            let absolute_path = self.note_path(&relative_path);
            let file_meta = fs::metadata(&absolute_path)
                .with_context(|| format!("failed to stat note {}", absolute_path.display()))?;
            let modified =
                system_time_to_utc(file_meta.modified().unwrap_or_else(|_| SystemTime::now()))?;
            let created = file_meta
                .created()
                .ok()
                .and_then(|time| system_time_to_utc(time).ok());

            let entry = NoteInventoryEntry {
                id: note_id.clone(),
                relative_path: relative_path.clone(),
                absolute_path,
                file_modified_at: modified,
                created_at: created,
            };
            ids.insert(note_id, entries.len());
            entries.push(entry);
        }

        info!(count = entries.len(), "completed vault inventory build");
        Ok(entries)
    }

    /// Load a note using a precomputed inventory entry.
    pub fn load_note_from_entry(&self, entry: &NoteInventoryEntry) -> Result<NoteRecord> {
        let raw = fs::read_to_string(&entry.absolute_path)
            .with_context(|| format!("failed to read note {}", entry.absolute_path.display()))?;

        let (frontmatter_str, body) = split_frontmatter(&raw);
        let metadata = parse_frontmatter(frontmatter_str).with_context(|| {
            format!(
                "invalid frontmatter in note {}",
                entry.relative_path.display()
            )
        })?;

        let title = derive_title(&metadata, body);

        debug!(
            note_id = %entry.id,
            path = %entry.relative_path.display(),
            "loaded note from inventory entry"
        );

        Ok(NoteRecord {
            id: entry.id.clone(),
            title,
            metadata,
            content: body.to_string(),
            relative_path: entry.relative_path.clone(),
            file_modified_at: entry.file_modified_at,
            created_at: entry.created_at,
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

    #[test]
    fn inventory_entries_can_load_notes() {
        let vault = build_vault();
        let inventory = vault.inventory().expect("inventory builds");

        assert!(!inventory.is_empty());
        let entry = inventory
            .iter()
            .find(|entry| entry.id == "2024-01-15")
            .expect("locate fixture note");

        assert_eq!(entry.relative_path, PathBuf::from("2024-01-15.md"));
        assert!(entry.absolute_path.is_absolute());
        assert!(entry.file_modified_at.timestamp() > 0);

        let record = vault
            .load_note_from_entry(entry)
            .expect("load via inventory entry");
        assert_eq!(record.id, "2024-01-15");
        assert_eq!(record.relative_path, entry.relative_path);
        assert_eq!(record.file_modified_at, entry.file_modified_at);
    }

    #[test]
    fn inventory_snapshot_supports_id_and_path_lookup() {
        let vault = build_vault();
        let snapshot = vault.inventory_snapshot().expect("snapshot builds");
        let entry = snapshot
            .get_by_id("2024-01-15")
            .expect("note present in snapshot");

        assert_eq!(entry.relative_path, PathBuf::from("2024-01-15.md"));

        let absolute = vault.note_path(&entry.relative_path);
        let by_absolute = snapshot
            .get_by_path(&absolute)
            .expect("snapshot resolves absolute path");
        assert_eq!(by_absolute.id, entry.id);

        let by_relative = snapshot
            .get_by_path(&entry.relative_path)
            .expect("snapshot resolves relative path");
        assert_eq!(by_relative.id, entry.id);
    }

    #[test]
    fn note_id_from_path_normalises_absolute_paths() {
        let vault = build_vault();
        let absolute = vault.note_path("2024-01-15.md");
        let note_id = vault
            .note_id_from_path(&absolute)
            .expect("note id derived from absolute path");
        assert_eq!(note_id, "2024-01-15");

        assert!(
            vault
                .note_id_from_path(vault.note_path(".arrowhead/index.db"))
                .is_none()
        );
    }

    #[test]
    fn inventory_entry_for_path_returns_none_for_missing_files() {
        let vault = build_vault();
        let missing = vault.note_path("does-not-exist.md");
        let entry = vault
            .inventory_entry_for_path(&missing)
            .expect("inventory entry lookup");
        assert!(entry.is_none());
    }

    #[test]
    fn list_note_ids_respects_ignored_folders() {
        let vault = build_vault();
        let ids = vault.list_note_ids().expect("listing succeeds");

        assert!(ids.iter().any(|id| id == "2024-01-15"));
        assert!(
            !ids.iter()
                .any(|id| id.starts_with("Templates") || id.contains("Meeting Template"))
        );
    }
}

fn collect_markdown_files(
    root: &Path,
    arrowhead_dir: Option<&Path>,
    attachments_dir: Option<&Path>,
    ignored_folders: &[PathBuf],
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

            let relative = path.strip_prefix(root).unwrap_or_else(|_| Path::new(""));

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

                if is_ignored(relative, ignored_folders) {
                    continue;
                }

                stack.push(path);
            } else if file_type.is_file() && is_markdown(&path) {
                if is_ignored(relative, ignored_folders) {
                    continue;
                }

                let relative = relative.to_path_buf();
                files.push(relative);
            }
        }
    }

    files.sort();
    debug!(
        root = %root.display(),
        collected = files.len(),
        "collected markdown files for inventory"
    );
    Ok(files)
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

fn is_ignored(relative: &Path, ignored_folders: &[PathBuf]) -> bool {
    if relative.as_os_str().is_empty() {
        return false;
    }

    ignored_folders
        .iter()
        .any(|ignore| relative.starts_with(ignore))
}

fn compose_note_content(metadata: &MetadataMap, body: &str) -> Result<String> {
    let mut content = String::new();

    if !metadata.is_empty() {
        let yaml = metadata_to_yaml(metadata)?;
        content.push_str("---\n");
        content.push_str(&yaml);
        if !yaml.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("---\n\n");
    }

    content.push_str(body);
    Ok(content)
}

fn metadata_to_yaml(metadata: &MetadataMap) -> Result<String> {
    let mut mapping = serde_yaml::Mapping::new();
    for (key, value) in metadata {
        let yaml_value = serde_yaml::to_value(value.clone())?;
        mapping.insert(serde_yaml::Value::String(key.clone()), yaml_value);
    }

    let yaml = serde_yaml::to_string(&mapping)?;
    Ok(yaml.trim_end().to_string() + "\n")
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

#[derive(Debug, Deserialize)]
struct ObsidianAppConfig {
    #[serde(rename = "attachmentFolderPath")]
    attachment_folder_path: Option<String>,
    #[serde(rename = "userIgnoreFilters")]
    user_ignore_filters: Option<Vec<String>>,
}

fn load_obsidian_settings(obsidian_dir: &Path) -> VaultSettings {
    let mut settings = VaultSettings::default();
    let app_path = obsidian_dir.join("app.json");

    match fs::read_to_string(&app_path) {
        Ok(content) => match serde_json::from_str::<ObsidianAppConfig>(&content) {
            Ok(app) => {
                if let Some(folder) = app.attachment_folder_path {
                    if let Some(relative) = normalise_relative_str(&folder) {
                        settings.attachments_folder = Some(relative);
                    }
                }

                if let Some(filters) = app.user_ignore_filters {
                    settings.ignored_folders = filters
                        .into_iter()
                        .filter_map(|filter| normalise_relative_str(&filter))
                        .collect();
                    settings.ignored_folders.sort();
                    settings.ignored_folders.dedup();
                }
            }
            Err(err) => {
                warn!("failed to parse Obsidian app.json: {}", err);
            }
        },
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!("failed to read Obsidian settings: {}", err);
            }
        }
    }

    settings
}

pub(crate) fn normalise_relative_str(value: &str) -> Option<PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let trimmed = trimmed.trim_end_matches(|c| c == '/' || c == char::from(b'\\'));
    if trimmed.is_empty() {
        return None;
    }

    normalise_relative_path(Path::new(trimmed))
}

pub(crate) fn normalise_relative_path(path: &Path) -> Option<PathBuf> {
    let mut cleaned = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => continue,
            Component::CurDir => continue,
            Component::ParentDir => continue,
            Component::Normal(part) => cleaned.push(part),
        }
    }

    if cleaned.as_os_str().is_empty() {
        None
    } else {
        Some(cleaned)
    }
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
