//! Vault operations module.
//!
//! The `Vault` type is responsible for resolving vault-relative paths and
//! performing lightweight validation of an Obsidian vault before handing work to
//! other subsystems. Full I/O heavy operations (reading files, indexing, etc.)
//! live in dedicated modules so they can be unit tested independently.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
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

use crate::metrics::{
    DEFAULT_DAY_START_HOUR, DEFAULT_METRIC_REFERENCE_PREFIX, DEFAULT_METRICS_EXTENSION,
    DEFAULT_METRICS_ROOT, DEFAULT_METRICS_WRITE_FILE_NAME, DEFAULT_WEEK_START_DAY,
    MetricsConfigFile, MetricsConventions, MetricsConventionsSource, MetricsFileEntry,
};
use crate::types::VaultPaths;
use crate::workspace::{
    WORKSPACE_CONFIG_FILE, WorkspaceFile, WorkspaceKind, WorkspaceSource, load_workspace_file,
};
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
    daily_note_format: Option<String>,
    link_style: Option<String>,
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

    /// Daily note format if configured.
    pub fn daily_note_format(&self) -> Option<&str> {
        self.daily_note_format.as_deref()
    }

    /// Preferred link style if configured.
    pub fn link_style(&self) -> Option<&str> {
        self.link_style.as_deref()
    }
}

/// Lightweight accessor for an Obsidian vault.
#[derive(Debug, Clone)]
pub struct Vault {
    paths: Arc<VaultPaths>,
    settings: Arc<VaultSettings>,
    metrics: Arc<MetricsConventions>,
    workspace_kind: WorkspaceKind,
    workspace_source: WorkspaceSource,
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
        let workspace_file_path = arrowhead_dir.join(WORKSPACE_CONFIG_FILE);
        let obsidian_present = obsidian_dir.exists();

        let workspace_file = load_workspace_file(&workspace_file_path)?;
        let arrowhead_settings = workspace_file.clone().map(settings_from_workspace_file);
        let workspace_metrics = workspace_file
            .as_ref()
            .and_then(|file| file.metrics.clone());

        let (workspace_kind, workspace_source, mut settings) = if obsidian_present {
            if arrowhead_settings.is_some() {
                warn!(
                    obsidian = %obsidian_dir.display(),
                    workspace = %workspace_file_path.display(),
                    "found Obsidian metadata and Arrowhead workspace config; preferring Obsidian settings"
                );
            }
            (
                WorkspaceKind::Obsidian,
                WorkspaceSource::Obsidian(obsidian_dir.clone()),
                load_obsidian_settings(&obsidian_dir),
            )
        } else if let Some(file_settings) = arrowhead_settings {
            (
                WorkspaceKind::Generic,
                WorkspaceSource::Arrowhead(workspace_file_path.clone()),
                file_settings,
            )
        } else {
            (
                WorkspaceKind::Generic,
                WorkspaceSource::Default,
                VaultSettings::default(),
            )
        };
        if let Some(config_attachments) = &config.attachments_dir {
            if let Some(relative) = normalise_relative_path(config_attachments.as_path()) {
                settings.attachments_folder = Some(relative);
            }
        }
        let metrics =
            resolve_metrics_conventions(&obsidian_dir, &workspace_file_path, workspace_metrics);

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
            workspace_kind = ?workspace_kind,
            workspace_source = ?workspace_source,
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
            metrics: Arc::new(metrics),
            workspace_kind,
            workspace_source,
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

    /// Access the resolved metrics conventions.
    pub fn metrics_conventions(&self) -> &MetricsConventions {
        &self.metrics
    }

    /// Identify which workspace flavour is active.
    pub fn workspace_kind(&self) -> WorkspaceKind {
        self.workspace_kind
    }

    /// Describe the configuration source backing the current workspace.
    pub fn workspace_source(&self) -> &WorkspaceSource {
        &self.workspace_source
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
        let relative = append_md_extension(self.relative_path_from_id(note_id)?);
        Ok(self.note_path(relative))
    }

