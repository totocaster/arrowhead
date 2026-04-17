//! `arrowhead graph` operations.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::json;

use arrowhead_core::{
    DEFAULT_CONTEXT_METRIC_LIMIT, DEFAULT_CONTEXT_NOTE_LIMIT, GraphService, LinkEdge, LinkReason,
    Vault, VaultConfig, sqlite::IndexDatabase,
};

use super::{
    CommandContext,
    context::{SemanticContextMode, build_context_service, render_context_payload},
};

/// Graph command dispatcher.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct GraphCommand {
    /// Emit JSON instead of human-readable output.
    #[arg(long, global = true)]
    pub json: bool,
    /// Select an output format optimised for different pipelines.
    #[arg(long, global = true, value_enum, default_value_t)]
    pub format: GraphOutputFormat,
    /// Graph operation to perform.
    #[command(subcommand)]
    pub action: Option<GraphAction>,
}

/// Supported graph subcommands.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum GraphAction {
    /// Show backlinks pointing to a note.
    Backlinks(NoteIdArg),
    /// Show forward links from a note.
    ForwardLinks(NoteIdArg),
    /// List orphan notes with no links.
    Orphans,
    /// List unresolved WikiLinks.
    Unresolved,
    /// Compatibility alias for `context note`.
    Context(NoteIdArg),
    /// Treat bare `note-id` invocations as context requests.
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// Graph output rendering formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GraphOutputFormat {
    /// Human-friendly narrative output.
    Human,
    /// Emit bare identifiers (or unresolved raw tokens) per line.
    Ids,
}

impl Default for GraphOutputFormat {
    fn default() -> Self {
        Self::Human
    }
}

/// Common argument containing a note identifier.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct NoteIdArg {
    /// Target note identifier.
    pub note_id: String,
}

/// Execute the graph command.
pub async fn run(ctx: &CommandContext, command: &GraphCommand) -> Result<()> {
    let vault_path = ctx
        .config
        .vault
        .clone()
        .context("no vault configured. Provide --vault or run `arrowhead init`.")?;

    let vault = Arc::new(Vault::new(VaultConfig::new(vault_path))?);
    vault.ensure_arrowhead_dirs()?;
    let db_path = vault.paths().arrowhead_dir.join("index.db");
    let database = Arc::new(IndexDatabase::open(&db_path)?);
    let service = GraphService::new(Arc::clone(&database));

    match resolve_action(&command.action)? {
        ResolvedGraphAction::Backlinks(note_id) => {
            let note_id = resolve_note_id(&database, &note_id)?;
            let edges = service.backlinks(&note_id).await?;
            render_backlinks(&note_id, &edges, command.json, command.format)?;
        }
        ResolvedGraphAction::ForwardLinks(note_id) => {
            let note_id = resolve_note_id(&database, &note_id)?;
            let edges = service.forward_links(&note_id).await?;
            render_forward_links(&note_id, &edges, command.json, command.format)?;
        }
        ResolvedGraphAction::Context(note_id) => {
            if !command.json && command.format == GraphOutputFormat::Ids {
                bail!(
                    "--format ids is not supported for `graph context`; use `graph forward-links` or `graph backlinks` instead."
                );
            }
            let note_id = resolve_note_id(&database, &note_id)?;
            let context_service = build_context_service(
                ctx,
                Arc::clone(&vault),
                Arc::clone(&database),
                SemanticContextMode::Preferred,
            )
            .await?;
            let payload = context_service
                .note(
                    &note_id,
                    Some(DEFAULT_CONTEXT_NOTE_LIMIT),
                    Some(DEFAULT_CONTEXT_METRIC_LIMIT),
                )
                .await?;
            if command.json {
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                render_context_payload(&payload);
            }
        }
        ResolvedGraphAction::Orphans => {
            let orphans = service.orphans().await?;
            render_orphans(&orphans, command.json, command.format)?;
        }
        ResolvedGraphAction::Unresolved => {
            let unresolved = service.unresolved_links().await?;
            render_unresolved(&unresolved, command.json, command.format)?;
        }
    }

    Ok(())
}

fn resolve_note_id(database: &IndexDatabase, note_id: &str) -> Result<String> {
    database
        .resolve_note_reference(note_id)?
        .map(|resolved| resolved.note_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "note {note_id} is not indexed. Run `arrowhead index start` to refresh the index."
            )
        })
}

