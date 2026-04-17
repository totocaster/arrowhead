//! `arrowhead notes` command family.

use std::{collections::BTreeSet, fs, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Duration, NaiveDate, Utc};
use clap::{Args, Subcommand};
use serde_json::{Value as JsonValue, json};
use tracing::info;

use super::{
    CommandContext,
    context::{SemanticContextMode, build_context_service, render_context_payload},
};
use crate::logging;
use arrowhead_core::{
    DEFAULT_CONTEXT_METRIC_LIMIT, MetadataMap, Vault, VaultConfig,
    query::{
        DateRange, DateRangeBound, parse_absolute_date, parse_month_date_lower_bound,
        parse_month_date_range, parse_month_date_upper_bound, parse_relative_range,
        range_from_lower, range_from_parsed_date, range_from_upper,
    },
    sqlite::IndexDatabase,
};

/// CRUD operations for notes.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct NotesCommand {
    /// Select which note operation to perform.
    #[command(subcommand)]
    pub action: NoteAction,
}

/// Available note subcommands.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum NoteAction {
    /// Read a note by ID.
    Read(ReadArgs),
    /// List notes in the vault.
    List(ListArgs),
    /// Create a new note.
    Create(CreateArgs),
    /// Update an existing note.
    Update(UpdateArgs),
    /// Delete a note.
    Delete(DeleteArgs),
    /// Compatibility alias for `context note`.
    #[command(visible_alias = "surprise")]
    Similar(SimilarArgs),
}

/// Arguments for reading a note.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct ReadArgs {
    /// Identifier of the note to read.
    pub note_id: String,
}

/// Arguments for listing notes.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct ListArgs {
    /// Only return note identifiers.
    #[arg(long)]
    pub ids_only: bool,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
    /// Filter notes by the `category` metadata field.
    #[arg(long)]
    pub category: Option<String>,
    /// Filter notes by the `status` metadata field.
    #[arg(long)]
    pub status: Option<String>,
    /// Filter notes whose frontmatter date or YYYY-MM-DD id prefix falls inside this range.
    #[arg(long, value_name = "RANGE")]
    pub date_range: Option<String>,
    /// Maximum number of notes to return.
    #[arg(long)]
    pub limit: Option<usize>,
}

/// Arguments for creating a note.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct CreateArgs {
    /// Explicit note identifier. If omitted one will be generated.
    #[arg(long)]
    pub id: Option<String>,
    /// Title for the note.
    #[arg(long)]
    pub title: Option<String>,
    /// Category metadata field.
    #[arg(long)]
    pub category: Option<String>,
    /// Inline content for the note.
    #[arg(long)]
    pub content: Option<String>,
    /// Read content from a file.
    #[arg(long, value_name = "PATH")]
    pub file: Option<String>,
    /// Additional metadata encoded as JSON.
    #[arg(long, value_name = "JSON")]
    pub metadata: Option<String>,
}

/// Arguments for updating a note.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct UpdateArgs {
    /// Identifier of the note to update.
    pub note_id: String,
    /// Replacement content for the note.
    #[arg(long)]
    pub content: Option<String>,
    /// Provide content via file path.
    #[arg(long, value_name = "PATH")]
    pub file: Option<String>,
    /// Update the note title.
    #[arg(long)]
    pub title: Option<String>,
    /// Merge metadata provided as JSON.
    #[arg(long, value_name = "JSON")]
    pub metadata: Option<String>,
}

/// Arguments for deleting a note.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct DeleteArgs {
    /// Identifier of the note to delete.
    pub note_id: String,
    /// Skip confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for the `notes similar`/`notes surprise` compatibility alias.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct SimilarArgs {
    /// Identifier of the anchor note.
    pub note_id: String,
    /// Maximum number of related notes to surface inside the context payload.
    #[arg(long, default_value_t = 5)]
    pub limit: usize,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Execute the requested notes command.