    /// Write the supplied metadata/body to the given note identifier, creating parent directories.
    pub fn write_note(&self, note_id: &str, metadata: &MetadataMap, body: &str) -> Result<()> {
        let relative = append_md_extension(self.relative_path_from_id(note_id)?);
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

    /// Normalise a filesystem path to a vault-relative metrics file path.
    pub fn resolve_relative_metrics_path<P: AsRef<Path>>(&self, path: P) -> Option<PathBuf> {
        let relative = self.normalise_path(path.as_ref())?;
        if !relative.starts_with(&self.metrics.root) {
            return None;
        }

        let path_value = relative.to_string_lossy();
        if self
            .metrics
            .extensions
            .iter()
            .any(|suffix| path_value.ends_with(suffix))
        {
            Some(relative)
        } else {
            None
        }
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

    /// Construct a metrics file entry for the supplied path if the file exists.
    pub fn metrics_entry_for_path<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<Option<MetricsFileEntry>> {
        let relative_path = match self.resolve_relative_metrics_path(path.as_ref()) {
            Some(path) => path,
            None => return Ok(None),
        };

        let absolute_path = self.note_path(&relative_path);
        let meta = match fs::metadata(&absolute_path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("failed to inspect metrics file {}", absolute_path.display())
                });
            }
        };

