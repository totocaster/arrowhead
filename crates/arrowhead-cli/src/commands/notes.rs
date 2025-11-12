//! `arrowhead notes` command family.

use std::{fs, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use tracing::info;

use super::CommandContext;
use crate::logging;
use arrowhead_core::{
    MetadataMap, NoteRecord, SearchConfig, Vault, VaultConfig,
    embeddings::{EmbeddingMatch, EmbeddingPipeline},
    sqlite::IndexDatabase,
};
use serde_json::{Value as JsonValue, json};

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
    /// Find semantically similar notes.
    #[command(alias = "surprise", visible_alias = "surprise")]
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

/// Arguments for the `notes similar`/`notes surprise` command.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct SimilarArgs {
    /// Identifier of the anchor note.
    pub note_id: String,
    /// Maximum number of related notes to surface.
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

    let vault = Vault::new(VaultConfig::new(vault_path))?;
    vault.ensure_arrowhead_dirs()?;

    let logs_dir = vault.paths().logs_dir();
    let _logging_guard = logging::scoped_file_logging(&logs_dir, ctx.verbosity())?;

    match &command.action {
        NoteAction::Read(args) => {
            info!(note_id = %args.note_id, "reading note contents");
            let content = read_note_raw(&vault, &args.note_id)?;
            print!("{content}");
            Ok(())
        }
        NoteAction::List(args) => {
            info!(ids_only = args.ids_only, "listing notes");
            let items = collect_note_list(&vault, args.ids_only)?;
            for (id, title) in items {
                if let Some(title) = title {
                    println!("{id}\t{title}");
                } else {
                    println!("{id}");
                }
            }
            Ok(())
        }
        NoteAction::Create(args) => {
            info!(id = ?args.id, "creating note");
            create_note(&vault, args)?;
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
            update_note(&vault, args)?;
            println!("Updated note {}", args.note_id);
            Ok(())
        }
        NoteAction::Delete(args) => {
            info!(note_id = %args.note_id, "deleting note");
            delete_note(&vault, args)?;
            println!("Deleted note {}", args.note_id);
            Ok(())
        }
        NoteAction::Similar(args) => {
            info!(
                note_id = %args.note_id,
                limit = args.limit,
                json = args.json,
                "finding similar notes"
            );
            run_similar(ctx, &vault, args).await
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

fn collect_note_list(vault: &Vault, ids_only: bool) -> Result<Vec<(String, Option<String>)>> {
    let mut results = Vec::new();
    for note_id in vault.list_note_ids()? {
        if ids_only {
            results.push((note_id, None));
        } else {
            let note = vault.load_note(&note_id)?;
            results.push((note_id, note.title.clone()));
        }
    }
    Ok(results)
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
            metadata.remove("title");
        } else {
            metadata.insert("title".to_string(), JsonValue::String(title.clone()));
        }
    }

    merge_metadata_json(&mut metadata, &args.metadata)?;

    let body = if args.content.is_some() || args.file.is_some() {
        load_content(args.content.as_ref(), args.file.as_ref())?
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

async fn run_similar(ctx: &CommandContext, vault: &Vault, args: &SimilarArgs) -> Result<()> {
    let anchor = vault
        .load_note(&args.note_id)
        .with_context(|| format!("note {} not found", args.note_id))?;
    if note_is_semantically_empty(&anchor) {
        render_empty_anchor_message(&args.note_id, args.json)?;
        return Ok(());
    }

    let db_path = vault.paths().arrowhead_dir.join("index.db");
    let database = Arc::new(IndexDatabase::open(&db_path)?);
    let selected_model = ctx
        .config
        .embedding_model
        .clone()
        .unwrap_or_else(|| "fast".to_string());
    let pipeline = EmbeddingPipeline::initialise(vault, Arc::clone(&database), &selected_model)
        .await
        .with_context(|| format!("failed to prepare embedding pipeline `{selected_model}`"))?;

    let vector = pipeline
        .store()
        .vector_for_note(&args.note_id)
        .await?
        .with_context(|| {
            format!(
                "note {} does not have embeddings yet. Run `arrowhead index start` to reindex it.",
                args.note_id
            )
        })?;

    let limit = args.limit.max(1);
    let config = SearchConfig::default();
    let oversample = limit + 5;
    let matches = pipeline
        .store()
        .search(&vector, oversample, config.semantic_threshold)
        .await
        .context("embedding search failed")?;

    let synopses = build_similar_synopses(
        &database,
        &args.note_id,
        &matches,
        limit,
        config.semantic_threshold,
    )?;
    render_similar_results(&synopses, args.json, vault)?;
    Ok(())
}

fn build_similar_synopses(
    database: &IndexDatabase,
    anchor_id: &str,
    matches: &[EmbeddingMatch],
    limit: usize,
    threshold: f32,
) -> Result<Vec<SimilarSynopsis>> {
    const SYNOPSIS_LIMIT: usize = 240;
    let mut scored = Vec::new();
    for item in matches {
        if item.note_id == anchor_id {
            continue;
        }
        let similarity = (1.0_f32 - item.distance).max(0.0_f32);
        if similarity < threshold {
            continue;
        }
        scored.push((item.note_id.clone(), similarity));
        if scored.len() >= limit {
            break;
        }
    }

    if scored.is_empty() {
        return Ok(Vec::new());
    }

    let note_ids: Vec<String> = scored.iter().map(|(id, _)| id.clone()).collect();
    let title_map = database
        .titles_for_notes(&note_ids)
        .context("failed to load titles for similar notes")?;
    let relative_map = database
        .relative_paths_for_notes(&note_ids)
        .context("failed to load note paths for similar notes")?;

    let mut synopses = Vec::new();
    for (note_id, similarity) in scored {
        let title = title_map.get(&note_id).cloned().unwrap_or(None);
        let relative_path = relative_map.get(&note_id).cloned();
        let preview = database
            .note_excerpt(&note_id, SYNOPSIS_LIMIT)
            .context("failed to load note synopsis")?;
        synopses.push(SimilarSynopsis {
            note_id,
            similarity,
            title,
            relative_path,
            preview,
        });
    }

    Ok(synopses)
}

fn render_similar_results(
    results: &[SimilarSynopsis],
    json_output: bool,
    vault: &Vault,
) -> Result<()> {
    if json_output {
        let payload: Vec<_> = results
            .iter()
            .map(|entry| {
                let mut object = json!({
                    "note_id": entry.note_id,
                    "similarity": entry.similarity,
                    "title": entry.title,
                    "preview": entry.preview,
                    "relative_path": entry.relative_path,
                });
                if let Some(relative) = entry.relative_path.as_deref() {
                    let absolute = vault.note_path(relative);
                    if let serde_json::Value::Object(ref mut map) = object {
                        map.insert(
                            "absolute_path".to_string(),
                            json!(absolute.display().to_string()),
                        );
                    }
                }
                object
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if results.is_empty() {
        println!("No similar notes found.");
        return Ok(());
    }

    for entry in results {
        let title = entry.title.as_deref().unwrap_or("-");
        println!("{}\t{:.3}\t{}", entry.note_id, entry.similarity, title);
        if let Some(preview) = &entry.preview {
            println!("  {}", preview.trim());
        }
    }

    Ok(())
}

fn note_is_semantically_empty(note: &NoteRecord) -> bool {
    let has_title = note
        .title
        .as_ref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let has_metadata = !note.metadata.is_empty();
    let has_body = !note.content.trim().is_empty();
    !(has_title || has_metadata || has_body)
}

fn render_empty_anchor_message(note_id: &str, json_output: bool) -> Result<()> {
    let message = format!(
        "Note {note_id} is empty, so semantic discovery has nothing to compare. \
         Add content or metadata, reindex, and try again."
    );

    if json_output {
        println!("[]");
        eprintln!("{message}");
    } else {
        println!("{message}");
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct SimilarSynopsis {
    note_id: String,
    similarity: f32,
    title: Option<String>,
    relative_path: Option<String>,
    preview: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use chrono::Utc;
    use tempfile::TempDir;

    use arrowhead_core::{
        NoteRecord,
        graph::{LinkReason, LinkResolutionRecord},
        metadata::{MetadataExtraction, MetadataExtractor},
    };

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
        let entries = collect_note_list(&vault, true).expect("list notes");
        assert!(entries.iter().all(|(_, title)| title.is_none()));
        assert!(entries.iter().any(|(id, _)| id == "2024-01-15"));
    }

    #[test]
    fn list_with_titles_includes_note_titles() {
        let vault = fixture_vault();
        let entries = collect_note_list(&vault, false).expect("list notes");
        let photography_title = entries
            .iter()
            .find(|(id, _)| id == "Photography Equipment")
            .and_then(|(_, title)| title.clone())
            .expect("title present");
        assert_eq!(photography_title, "Photography Equipment");
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

    #[test]
    fn build_similar_synopses_returns_preview_and_title() {
        let temp_dir = TempDir::new().expect("tempdir");
        let database =
            IndexDatabase::open(temp_dir.path().join("index.db")).expect("database opens");
        let vault = fixture_vault();
        let anchor = vault.load_note("2024-01-15").expect("anchor loads");
        let candidate = vault
            .load_note("Photography Equipment")
            .expect("candidate note loads");

        let extractor = MetadataExtractor::new();
        let anchor_meta = extractor
            .extract(&anchor)
            .expect("anchor metadata extraction");
        database
            .upsert_note(&anchor, &anchor_meta, &make_links(&anchor_meta), Utc::now())
            .expect("anchor upsert");

        let candidate_meta = extractor
            .extract(&candidate)
            .expect("candidate metadata extraction");
        database
            .upsert_note(
                &candidate,
                &candidate_meta,
                &make_links(&candidate_meta),
                Utc::now(),
            )
            .expect("candidate upsert");

        let matches = vec![
            EmbeddingMatch {
                note_id: anchor.id.clone(),
                distance: 0.0,
            },
            EmbeddingMatch {
                note_id: candidate.id.clone(),
                distance: 0.05,
            },
        ];

        let synopses = build_similar_synopses(
            &database,
            &anchor.id,
            &matches,
            5,
            SearchConfig::default().semantic_threshold,
        )
        .expect("synopses build");
        assert_eq!(synopses.len(), 1);
        let synopsis = &synopses[0];
        assert_eq!(synopsis.note_id, candidate.id);
        let preview = synopsis.preview.as_ref().expect("preview present");
        assert!(!preview.is_empty());
        assert_eq!(synopsis.title, Some("Photography Equipment".to_string()));
    }

    #[test]
    fn note_with_no_content_or_metadata_is_semantically_empty() {
        let mut note = NoteRecord::new("Empty", "Empty.md", Utc::now(), String::new());
        assert!(note_is_semantically_empty(&note));

        note.content = "   ".to_string();
        assert!(note_is_semantically_empty(&note));

        note.content = "Body text".to_string();
        assert!(!note_is_semantically_empty(&note));
    }

    #[test]
    fn metadata_or_title_counts_as_semantic_content() {
        let mut note = NoteRecord::new("MetaOnly", "MetaOnly.md", Utc::now(), String::new());

        note.metadata
            .insert("status".to_string(), JsonValue::String("draft".to_string()));
        assert!(!note_is_semantically_empty(&note));

        note.metadata.clear();
        note.title = Some("Has Title".to_string());
        assert!(!note_is_semantically_empty(&note));
    }

    fn make_links(extraction: &MetadataExtraction) -> Vec<LinkResolutionRecord> {
        extraction
            .wikilinks
            .iter()
            .map(|link| LinkResolutionRecord {
                raw: link.raw.clone(),
                target: Some(link.target.clone()),
                display: link.display.clone(),
                heading: link.heading.clone(),
                reason: LinkReason::Direct,
            })
            .collect()
    }
}