pub async fn run(ctx: &CommandContext, command: &NotesCommand) -> Result<()> {
    let vault_path = ctx
        .config
        .vault
        .clone()
        .context("no vault configured. Provide --vault or run `arrowhead init`.")?;

    let vault = Arc::new(Vault::new(VaultConfig::new(vault_path))?);
    vault.ensure_arrowhead_dirs()?;

    let logs_dir = vault.paths().logs_dir();
    let _logging_guard = logging::scoped_file_logging(&logs_dir, ctx.verbosity())?;

    match &command.action {
        NoteAction::Read(args) => {
            info!(note_id = %args.note_id, "reading note contents");
            let content = read_note_raw(vault.as_ref(), &args.note_id)?;
            print!("{content}");
            Ok(())
        }
        NoteAction::List(args) => {
            info!(
                ids_only = args.ids_only,
                json = args.json,
                category = ?args.category,
                status = ?args.status,
                date_range = ?args.date_range,
                limit = ?args.limit,
                "listing notes"
            );
            let items = collect_note_list(vault.as_ref(), args)?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&note_list_json_payload(items))?
                );
            } else {
                for (id, title) in items {
                    if let Some(title) = title {
                        println!("{id}\t{title}");
                    } else {
                        println!("{id}");
                    }
                }
            }
            Ok(())
        }
        NoteAction::Create(args) => {
            info!(id = ?args.id, "creating note");
            create_note(vault.as_ref(), args)?;
            println!(
                "Created note {}",
                args.id
                    .clone()
                    .or_else(|| args.title.clone())
                    .unwrap_or_else(|| "(generated)".to_string())
            );
            Ok(())
        }
        NoteAction::Update(args) => {
            info!(note_id = %args.note_id, "updating note");
            update_note(vault.as_ref(), args)?;
            println!("Updated note {}", args.note_id);
            Ok(())
        }
        NoteAction::Delete(args) => {
            info!(note_id = %args.note_id, "deleting note");
            delete_note(vault.as_ref(), args)?;
            println!("Deleted note {}", args.note_id);
            Ok(())
        }
        NoteAction::Similar(args) => {
            info!(
                note_id = %args.note_id,
                limit = args.limit,
                json = args.json,
                "loading note context via compatibility alias"
            );
            run_similar_alias(ctx, Arc::clone(&vault), args).await
        }
    }
}

fn read_note_raw(vault: &Vault, note_id: &str) -> Result<String> {
    let note = vault
        .load_note(note_id)
        .with_context(|| format!("note {note_id} not found"))?;
    let path = vault.note_path(&note.relative_path);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read note file {}", path.display()))?;
    Ok(content)
}

fn collect_note_list(vault: &Vault, args: &ListArgs) -> Result<Vec<(String, Option<String>)>> {
    let filters = NoteListFilters::from_args(args)?;
    let mut results = Vec::new();
    for note_id in vault.list_note_ids()? {
        if args.ids_only && !filters.has_metadata_filters() {
            results.push((note_id, None));
        } else {
            let note = vault.load_note(&note_id)?;
            if !filters.matches(&note) {
                continue;
            }
            if args.ids_only {
                results.push((note_id, None));
            } else {
                results.push((note_id, note.title.clone()));
            }
        }
        if let Some(limit) = filters.limit {
            if results.len() >= limit {
                break;
            }
        }
    }
    Ok(results)
}

fn note_list_json_payload(items: Vec<(String, Option<String>)>) -> JsonValue {
    json!({
        "notes": items
            .into_iter()
            .map(|(id, title)| json!({ "id": id, "title": title }))
            .collect::<Vec<_>>()
    })
}

#[derive(Debug, Clone, PartialEq)]
struct NoteListFilters {
    category: Option<String>,
    status: Option<String>,
    date_range: Option<DateRange>,
    limit: Option<usize>,
}