        let modified = system_time_to_utc(meta.modified().unwrap_or_else(|_| SystemTime::now()))?;
        Ok(Some(MetricsFileEntry {
            relative_path,
            absolute_path,
            file_modified_at: modified,
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

    /// Discover metrics files using the resolved metrics conventions.
    pub fn metrics_files(&self) -> Result<Vec<MetricsFileEntry>> {
        let root = self.paths.root.join(&self.metrics.root);
        if !root.exists() {
            return Ok(Vec::new());
        }
        if !root.is_dir() {
            warn!(
                metrics_root = %root.display(),
                "configured metrics root is not a directory; skipping metrics discovery"
            );
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();
        for absolute_path in collect_metrics_files(&root, &self.metrics.extensions)? {
            let relative_path = absolute_path
                .strip_prefix(&self.paths.root)
                .unwrap_or(&absolute_path)
                .to_path_buf();
            let file_meta = fs::metadata(&absolute_path).with_context(|| {
                format!("failed to stat metrics file {}", absolute_path.display())
            })?;
            let modified =
                system_time_to_utc(file_meta.modified().unwrap_or_else(|_| SystemTime::now()))?;
            entries.push(MetricsFileEntry {
                relative_path,
                absolute_path,
                file_modified_at: modified,
            });
        }

        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(entries)
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
            if let Some(existing_index) = ids.get(&note_id) {
                // A single odd file must not take the whole vault down: every
                // operation starts from inventory, so failing here would brick
                // search, indexing, and the MCP server alike. Keep the first
                // entry (paths are sorted, so this is deterministic) and skip
                // the duplicate with a warning.
                let existing = entries
                    .get(*existing_index)
                    .map(|entry: &NoteInventoryEntry| entry.relative_path.display().to_string())
                    .unwrap_or_default();
                warn!(
                    note_id = %note_id,
                    path = %relative_path.display(),
                    kept = %existing,
                    "duplicate note identifier detected during inventory; skipping this file"
                );
                continue;
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

        debug!(count = entries.len(), "completed vault inventory build");
        Ok(entries)
    }

    /// Load a note using a precomputed inventory entry.
    pub fn load_note_from_entry(&self, entry: &NoteInventoryEntry) -> Result<NoteRecord> {
        let raw = fs::read_to_string(&entry.absolute_path)
            .with_context(|| format!("failed to read note {}", entry.absolute_path.display()))?;

        let (frontmatter_str, body) = split_frontmatter(&raw);
        let (metadata, body) = match parse_frontmatter(frontmatter_str) {
            Ok(metadata) => (metadata, body),
            Err(err) => {
                warn!(
                    note = %entry.relative_path.display(),
                    error = ?err,
                    "invalid frontmatter detected; treating note as plain text"
                );
                (MetadataMap::default(), raw.as_str())
            }
        };

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
    use crate::metrics::{MetricsConfigFile, MetricsConventionsSource};
    use crate::workspace::{WORKSPACE_CONFIG_FILE, WorkspaceFile, write_workspace_file};
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
    fn load_note_with_invalid_frontmatter_falls_back_to_plain_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let note_path = dir.path().join("Broken.md");
        fs::write(
            &note_path,
            "---\nrelated: [[A]], [[B]]\n---\n\n# Title\nBody\n",
        )
        .expect("write note");

        let vault =
            Vault::new(VaultConfig::new(dir.path().to_path_buf())).expect("vault initialises");
        let note = vault.load_note("Broken").expect("load note");

        assert!(note.metadata.is_empty());
        assert!(note.content.contains("related: [[A]], [[B]]"));
        assert!(note.content.contains("# Title"));
    }

    #[test]
    fn note_ids_with_dots_round_trip_to_the_same_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let note_path = dir.path().join("Meeting 2024.01.md");
        fs::write(&note_path, "---\ntitle: Dotted\n---\n\nBody\n").expect("write note");

        let vault =
            Vault::new(VaultConfig::new(dir.path().to_path_buf())).expect("vault initialises");

        // The id derived during inventory must resolve back to the same file.
        let inventory = vault.inventory().expect("inventory builds");
        let entry = inventory
            .iter()
            .find(|entry| entry.relative_path == PathBuf::from("Meeting 2024.01.md"))
            .expect("dotted note discovered");
        assert_eq!(entry.id, "Meeting 2024.01");

        let resolved = vault.note_file_path(&entry.id).expect("path resolves");
        assert_eq!(resolved, fs::canonicalize(&note_path).expect("canonical"));

        // Writing through the id must update the original file, not create a sibling.
        vault
            .write_note(&entry.id, &MetadataMap::default(), "Updated body\n")
            .expect("write note");
        assert!(
            !dir.path().join("Meeting 2024.md").exists(),
            "write must not target a truncated path"
        );
        let updated = fs::read_to_string(&note_path).expect("read updated note");
        assert!(updated.contains("Updated body"));
    }

    #[test]
    fn inventory_skips_duplicate_note_ids_instead_of_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Both file names derive the id "Note" (derive_note_id trims the
        // surrounding whitespace), which previously aborted the entire
        // inventory build and with it every vault operation.
        fs::write(dir.path().join("Note.md"), "# First").expect("write note");
        fs::write(dir.path().join(" Note.md"), "# Duplicate").expect("write duplicate");
        fs::write(dir.path().join("Other.md"), "# Other").expect("write other");

        let vault =
            Vault::new(VaultConfig::new(dir.path().to_path_buf())).expect("vault initialises");
        let inventory = vault
            .inventory()
            .expect("inventory must tolerate duplicate note ids");

        let matching: Vec<_> = inventory
            .iter()
            .filter(|entry| entry.id == "Note")
            .collect();
        assert_eq!(matching.len(), 1, "exactly one entry per note id");
        // Markdown files are collected in sorted order, so the first path
        // wins deterministically (" Note.md" sorts before "Note.md").
        assert_eq!(matching[0].relative_path, PathBuf::from(" Note.md"));

        // The rest of the vault keeps working.
        let ids = vault.list_note_ids().expect("listing succeeds");
        assert!(ids.contains(&"Other".to_string()));
        assert_eq!(ids.iter().filter(|id| *id == "Note").count(), 1);
        let note = vault.load_note("Other").expect("unaffected notes load");
        assert!(note.content.contains("# Other"));
    }

    #[test]
    fn append_md_extension_handles_dotted_and_plain_names() {
        assert_eq!(
            append_md_extension(PathBuf::from("Meeting 2024.01")),
            PathBuf::from("Meeting 2024.01.md")
        );
        assert_eq!(
            append_md_extension(PathBuf::from("Plain Note")),
            PathBuf::from("Plain Note.md")
        );
        assert_eq!(
            append_md_extension(PathBuf::from("Nested/Notes v1.2")),
            PathBuf::from("Nested/Notes v1.2.md")
        );
        // Paths that already carry the extension are left untouched.
        assert_eq!(
            append_md_extension(PathBuf::from("Already.md")),
            PathBuf::from("Already.md")
        );
        assert_eq!(
            append_md_extension(PathBuf::from("Upper.MD")),
            PathBuf::from("Upper.MD")
        );
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

    #[test]
    fn generic_workspace_uses_arrowhead_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join(".arrowhead")).expect("arrowhead dir");
        fs::write(dir.path().join("Note.md"), "# Note").expect("write note");

        let file = WorkspaceFile {
            attachments_dir: Some("Assets".to_string()),
            ignored_folders: vec!["Private".to_string()],
            daily_note_format: Some("YYYY-MM-DD".to_string()),
            link_style: Some("absolute".to_string()),
            metrics: None,
        };
        let workspace_path = dir.path().join(".arrowhead").join(WORKSPACE_CONFIG_FILE);
        write_workspace_file(&workspace_path, &file).expect("write workspace file");

        let vault =
            Vault::new(VaultConfig::new(dir.path().to_path_buf())).expect("vault initialises");
        assert_eq!(vault.workspace_kind(), WorkspaceKind::Generic);
        assert_eq!(
            vault.settings().attachments_folder(),
            Some(Path::new("Assets"))
        );
        assert_eq!(
            vault.settings().ignored_folders(),
            &[PathBuf::from("Private")]
        );
        assert_eq!(vault.settings().daily_note_format(), Some("YYYY-MM-DD"));
        assert_eq!(vault.settings().link_style(), Some("absolute"));
    }

    #[test]
    fn obsidian_settings_take_precedence_over_arrowhead_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let obsidian_dir = dir.path().join(".obsidian");
        fs::create_dir_all(&obsidian_dir).expect("obsidian dir");
        fs::write(
            obsidian_dir.join("app.json"),
            r#"{
  "attachmentFolderPath": "Attachments",
  "userIgnoreFilters": ["Templates/"]
}"#,
        )
        .expect("write app.json");

        fs::create_dir_all(dir.path().join(".arrowhead")).expect("arrowhead dir");
        let file = WorkspaceFile {
            attachments_dir: Some("Assets".to_string()),
            ..WorkspaceFile::default()
        };
        write_workspace_file(
            &dir.path().join(".arrowhead").join(WORKSPACE_CONFIG_FILE),
            &file,
        )
        .expect("write workspace file");

        fs::write(dir.path().join("Note.md"), "# Note").expect("write note");

        let vault =
            Vault::new(VaultConfig::new(dir.path().to_path_buf())).expect("vault initialises");
        assert_eq!(vault.workspace_kind(), WorkspaceKind::Obsidian);
        assert_eq!(
            vault.settings().attachments_folder(),
            Some(Path::new("Attachments"))
        );
        assert_eq!(
            vault.settings().ignored_folders(),
            &[PathBuf::from("Templates")]
        );
    }

    #[test]
    fn obsidian_daily_note_format_is_detected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let obsidian_dir = dir.path().join(".obsidian");
        fs::create_dir_all(&obsidian_dir).expect("obsidian dir");
        fs::write(
            obsidian_dir.join("daily-notes.json"),
            r#"{"format": "YYYY/[week]WW"}"#,
        )
        .expect("write daily notes config");
        fs::write(dir.path().join("Note.md"), "# Note").expect("write note");

        let vault =
            Vault::new(VaultConfig::new(dir.path().to_path_buf())).expect("vault initialises");
        assert_eq!(vault.settings().daily_note_format(), Some("YYYY/[week]WW"));
    }

