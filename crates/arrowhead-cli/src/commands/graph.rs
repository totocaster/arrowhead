//! `arrowhead graph` operations.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use arrowhead_core::{
    GraphContext, GraphService, LinkEdge, LinkReason, Vault, VaultConfig, sqlite::IndexDatabase,
};

use super::CommandContext;

/// Graph command dispatcher.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct GraphCommand {
    /// Graph operation to perform.
    #[command(subcommand)]
    pub action: GraphAction,
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

    match &command.action {
        GraphAction::Backlinks(args) => {
            ensure_note_indexed(&database, &args.note_id)?;
            let edges = service.backlinks(&args.note_id).await?;
            render_backlinks(&args.note_id, &edges);
        }
        GraphAction::ForwardLinks(args) => {
            ensure_note_indexed(&database, &args.note_id)?;
            let edges = service.forward_links(&args.note_id).await?;
            render_forward_links(&args.note_id, &edges);
        }
        GraphAction::Orphans => {
            let orphans = service.orphans().await?;
            render_orphans(&orphans);
        }
        GraphAction::Unresolved => {
            let unresolved = service.unresolved_links().await?;
            render_unresolved(&unresolved);
        }
        GraphAction::Context(args) => {
            ensure_note_indexed(&database, &args.note_id)?;
            let context = service.context(&args.note_id).await?;
            render_context(&args.note_id, &context);
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

fn render_forward_links(note_id: &str, edges: &[LinkEdge]) {
    if edges.is_empty() {
        println!("No outbound links from {}.", note_id);
        return;
    }

    println!("Forward links from {}:", note_id);
    for edge in edges {
        let label = format_edge_label(edge, LinkDirection::Forward);
        println!("- {} ({})", label, describe_edge(edge));
    }
}

fn render_backlinks(note_id: &str, edges: &[LinkEdge]) {
    if edges.is_empty() {
        println!("No backlinks found for {}.", note_id);
        return;
    }

    println!("Backlinks to {}:", note_id);
    for edge in edges {
        println!(
            "- {} ({})",
            format_edge_label(edge, LinkDirection::Backward),
            describe_edge(edge)
        );
    }
}

fn render_orphans(note_ids: &[String]) {
    if note_ids.is_empty() {
        println!("No orphan notes detected.");
        return;
    }

    println!("Orphan notes:");
    for note_id in note_ids {
        println!("- {}", note_id);
    }
}

fn render_unresolved(edges: &[LinkEdge]) {
    if edges.is_empty() {
        println!("No unresolved links detected.");
        return;
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
}

fn render_context(note_id: &str, context: &GraphContext) {
    render_forward_links(note_id, &context.forward_links);
    println!();
    render_backlinks(note_id, &context.backlinks);

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