impl NoteListFilters {
    fn from_args(args: &ListArgs) -> Result<Self> {
        if matches!(args.limit, Some(0)) {
            bail!("`--limit` must be at least 1");
        }

        Ok(Self {
            category: args.category.as_deref().map(normalise_filter_value),
            status: args.status.as_deref().map(normalise_filter_value),
            date_range: args
                .date_range
                .as_deref()
                .map(parse_note_date_filter)
                .transpose()?,
            limit: args.limit,
        })
    }

    fn has_metadata_filters(&self) -> bool {
        self.category.is_some() || self.status.is_some() || self.date_range.is_some()
    }

    fn matches(&self, note: &arrowhead_core::NoteRecord) -> bool {
        if let Some(category) = self.category.as_deref() {
            if !metadata_string_matches(note, "category", category) {
                return false;
            }
        }

        if let Some(status) = self.status.as_deref() {
            if !metadata_string_matches(note, "status", status) {
                return false;
            }
        }

        if let Some(range) = self.date_range.as_ref() {
            let note_dates = extract_note_dates(note);
            if note_dates.is_empty()
                || !note_dates
                    .iter()
                    .any(|date| date_range_contains(range, *date))
            {
                return false;
            }
        }

        true
    }
}

fn normalise_filter_value(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn metadata_string_matches(note: &arrowhead_core::NoteRecord, key: &str, expected: &str) -> bool {
    note.metadata
        .get(key)
        .and_then(|value| value.as_str())
        .map(normalise_filter_value)
        .is_some_and(|value| value == expected)
}

fn extract_note_dates(note: &arrowhead_core::NoteRecord) -> BTreeSet<NaiveDate> {
    let mut dates = BTreeSet::new();
    if let Some(prefix) = note.id.get(..10) {
        if let Ok(date) = NaiveDate::parse_from_str(prefix, "%Y-%m-%d") {
            dates.insert(date);
        }
    }
    if let Some(value) = note.metadata.get("date").and_then(|value| value.as_str()) {
        if let Ok(date) = NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d") {
            dates.insert(date);
        }
    }
    dates
}

fn parse_note_date_filter(input: &str) -> Result<DateRange> {
    let trimmed = input.trim();
    if let Some(range) = parse_relative_range(trimmed, Utc::now())? {
        return Ok(range);
    }

    if let Some(range) = parse_month_date_range(trimmed)? {
        return Ok(range);
    }

    if let Some((lower, upper)) = trimmed.split_once("..") {
        let lower = lower.trim();
        let upper = upper.trim();

        if lower.is_empty() && upper.is_empty() {
            bail!("date range `{trimmed}` must include at least one bound");
        }
        if lower.is_empty() {
            if let Some(bound) = parse_month_date_upper_bound(upper)? {
                return Ok(range_from_upper(bound));
            }

            let parsed = parse_absolute_date(upper)
                .with_context(|| format!("invalid date range `{trimmed}`"))?;
            return Ok(range_from_upper(DateRangeBound {
                value: parsed.instant,
                inclusive: true,
            }));
        }
        if upper.is_empty() {
            if let Some(bound) = parse_month_date_lower_bound(lower)? {
                return Ok(range_from_lower(bound));
            }

            let parsed = parse_absolute_date(lower)
                .with_context(|| format!("invalid date range `{trimmed}`"))?;
            return Ok(range_from_lower(DateRangeBound {
                value: parsed.instant,
                inclusive: true,
            }));
        }

        let lower_bound = if let Some(bound) = parse_month_date_lower_bound(lower)? {
            bound
        } else {
            DateRangeBound {
                value: parse_absolute_date(lower)
                    .with_context(|| format!("invalid date range `{trimmed}`"))?
                    .instant,
                inclusive: true,
            }
        };
        let upper_bound = if let Some(bound) = parse_month_date_upper_bound(upper)? {
            bound
        } else {
            DateRangeBound {
                value: parse_absolute_date(upper)
                    .with_context(|| format!("invalid date range `{trimmed}`"))?
                    .instant,
                inclusive: true,
            }
        };
        let lower_range = range_from_lower(lower_bound);
        let upper_range = range_from_upper(upper_bound);
        return lower_range
            .intersect(&upper_range)
            .with_context(|| format!("date range `{trimmed}` resolves to an empty range"));
    }

    let parsed =
        parse_absolute_date(trimmed).with_context(|| format!("invalid date range `{trimmed}`"))?;
    Ok(range_from_parsed_date(parsed))
}