    #[test]
    fn metrics_conventions_default_when_no_config_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("Note.md"), "# Note").expect("write note");

        let vault =
            Vault::new(VaultConfig::new(dir.path().to_path_buf())).expect("vault initialises");
        let metrics = vault.metrics_conventions();

        assert_eq!(metrics.source, MetricsConventionsSource::Default);
        assert_eq!(metrics.root, PathBuf::from("Metrics"));
        assert_eq!(metrics.extensions, vec![".metrics.ndjson".to_string()]);
        assert_eq!(
            metrics.default_write_file,
            PathBuf::from("Metrics/All.metrics.ndjson")
        );
        assert_eq!(metrics.record_reference_prefix, "metric:");
        assert_eq!(metrics.week_start_day, "monday");
        assert_eq!(metrics.day_start_hour, 0);
    }

    #[test]
    fn workspace_metrics_conventions_are_loaded_for_generic_workspaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join(".arrowhead")).expect("arrowhead dir");
        fs::write(dir.path().join("Note.md"), "# Note").expect("write note");

        let file = WorkspaceFile {
            metrics: Some(MetricsConfigFile {
                root: Some("Health".to_string()),
                extensions: vec!["health.ndjson".to_string()],
                default_write_file: Some("Health/Daily.health.ndjson".to_string()),
                record_reference_prefix: Some("health:".to_string()),
                week_start_day: Some("Sunday".to_string()),
                day_start_hour: Some(4),
            }),
            ..WorkspaceFile::default()
        };
        let workspace_path = dir.path().join(".arrowhead").join(WORKSPACE_CONFIG_FILE);
        write_workspace_file(&workspace_path, &file).expect("write workspace file");

        let vault =
            Vault::new(VaultConfig::new(dir.path().to_path_buf())).expect("vault initialises");
        let metrics = vault.metrics_conventions();

        assert_eq!(
            metrics.source,
            MetricsConventionsSource::ArrowheadWorkspace(workspace_path)
        );
        assert_eq!(metrics.root, PathBuf::from("Health"));
        assert_eq!(metrics.extensions, vec![".health.ndjson".to_string()]);
        assert_eq!(
            metrics.default_write_file,
            PathBuf::from("Health/Daily.health.ndjson")
        );
        assert_eq!(metrics.record_reference_prefix, "health:");
        assert_eq!(metrics.week_start_day, "sunday");
        assert_eq!(metrics.day_start_hour, 4);
    }

    #[test]
    fn obsidian_metrics_plugin_takes_precedence_over_workspace_metrics() {
        let dir = tempfile::tempdir().expect("tempdir");
        let obsidian_dir = dir.path().join(".obsidian");
        let plugin_dir = obsidian_dir.join("plugins").join("metrics-lens");
        fs::create_dir_all(&plugin_dir).expect("plugin dir");
        fs::write(
            plugin_dir.join("data.json"),
            r#"{
  "metricsRoot": "PluginMetrics",
  "supportedExtensions": [".plugin.ndjson"],
  "defaultWriteFile": "PluginMetrics/Inbox.plugin.ndjson",
  "recordReferencePrefix": "plugin-metric:",
  "weekStartsOn": 2,
  "dayStartHour": 6
}"#,
        )
        .expect("write plugin config");

        fs::create_dir_all(dir.path().join(".arrowhead")).expect("arrowhead dir");
        write_workspace_file(
            &dir.path().join(".arrowhead").join(WORKSPACE_CONFIG_FILE),
            &WorkspaceFile {
                metrics: Some(MetricsConfigFile {
                    root: Some("WorkspaceMetrics".to_string()),
                    ..MetricsConfigFile::default()
                }),
                ..WorkspaceFile::default()
            },
        )
        .expect("write workspace file");

        fs::write(dir.path().join("Note.md"), "# Note").expect("write note");

        let vault =
            Vault::new(VaultConfig::new(dir.path().to_path_buf())).expect("vault initialises");
        let metrics = vault.metrics_conventions();
        let plugin_data_path =
            fs::canonicalize(plugin_dir.join("data.json")).expect("canonicalise plugin path");

        assert_eq!(
            metrics.source,
            MetricsConventionsSource::ObsidianPlugin(plugin_data_path)
        );
        assert_eq!(metrics.root, PathBuf::from("PluginMetrics"));
        assert_eq!(metrics.extensions, vec![".plugin.ndjson".to_string()]);
        assert_eq!(
            metrics.default_write_file,
            PathBuf::from("PluginMetrics/Inbox.plugin.ndjson")
        );
        assert_eq!(metrics.record_reference_prefix, "plugin-metric:");
        assert_eq!(metrics.week_start_day, "tuesday");
        assert_eq!(metrics.day_start_hour, 6);
    }

    #[test]
    fn metrics_file_discovery_uses_configured_root_and_suffixes() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join(".arrowhead")).expect("arrowhead dir");
        fs::create_dir_all(dir.path().join("Health").join("nested")).expect("metrics dir");
        fs::write(
            dir.path().join(".arrowhead").join(WORKSPACE_CONFIG_FILE),
            toml::to_string_pretty(&WorkspaceFile {
                metrics: Some(MetricsConfigFile {
                    root: Some("Health".to_string()),
                    extensions: vec![".health.ndjson".to_string()],
                    ..MetricsConfigFile::default()
                }),
                ..WorkspaceFile::default()
            })
            .expect("serialise workspace"),
        )
        .expect("write workspace file");
        fs::write(
            dir.path().join("Health").join("daily.health.ndjson"),
            "{}\n",
        )
        .expect("write metrics file");
        fs::write(
            dir.path()
                .join("Health")
                .join("nested")
                .join("other.health.ndjson"),
            "{}\n",
        )
        .expect("write nested metrics file");
        fs::write(dir.path().join("Health").join("ignore.txt"), "nope")
            .expect("write unrelated file");
        fs::write(dir.path().join("Note.md"), "# Note").expect("write note");

        let vault =
            Vault::new(VaultConfig::new(dir.path().to_path_buf())).expect("vault initialises");
        let files = vault.metrics_files().expect("metrics discovery succeeds");

        let paths = files
            .iter()
            .map(|entry| entry.relative_path.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("Health/daily.health.ndjson"),
                PathBuf::from("Health/nested/other.health.ndjson"),
            ]
        );
    }

    #[test]
    fn metrics_entry_for_path_resolves_existing_and_missing_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join(".arrowhead")).expect("arrowhead dir");
        fs::create_dir_all(dir.path().join("Health")).expect("metrics dir");
        fs::write(
            dir.path().join(".arrowhead").join(WORKSPACE_CONFIG_FILE),
            toml::to_string_pretty(&WorkspaceFile {
                metrics: Some(MetricsConfigFile {
                    root: Some("Health".to_string()),
                    extensions: vec![".health.ndjson".to_string()],
                    ..MetricsConfigFile::default()
                }),
                ..WorkspaceFile::default()
            })
            .expect("serialise workspace"),
        )
        .expect("write workspace file");
        fs::write(
            dir.path().join("Health").join("daily.health.ndjson"),
            "{}\n",
        )
        .expect("write metrics file");
        fs::write(dir.path().join("Note.md"), "# Note").expect("write note");

        let vault =
            Vault::new(VaultConfig::new(dir.path().to_path_buf())).expect("vault initialises");
        let existing = vault.note_path("Health/daily.health.ndjson");
        let entry = vault
            .metrics_entry_for_path(&existing)
            .expect("metrics entry lookup")
            .expect("metrics entry present");
        assert_eq!(
            entry.relative_path,
            PathBuf::from("Health/daily.health.ndjson")
        );

        let missing = vault.note_path("Health/missing.health.ndjson");
        assert_eq!(
            vault.resolve_relative_metrics_path(&missing),
            Some(PathBuf::from("Health/missing.health.ndjson"))
        );
        assert!(
            vault
                .metrics_entry_for_path(&missing)
                .expect("missing metrics entry lookup")
                .is_none()
        );
        assert!(
            vault
                .resolve_relative_metrics_path(vault.note_path("Note.md"))
                .is_none()
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

fn collect_metrics_files(root: &Path, suffixes: &[String]) -> Result<Vec<PathBuf>> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();

    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed to read directory {}", dir.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();

            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && is_metrics_file(&path, suffixes) {
                files.push(path);
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

fn is_metrics_file(path: &Path, suffixes: &[String]) -> bool {
    let name = match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => name.to_ascii_lowercase(),
        None => return false,
    };
    suffixes.iter().any(|suffix| name.ends_with(suffix))
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

/// Append the `.md` extension to a note-id-derived relative path.
///
/// Note ids may contain dots (`Meeting 2024.01`), so `Path::set_extension`
/// must not be used here: it would replace everything after the last dot and
/// point at a different file. This is the inverse of [`derive_note_id`].
fn append_md_extension(mut relative: PathBuf) -> PathBuf {
    let already_md = relative
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
    if already_md {
        return relative;
    }

    let mut file_name = relative.file_name().map(OsString::from).unwrap_or_default();
    file_name.push(".md");
    relative.set_file_name(file_name);
    relative
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
    #[serde(rename = "newLinkFormat")]
    new_link_format: Option<String>,
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

                if let Some(link_style) = app.new_link_format {
                    if let Some(normalised) = normalise_string_field(&link_style) {
                        settings.link_style = Some(normalised);
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

    if let Some(format) = load_obsidian_daily_note_format(obsidian_dir) {
        settings.daily_note_format = Some(format);
    }

    settings
}

fn settings_from_workspace_file(file: WorkspaceFile) -> VaultSettings {
    let mut settings = VaultSettings::default();

    if let Some(folder) = file.attachments_dir {
        if let Some(relative) = normalise_relative_str(&folder) {
            settings.attachments_folder = Some(relative);
        }
    }

    let mut ignored = Vec::new();
    for filter in file.ignored_folders {
        if let Some(relative) = normalise_relative_str(&filter) {
            ignored.push(relative);
        }
    }
    ignored.sort();
    ignored.dedup();
    settings.ignored_folders = ignored;

    if let Some(format) = file.daily_note_format {
        settings.daily_note_format = normalise_string_field(&format);
    }

    if let Some(link_style) = file.link_style {
        settings.link_style = normalise_string_field(&link_style);
    }

    settings
}

fn resolve_metrics_conventions(
    obsidian_dir: &Path,
    workspace_file_path: &Path,
    workspace_metrics: Option<MetricsConfigFile>,
) -> MetricsConventions {
    let plugin_path = obsidian_dir
        .join("plugins")
        .join("metrics-lens")
        .join("data.json");
    if let Some(config) = load_obsidian_metrics_config(&plugin_path) {
        if workspace_metrics.is_some() {
            warn!(
                plugin = %plugin_path.display(),
                workspace = %workspace_file_path.display(),
                "found metrics-lens plugin config and Arrowhead metrics config; preferring plugin conventions"
            );
        }
        return build_metrics_conventions(
            config,
            MetricsConventionsSource::ObsidianPlugin(plugin_path),
        );
    }

    if let Some(config) = workspace_metrics {
        return build_metrics_conventions(
            config,
            MetricsConventionsSource::ArrowheadWorkspace(workspace_file_path.to_path_buf()),
        );
    }

    build_metrics_conventions(
        MetricsConfigFile::default(),
        MetricsConventionsSource::Default,
    )
}

fn load_obsidian_metrics_config(path: &Path) -> Option<MetricsConfigFile> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    plugin = %path.display(),
                    "failed to read metrics-lens settings: {err}"
                );
            }
            return None;
        }
    };

    let value = match serde_json::from_str::<Value>(&content) {
        Ok(value) => value,
        Err(err) => {
            warn!(
                plugin = %path.display(),
                "failed to parse metrics-lens settings: {err}"
            );
            return None;
        }
    };

    Some(MetricsConfigFile {
        root: find_json_string(
            &value,
            &[
                "metricsRoot",
                "metricsFolder",
                "metricsDir",
                "metrics.root",
                "storage.metricsRoot",
            ],
        ),
        extensions: find_json_string_list(
            &value,
            &["extensions", "supportedExtensions", "metrics.extensions"],
        ),
        default_write_file: find_json_string(
            &value,
            &[
                "defaultWriteFile",
                "defaultFile",
                "metrics.defaultWriteFile",
                "storage.defaultWriteFile",
            ],
        ),
        record_reference_prefix: find_json_string(
            &value,
            &[
                "recordReferencePrefix",
                "referencePrefix",
                "metrics.recordReferencePrefix",
            ],
        ),
        week_start_day: find_json_week_start_day(
            &value,
            &[
                "weekStartDay",
                "weekStart",
                "calendar.weekStartDay",
                "weekStartsOn",
                "calendar.weekStartsOn",
            ],
        ),
        day_start_hour: find_json_u8(
            &value,
            &["dayStartHour", "dayBoundaryHour", "calendar.dayStartHour"],
        ),
    })
}

