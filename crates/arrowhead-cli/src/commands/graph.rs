//! `arrowhead graph` operations.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::json;

use arrowhead_core::{
    GraphContext, GraphService, LinkEdge, LinkReason, Vault, VaultConfig, sqlite::IndexDatabase,
};

use super::CommandContext;

/// Graph command dispatcher.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct GraphCommand {
    /// Emit JSON instead of human-readable output.
    #[arg(long, global = true)]
    pub json: bool,
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
    /// Show a full context summary for a note.
    Context(NoteIdArg),
    /// Treat bare `note-id` invocations as context requests.
    #[command(external_subcommand)]
    External(Vec<String>),
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

    let vault = Vault::new(VaultConfig::new(vault_path))?;
    vault.ensure_arrowhead_dirs()?;
    let db_path = vault.paths().arrowhead_dir.join("index.db");
    let database = Arc::new(IndexDatabase::open(&db_path)?);
    let service = GraphService::new(Arc::clone(&database));

    match resolve_action(&command.action)? {
        ResolvedGraphAction::Backlinks(note_id) => {
            ensure_note_indexed(&database, &note_id)?;
            let edges = service.backlinks(&note_id).await?;
            render_backlinks(&note_id, &edges, command.json)?;
        }
        ResolvedGraphAction::ForwardLinks(note_id) => {
            ensure_note_indexed(&database, &note_id)?;
            let edges = service.forward_links(&note_id).await?;
            render_forward_links(&note_id, &edges, command.json)?;
        }
        ResolvedGraphAction::Context(note_id) => {
            ensure_note_indexed(&database, &note_id)?;
            let context = service.context(&note_id).await?;
            render_context(&note_id, &context, command.json)?;
        }
        ResolvedGraphAction::Orphans => {
            let orphans = service.orphans().await?;
            render_orphans(&orphans, command.json)?;
        }
        ResolvedGraphAction::Unresolved => {
            let unresolved = service.unresolved_links().await?;
            render_unresolved(&unresolved, command.json)?;
        }
    }

    Ok(())
}

fn ensure_note_indexed(database: &IndexDatabase, note_id: &str) -> Result<()> {
    if database.note_state(note_id)?.is_some() {
        Ok(())
    } else {
        bail!("note {note_id} is not indexed. Run `arrowhead vault start` to refresh the index.");
    }
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

fn render_forward_links(note_id: &str, edges: &[LinkEdge], json_output: bool) -> Result<()> {
    if json_output {
        let payload = json!({
            "note_id": note_id,
            "direction": "outbound",
            "links": edges.iter().map(|edge| edge_to_json(edge, LinkDirection::Forward)).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

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

fn render_backlinks(note_id: &str, edges: &[LinkEdge], json_output: bool) -> Result<()> {
    if json_output {
        let payload = json!({
            "note_id": note_id,
            "direction": "inbound",
            "links": edges.iter().map(|edge| edge_to_json(edge, LinkDirection::Backward)).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

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

fn render_orphans(note_ids: &[String], json_output: bool) -> Result<()> {
    if json_output {
        let payload = json!({
            "orphan_notes": note_ids,
            "count": note_ids.len(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

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

fn render_unresolved(edges: &[LinkEdge], json_output: bool) -> Result<()> {
    if json_output {
        let payload = json!({
            "unresolved_links": edges.iter().map(|edge| edge_to_json(edge, LinkDirection::Forward)).collect::<Vec<_>>(),
            "count": edges.len(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

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

fn render_context(note_id: &str, context: &GraphContext, json_output: bool) -> Result<()> {
    if json_output {
        let forward: Vec<_> = context
            .forward_links
            .iter()
            .map(|edge| edge_to_json(edge, LinkDirection::Forward))
            .collect();
        let backlinks: Vec<_> = context
            .backlinks
            .iter()
            .map(|edge| edge_to_json(edge, LinkDirection::Backward))
            .collect();
        let unresolved: Vec<_> = context
            .forward_links
            .iter()
            .filter(|edge| edge.reason == LinkReason::Unresolved)
            .map(|edge| edge_to_json(edge, LinkDirection::Forward))
            .collect();

        let payload = json!({
            "note_id": note_id,
            "summary": {
                "forward": forward.len(),
                "backlinks": backlinks.len(),
                "unresolved": unresolved.len(),
            },
            "forward_links": forward,
            "backlinks": backlinks,
            "unresolved": unresolved,
        });

        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    render_forward_links(note_id, &context.forward_links, false)?;
    println!();
    render_backlinks(note_id, &context.backlinks, false)?;

    let unresolved: Vec<&LinkEdge> = context
        .forward_links
        .iter()
        .filter(|edge| edge.reason == LinkReason::Unresolved)
        .collect();
    if !unresolved.is_empty() {
        println!();
        println!("Unresolved links from {}:", note_id);
        for edge in unresolved {
            println!("- [[{}]]", edge.raw);
        }
    }

    Ok(())
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