fn resolve_action(action: &Option<GraphAction>) -> Result<ResolvedGraphAction> {
    match action {
        Some(GraphAction::Backlinks(args)) => {
            Ok(ResolvedGraphAction::Backlinks(args.note_id.clone()))
        }
        Some(GraphAction::ForwardLinks(args)) => {
            Ok(ResolvedGraphAction::ForwardLinks(args.note_id.clone()))
        }
        Some(GraphAction::Context(args)) => Ok(ResolvedGraphAction::Context(args.note_id.clone())),
        Some(GraphAction::Orphans) => Ok(ResolvedGraphAction::Orphans),
        Some(GraphAction::Unresolved) => Ok(ResolvedGraphAction::Unresolved),
        Some(GraphAction::External(values)) => {
            if values.is_empty() {
                bail!("expected a note identifier after `arrowhead graph`.");
            }
            if values.len() > 1 {
                bail!(
                    "unexpected arguments {:?}. Provide a single note identifier or choose a subcommand.",
                    values
                );
            }
            Ok(ResolvedGraphAction::Context(values[0].clone()))
        }
        None => {
            bail!(
                "expected a note identifier or subcommand; run `arrowhead graph --help` for usage."
            );
        }
    }
}

#[derive(Debug, Clone)]
enum ResolvedGraphAction {
    Backlinks(String),
    ForwardLinks(String),
    Context(String),
    Orphans,
    Unresolved,
}