fn build_metrics_conventions(
    config: MetricsConfigFile,
    source: MetricsConventionsSource,
) -> MetricsConventions {
    let root = config
        .root
        .as_deref()
        .and_then(normalise_relative_str)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_METRICS_ROOT));
    let extensions = normalise_metrics_extensions(&config.extensions);
    let default_write_file = config
        .default_write_file
        .as_deref()
        .and_then(normalise_relative_str)
        .unwrap_or_else(|| root.join(DEFAULT_METRICS_WRITE_FILE_NAME));
    let record_reference_prefix = config
        .record_reference_prefix
        .as_deref()
        .and_then(normalise_string_field)
        .unwrap_or_else(|| DEFAULT_METRIC_REFERENCE_PREFIX.to_string());
    let week_start_day = config
        .week_start_day
        .as_deref()
        .and_then(normalise_string_field)
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| DEFAULT_WEEK_START_DAY.to_string());
    let day_start_hour = config
        .day_start_hour
        .filter(|hour| *hour <= 23)
        .unwrap_or(DEFAULT_DAY_START_HOUR);

    MetricsConventions {
        source,
        root,
        extensions,
        default_write_file,
        record_reference_prefix,
        week_start_day,
        day_start_hour,
    }
}

fn normalise_metrics_extensions(extensions: &[String]) -> Vec<String> {
    let mut normalised = Vec::new();
    for extension in extensions {
        let trimmed = extension.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = if trimmed.starts_with('.') {
            trimmed.to_ascii_lowercase()
        } else {
            format!(".{}", trimmed.to_ascii_lowercase())
        };
        if !normalised.iter().any(|existing| existing == &value) {
            normalised.push(value);
        }
    }

    if normalised.is_empty() {
        normalised.push(DEFAULT_METRICS_EXTENSION.to_string());
    }

    normalised
}