fn date_range_contains(range: &DateRange, date: NaiveDate) -> bool {
    let day_start = date
        .and_hms_opt(0, 0, 0)
        .expect("valid start of day")
        .and_utc()
        .timestamp_micros();
    let day_end = (date + Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .expect("valid start of next day")
        .and_utc()
        .timestamp_micros()
        - 1;
    let lower_ok = range
        .lower_bound_micros()
        .is_none_or(|lower| lower <= day_end);
    let upper_ok = range
        .upper_bound_micros()
        .is_none_or(|upper| upper >= day_start);
    lower_ok && upper_ok
}

fn create_note(vault: &Vault, args: &CreateArgs) -> Result<()> {
    let note_id = resolve_note_id(args)?;

    if vault.note_file_path(&note_id)?.exists() {
        bail!("note {note_id} already exists");
    }

    let mut metadata = MetadataMap::default();
    if let Some(title) = &args.title {
        metadata.insert("title".to_string(), JsonValue::String(title.clone()));
    }
    if let Some(category) = &args.category {
        metadata.insert("category".to_string(), JsonValue::String(category.clone()));
    }

    merge_metadata_json(&mut metadata, &args.metadata)?;

    if !metadata.contains_key("title") {
        metadata.insert("title".to_string(), JsonValue::String(note_id.clone()));
    }

    let body = load_content(args.content.as_ref(), args.file.as_ref())?;

    vault.write_note(&note_id, &metadata, &body)?;
    Ok(())
}

fn update_note(vault: &Vault, args: &UpdateArgs) -> Result<()> {
    let note = vault
        .load_note(&args.note_id)
        .with_context(|| format!("note {} not found", args.note_id))?;

    let mut metadata = note.metadata.clone();

    if let Some(title) = &args.title {
        if title.trim().is_empty() {
            bail!(
                "empty note title updates are not allowed; omit `--title` to keep the current title"
            );
        }
        metadata.insert("title".to_string(), JsonValue::String(title.clone()));
    }

    merge_metadata_json(&mut metadata, &args.metadata)?;

    let body = if args.content.is_some() || args.file.is_some() {
        load_update_content(args.content.as_ref(), args.file.as_ref())?
    } else {
        note.content.clone()
    };

    vault.write_note(&args.note_id, &metadata, &body)?;
    Ok(())
}

fn delete_note(vault: &Vault, args: &DeleteArgs) -> Result<()> {
    if !args.yes {
        bail!("use --yes to confirm deletion");
    }

    let path = vault
        .note_file_path(&args.note_id)
        .with_context(|| format!("invalid note id {}", args.note_id))?;

    if !path.exists() {
        bail!("note {} does not exist", args.note_id);
    }

    fs::remove_file(&path)
        .with_context(|| format!("failed to delete note file {}", path.display()))?;

    cleanup_empty_dirs(path.parent(), &vault.paths().root)?;
    Ok(())
}

fn cleanup_empty_dirs(start: Option<&std::path::Path>, root: &std::path::Path) -> Result<()> {
    let mut current = match start {
        Some(path) => path.to_path_buf(),
        None => return Ok(()),
    };

    while current.starts_with(root) && current != root {
        if fs::read_dir(&current)?.next().is_some() {
            break;
        }
        fs::remove_dir(&current)?;
        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        } else {
            break;
        }
    }

    Ok(())
}

fn resolve_note_id(args: &CreateArgs) -> Result<String> {
    if let Some(id) = &args.id {
        return clean_note_id(id);
    }

    if let Some(title) = &args.title {
        return clean_note_id(title);
    }

    Err(anyhow!("note id or title is required"))
}