fn render_forward_links(
    note_id: &str,
    edges: &[LinkEdge],
    json_output: bool,
    format: GraphOutputFormat,
) -> Result<()> {
    if json_output {
        let payload = json!({
            "note_id": note_id,
            "direction": "outbound",
            "links": edges.iter().map(|edge| edge_to_json(edge, LinkDirection::Forward)).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    match format {
        GraphOutputFormat::Human => {
            if edges.is_empty() {
                println!("No outbound links from {}.", note_id);
                return Ok(());
            }

            println!("Forward links from {}:", note_id);
            for edge in edges {
                let label = format_edge_label(edge, LinkDirection::Forward);
                println!("- {} ({})", label, describe_edge(edge));
            }
            Ok(())
        }
        GraphOutputFormat::Ids => {
            for identifier in forward_link_identifiers(edges) {
                println!("{}", identifier);
            }
            Ok(())
        }
    }
}

fn render_backlinks(
    note_id: &str,
    edges: &[LinkEdge],
    json_output: bool,
    format: GraphOutputFormat,
) -> Result<()> {
    if json_output {
        let payload = json!({
            "note_id": note_id,
            "direction": "inbound",
            "links": edges.iter().map(|edge| edge_to_json(edge, LinkDirection::Backward)).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    match format {
        GraphOutputFormat::Human => {
            if edges.is_empty() {
                println!("No backlinks found for {}.", note_id);
                return Ok(());
            }

            println!("Backlinks to {}:", note_id);
            for edge in edges {
                println!(
                    "- {} ({})",
                    format_edge_label(edge, LinkDirection::Backward),
                    describe_edge(edge)
                );
            }
            Ok(())
        }
        GraphOutputFormat::Ids => {
            for identifier in backlink_identifiers(edges) {
                println!("{}", identifier);
            }
            Ok(())
        }
    }
}

fn render_orphans(note_ids: &[String], json_output: bool, format: GraphOutputFormat) -> Result<()> {
    if json_output {
        let payload = json!({
            "orphan_notes": note_ids,
            "count": note_ids.len(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    match format {
        GraphOutputFormat::Human => {
            if note_ids.is_empty() {
                println!("No orphan notes detected.");
                return Ok(());
            }

            println!("Orphan notes:");
            for note_id in note_ids {
                println!("- {}", note_id);
            }
            Ok(())
        }
        GraphOutputFormat::Ids => {
            for identifier in note_ids {
                println!("{}", identifier);
            }
            Ok(())
        }
    }
}

fn render_unresolved(
    edges: &[LinkEdge],
    json_output: bool,
    format: GraphOutputFormat,
) -> Result<()> {
    if json_output {
        let payload = json!({
            "unresolved_links": edges.iter().map(|edge| edge_to_json(edge, LinkDirection::Forward)).collect::<Vec<_>>(),
            "count": edges.len(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    match format {
        GraphOutputFormat::Human => {
            if edges.is_empty() {
                println!("No unresolved links detected.");
                return Ok(());
            }

            println!("Unresolved links:");
            let mut grouped: BTreeMap<&str, Vec<&LinkEdge>> = BTreeMap::new();
            for edge in edges {
                grouped.entry(edge.source.as_str()).or_default().push(edge);
            }

            for (source, items) in grouped {
                println!("- {}:", source);
                for item in items {
                    println!("  - [[{}]]", item.raw);
                }
            }
            Ok(())
        }
        GraphOutputFormat::Ids => {
            for identifier in unresolved_identifiers(edges) {
                println!("{}", identifier);
            }
            Ok(())
        }
    }
}

fn describe_edge(edge: &LinkEdge) -> String {
    let mut parts = Vec::new();
    match edge.reason {
        LinkReason::Direct => parts.push("wikilink".to_string()),
        LinkReason::Title => parts.push("matched note title".to_string()),
        LinkReason::Alias => parts.push("matched note alias".to_string()),
        LinkReason::Unresolved => parts.push("unresolved".to_string()),
    }

    if let Some(display) = &edge.display_text {
        parts.push(format!("display \"{}\"", display));
    }

    if let Some(heading) = &edge.heading {
        parts.push(format!("heading #{}", heading));
    }

    if parts.is_empty() {
        "wikilink".to_string()
    } else {
        parts.join(", ")
    }
}

fn format_edge_label(edge: &LinkEdge, direction: LinkDirection) -> String {
    match direction {
        LinkDirection::Forward => edge
            .target
            .as_deref()
            .map(String::from)
            .unwrap_or_else(|| format!("[[{}]]", edge.raw)),
        LinkDirection::Backward => edge.source.clone(),
    }
}

#[derive(Debug, Clone, Copy)]
enum LinkDirection {
    Forward,
    Backward,
}

fn edge_to_json(edge: &LinkEdge, direction: LinkDirection) -> serde_json::Value {
    json!({
        "source": edge.source.clone(),
        "target": edge.target.clone(),
        "raw": edge.raw.clone(),
        "display": edge.display_text.clone(),
        "heading": edge.heading.clone(),
        "reason": edge.reason.as_str(),
        "direction": match direction {
            LinkDirection::Forward => "outbound",
            LinkDirection::Backward => "inbound",
        },
    })
}

fn forward_link_identifiers(edges: &[LinkEdge]) -> Vec<String> {
    edges
        .iter()
        .map(|edge| edge.target.clone().unwrap_or_else(|| edge.raw.clone()))
        .collect()
}

fn backlink_identifiers(edges: &[LinkEdge]) -> Vec<String> {
    edges.iter().map(|edge| edge.source.clone()).collect()
}

fn unresolved_identifiers(edges: &[LinkEdge]) -> Vec<String> {
    edges
        .iter()
        .map(|edge| format!("{}\t{}", edge.source, edge.raw))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge_with_target(
        source: &str,
        target: Option<&str>,
        raw: &str,
        reason: LinkReason,
    ) -> LinkEdge {
        LinkEdge {
            source: source.to_string(),
            target: target.map(|value| value.to_string()),
            raw: raw.to_string(),
            display_text: None,
            heading: None,
            reason,
        }
    }

    #[test]
    fn forward_link_identifiers_prefers_target_id() {
        let edges = vec![
            edge_with_target("source-a", Some("target-a"), "Target A", LinkReason::Direct),
            edge_with_target("source-b", None, "Unresolved Note", LinkReason::Unresolved),
        ];
        let identifiers = forward_link_identifiers(&edges);
        assert_eq!(
            identifiers,
            vec!["target-a".to_string(), "Unresolved Note".to_string()]
        );
    }

    #[test]
    fn backlink_identifiers_emit_sources() {
        let edges = vec![
            edge_with_target("note-a", Some("target"), "raw", LinkReason::Direct),
            edge_with_target("note-b", Some("target"), "raw", LinkReason::Alias),
        ];
        let identifiers = backlink_identifiers(&edges);
        assert_eq!(
            identifiers,
            vec!["note-a".to_string(), "note-b".to_string()]
        );
    }

    #[test]
    fn unresolved_identifiers_include_source_and_raw() {
        let edges = vec![edge_with_target(
            "note-a",
            None,
            "Missing Page",
            LinkReason::Unresolved,
        )];
        let identifiers = unresolved_identifiers(&edges);
        assert_eq!(identifiers, vec!["note-a\tMissing Page".to_string()]);
    }
}