fn find_json_string(value: &Value, paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|path| {
        lookup_json_path(value, path).and_then(|candidate| match candidate {
            Value::String(value) => normalise_string_field(value),
            _ => None,
        })
    })
}

fn find_json_string_list(value: &Value, paths: &[&str]) -> Vec<String> {
    for path in paths {
        if let Some(candidate) = lookup_json_path(value, path) {
            match candidate {
                Value::Array(items) => {
                    let collected = items
                        .iter()
                        .filter_map(|item| item.as_str())
                        .map(str::to_string)
                        .collect::<Vec<_>>();
                    if !collected.is_empty() {
                        return collected;
                    }
                }
                Value::String(single) => {
                    let trimmed = single.trim();
                    if !trimmed.is_empty() {
                        return vec![trimmed.to_string()];
                    }
                }
                _ => {}
            }
        }
    }

    Vec::new()
}

fn find_json_u8(value: &Value, paths: &[&str]) -> Option<u8> {
    paths.iter().find_map(|path| {
        lookup_json_path(value, path).and_then(|candidate| match candidate {
            Value::Number(number) => number.as_u64().and_then(|value| u8::try_from(value).ok()),
            _ => None,
        })
    })
}

fn find_json_week_start_day(value: &Value, paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|path| {
        lookup_json_path(value, path).and_then(|candidate| match candidate {
            Value::String(value) => normalise_string_field(value),
            Value::Number(number) => number.as_u64().and_then(|value| match value {
                0 => Some("sunday".to_string()),
                1 => Some("monday".to_string()),
                2 => Some("tuesday".to_string()),
                3 => Some("wednesday".to_string()),
                4 => Some("thursday".to_string()),
                5 => Some("friday".to_string()),
                6 => Some("saturday".to_string()),
                _ => None,
            }),
            _ => None,
        })
    })
}