fn clean_note_id(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("note id must not be empty"));
    }

    let without_ext = trimmed.strip_suffix(".md").unwrap_or(trimmed);
    let candidate = without_ext.trim_matches(|c| c == '/' || c == '\\').trim();
    if candidate.is_empty() {
        return Err(anyhow!("note id must not be empty"));
    }

    Ok(candidate.replace(char::from(b'\\'), "/"))
}

fn load_content(inline: Option<&String>, file: Option<&String>) -> Result<String> {
    if let Some(path) = file {
        return fs::read_to_string(path)
            .with_context(|| format!("failed to read content file {}", path));
    }

    Ok(inline.cloned().unwrap_or_default())
}

fn load_update_content(inline: Option<&String>, file: Option<&String>) -> Result<String> {
    let content = load_content(inline, file)?;
    if content.is_empty() {
        bail!(
            "empty note content updates are not allowed; omit `--content`/`--file` to keep the current body"
        );
    }
    Ok(content)
}

fn merge_metadata_json(metadata: &mut MetadataMap, payload: &Option<String>) -> Result<()> {
    if let Some(raw) = payload {
        if raw.trim().is_empty() {
            return Ok(());
        }

        let value: JsonValue = serde_json::from_str(raw)
            .with_context(|| "metadata must be a JSON object".to_string())?;
        let object = value
            .as_object()
            .ok_or_else(|| anyhow!("metadata must be a JSON object"))?;

        for (key, value) in object {
            if value.is_null() {
                metadata.remove(key);
            } else {
                metadata.insert(key.clone(), value.clone());
            }
        }
    }

    Ok(())
}

async fn run_similar_alias(
    ctx: &CommandContext,
    vault: Arc<Vault>,
    args: &SimilarArgs,
) -> Result<()> {
    let database = Arc::new(IndexDatabase::open(
        vault.paths().arrowhead_dir.join("index.db"),
    )?);
    let service = build_context_service(
        ctx,
        Arc::clone(&vault),
        database,
        SemanticContextMode::Preferred,
    )
    .await?;
    let payload = service
        .note(
            &args.note_id,
            Some(args.limit),
            Some(DEFAULT_CONTEXT_METRIC_LIMIT),
        )
        .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        render_context_payload(&payload);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use tempfile::TempDir;

    fn fixture_vault() -> Vault {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("test-vault");
        Vault::new(VaultConfig::new(root)).expect("fixture vault initialises")
    }

    #[test]
    fn read_note_returns_complete_content() {
        let vault = fixture_vault();
        let content = read_note_raw(&vault, "2024-01-15").expect("read note");
        assert!(content.starts_with("---"));
        assert!(content.contains("# January 15, 2024"));
    }

    #[test]
    fn list_ids_only_returns_identifiers() {
        let vault = fixture_vault();
        let entries = collect_note_list(
            &vault,
            &ListArgs {
                ids_only: true,
                json: false,
                category: None,
                status: None,
                date_range: None,
                limit: None,
            },
        )
        .expect("list notes");
        assert!(entries.iter().all(|(_, title)| title.is_none()));
        assert!(entries.iter().any(|(id, _)| id == "2024-01-15"));
    }

    #[test]
    fn list_with_titles_includes_note_titles() {
        let vault = fixture_vault();
        let entries = collect_note_list(
            &vault,
            &ListArgs {
                ids_only: false,
                json: false,
                category: None,
                status: None,
                date_range: None,
                limit: None,
            },
        )
        .expect("list notes");
        let photography_title = entries
            .iter()
            .find(|(id, _)| id == "Photography Equipment")
            .and_then(|(_, title)| title.clone())
            .expect("title present");
        assert_eq!(photography_title, "Photography Equipment");
    }

    #[test]
    fn list_filters_by_category_status_date_and_limit() {
        let temp_dir = TempDir::new().expect("tempdir");
        let vault = Vault::new(VaultConfig::new(temp_dir.path().to_path_buf())).expect("vault");

        create_note(
            &vault,
            &CreateArgs {
                id: Some("2026-04-14".to_string()),
                title: Some("April 14".to_string()),
                category: Some("project".to_string()),
                content: Some("Daily project note".to_string()),
                file: None,
                metadata: Some(r#"{"status":"active","date":"2026-04-14"}"#.to_string()),
            },
        )
        .expect("create first note");
        create_note(
            &vault,
            &CreateArgs {
                id: Some("2026-04-20".to_string()),
                title: Some("April 20".to_string()),
                category: Some("project".to_string()),
                content: Some("Second project note".to_string()),
                file: None,
                metadata: Some(r#"{"status":"active","date":"2026-04-20"}"#.to_string()),
            },
        )
        .expect("create second note");
        create_note(
            &vault,
            &CreateArgs {
                id: Some("Reference".to_string()),
                title: Some("Reference".to_string()),
                category: Some("reference".to_string()),
                content: Some("Reference note".to_string()),
                file: None,
                metadata: Some(r#"{"status":"done","date":"2026-04-14"}"#.to_string()),
            },
        )
        .expect("create reference note");

        let entries = collect_note_list(
            &vault,
            &ListArgs {
                ids_only: false,
                json: false,
                category: Some("project".to_string()),
                status: Some("active".to_string()),
                date_range: Some("2026-04-10..2026-04-18".to_string()),
                limit: Some(1),
            },
        )
        .expect("list filtered notes");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "2026-04-14");
        assert_eq!(entries[0].1.as_deref(), Some("April 14"));
    }

    #[test]
    fn parse_note_date_filter_supports_month_shorthand() {
        let range = parse_note_date_filter("2026-04").expect("month range");

        assert!(date_range_contains(
            &range,
            NaiveDate::from_ymd_opt(2026, 4, 1).expect("valid date")
        ));
        assert!(date_range_contains(
            &range,
            NaiveDate::from_ymd_opt(2026, 4, 30).expect("valid date")
        ));
        assert!(!date_range_contains(
            &range,
            NaiveDate::from_ymd_opt(2026, 5, 1).expect("valid date")
        ));
    }

    #[test]
    fn note_list_json_payload_includes_ids_and_titles() {
        let payload = note_list_json_payload(vec![
            ("2026-04-14".to_string(), Some("April 14".to_string())),
            ("Reference".to_string(), None),
        ]);

        assert_eq!(
            payload,
            json!({
                "notes": [
                    { "id": "2026-04-14", "title": "April 14" },
                    { "id": "Reference", "title": null }
                ]
            })
        );
    }

    #[test]
    fn create_note_writes_frontmatter() {
        let temp_dir = TempDir::new().expect("tempdir");
        let vault = Vault::new(VaultConfig::new(temp_dir.path().to_path_buf())).expect("vault");

        let args = CreateArgs {
            id: Some("Notes/Test Note".to_string()),
            title: Some("Test Note".to_string()),
            category: Some("testing".to_string()),
            content: Some("Body content".to_string()),
            file: None,
            metadata: Some("{\"status\": \"draft\"}".to_string()),
        };

        create_note(&vault, &args).expect("create");

        let note_path = vault.note_file_path("Notes/Test Note").expect("path");
        let written = fs::read_to_string(note_path).expect("read note");
        assert!(written.starts_with("---\n"));
        assert!(written.contains("title: Test Note"));
        assert!(written.contains("category: testing"));
        assert!(written.contains("status: draft"));
        assert!(written.contains("Body content"));
    }

    #[test]
    fn update_note_merges_metadata() {
        let temp_dir = TempDir::new().expect("tempdir");
        let vault = Vault::new(VaultConfig::new(temp_dir.path().to_path_buf())).expect("vault");

        let create_args = CreateArgs {
            id: Some("Note".to_string()),
            title: Some("Original".to_string()),
            category: Some("initial".to_string()),
            content: Some("Original body".to_string()),
            file: None,
            metadata: Some("{\"tags\": [\"one\"]}".to_string()),
        };
        create_note(&vault, &create_args).expect("create");

        let update_args = UpdateArgs {
            note_id: "Note".to_string(),
            content: Some("Updated body".to_string()),
            file: None,
            title: Some("Updated".to_string()),
            metadata: Some("{\"tags\": [\"two\"], \"status\": \"done\"}".to_string()),
        };

        update_note(&vault, &update_args).expect("update");

        let updated = vault.load_note("Note").expect("load note");
        assert_eq!(
            updated
                .metadata
                .get("title")
                .and_then(|value| value.as_str()),
            Some("Updated")
        );
        let tags = updated
            .metadata
            .get("tags")
            .and_then(|value| value.as_array())
            .expect("tags array");
        assert_eq!(tags, &vec![JsonValue::String("two".to_string())]);
        assert_eq!(
            updated
                .metadata
                .get("status")
                .and_then(|value| value.as_str()),
            Some("done")
        );
        assert!(updated.content.contains("Updated body"));
    }

    #[test]
    fn update_note_rejects_empty_content_replacement() {
        let temp_dir = TempDir::new().expect("tempdir");
        let vault = Vault::new(VaultConfig::new(temp_dir.path().to_path_buf())).expect("vault");

        let create_args = CreateArgs {
            id: Some("Note".to_string()),
            title: Some("Original".to_string()),
            category: None,
            content: Some("Original body".to_string()),
            file: None,
            metadata: None,
        };
        create_note(&vault, &create_args).expect("create");

        let update_args = UpdateArgs {
            note_id: "Note".to_string(),
            content: Some(String::new()),
            file: None,
            title: None,
            metadata: None,
        };

        let err = update_note(&vault, &update_args).expect_err("empty content should fail");
        assert!(
            err.to_string()
                .contains("empty note content updates are not allowed"),
            "unexpected error: {err:#}"
        );

        let updated = vault.load_note("Note").expect("load note");
        assert!(updated.content.contains("Original body"));
    }

    #[test]
    fn update_note_rejects_empty_title_replacement() {
        let temp_dir = TempDir::new().expect("tempdir");
        let vault = Vault::new(VaultConfig::new(temp_dir.path().to_path_buf())).expect("vault");

        let create_args = CreateArgs {
            id: Some("Note".to_string()),
            title: Some("Original".to_string()),
            category: None,
            content: Some("Original body".to_string()),
            file: None,
            metadata: None,
        };
        create_note(&vault, &create_args).expect("create");

        let update_args = UpdateArgs {
            note_id: "Note".to_string(),
            content: None,
            file: None,
            title: Some(String::new()),
            metadata: None,
        };

        let err = update_note(&vault, &update_args).expect_err("empty title should fail");
        assert!(
            err.to_string()
                .contains("empty note title updates are not allowed"),
            "unexpected error: {err:#}"
        );

        let updated = vault.load_note("Note").expect("load note");
        assert_eq!(updated.title.as_deref(), Some("Original"));
        assert!(updated.content.contains("Original body"));
    }

    #[test]
    fn delete_note_removes_file() {
        let temp_dir = TempDir::new().expect("tempdir");
        let vault = Vault::new(VaultConfig::new(temp_dir.path().to_path_buf())).expect("vault");

        let args = CreateArgs {
            id: Some("Disposable".to_string()),
            title: None,
            category: None,
            content: Some("Temporary".to_string()),
            file: None,
            metadata: None,
        };
        create_note(&vault, &args).expect("create");

        let delete_args = DeleteArgs {
            note_id: "Disposable".to_string(),
            yes: true,
        };
        delete_note(&vault, &delete_args).expect("delete");

        assert!(!vault.note_file_path("Disposable").unwrap().exists());
    }
}