fn lookup_json_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

fn load_obsidian_daily_note_format(obsidian_dir: &Path) -> Option<String> {
    let daily_notes_path = obsidian_dir.join("daily-notes.json");
    match fs::read_to_string(&daily_notes_path) {
        Ok(content) => match serde_json::from_str::<ObsidianDailyNotesConfig>(&content) {
            Ok(config) => config
                .format
                .and_then(|value| normalise_string_field(&value)),
            Err(err) => {
                warn!(
                    path = %daily_notes_path.display(),
                    error = %err,
                    "failed to parse Obsidian daily-notes.json"
                );
                None
            }
        },
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    path = %daily_notes_path.display(),
                    error = %err,
                    "failed to read Obsidian daily note settings"
                );
            }
            None
        }
    }
}

fn normalise_string_field(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Debug, Deserialize)]
struct ObsidianDailyNotesConfig {
    #[serde(default)]
    format: Option<String>,
}

/// Normalise a relative path string by trimming whitespace and removing redundant separators.
pub fn normalise_relative_str(value: &str) -> Option<PathBuf> {
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

/// Normalise a relative [`Path`] by removing prefixes, `.` segments, and `..` escapes.
pub fn normalise_relative_path(path: &Path) -> Option<PathBuf> {
    let mut cleaned = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => return None,
            Component::CurDir => continue,
            Component::ParentDir => return None,
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
