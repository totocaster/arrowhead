# Arrowhead: Complete Rewrite Specification

**Date:** 2025-10-21
**Author:** Claude Code Analysis
**Based On:** Synapse-Obsidian (Swift implementation)
**Original repo:** https://github.com/totocaster/Synapse-Obsidian

---

## Executive Summary

This document specifies a complete rewrite of Synapse from Swift to Rust as **Arrowhead**, transforming it from a macOS menu bar app with real-time indexing to a cross-platform CLI tool with on-demand indexing. The core functionality—Obsidian vault indexing, full-text search, semantic search, and MCP server—remains intact while the architecture is simplified for CLI-first operation.

**Name Origin:** Arrowhead references the precision obsidian tools used by prehistoric humans for hunting and crafting—sharp, targeted instruments that point the way, just as this tool precisely finds and connects knowledge within your Obsidian vault.

## Current Status (2025-10-21)

- Workspace standardised on Rust 1.85 / edition 2024 (`rust-toolchain.toml` committed).
- Core, CLI, and MCP crates compile with fully typed scaffolding; modules validate configuration and return descriptive `todo` errors instead of panicking.
- CLI parses the complete command surface (`init`, `index`, `search`, `notes`, `graph`, `vault`) and persists config, ready for implementation work.
- Documentation refreshed (`docs/api.md`, `docs/mcp_protocol.md`) and tests directory bootstrapped (`tests/integration/`).
- Vector storage integration sits behind an optional `vector-lancedb` feature that stays disabled until semantic search work begins.

### Key Changes from Original

| Aspect | Original (Swift) | Arrowhead |
|--------|------------------|-----------|
| **Platform** | macOS only | macOS + Linux |
| **Interface** | Menu bar GUI app | CLI tool |
| **Indexing** | Real-time FSEvents monitoring | Smart on-demand (mtime-based) |
| **Deployment** | Sandboxed app bundle | Single binary |
| **MCP Modes** | stdio only | stdio (local) + HTTP (remote) |
| **Language** | Swift 5.9+ | Rust 1.85+ (2024 edition) |
| **Vector Storage** | SQLite BLOBs | Dedicated vector database |

---

## Table of Contents

1. [Architecture Overview](#architecture-overview)
2. [Core Components](#core-components)
3. [Storage Strategy](#storage-strategy)
4. [Feature Specifications](#feature-specifications)
5. [MCP Protocol Implementation](#mcp-protocol-implementation)
6. [CLI Interface Design](#cli-interface-design)
7. [Technology Stack](#technology-stack)
8. [Implementation Phases](#implementation-phases)
9. [Performance Targets](#performance-targets)
10. [Security Considerations](#security-considerations)

---

## Architecture Overview

### High-Level Design

```
┌─────────────────────────────────────────────────────────────┐
│                      Arrowhead Binary                        │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ CLI Commands │  │  MCP Server  │  │  MCP Remote  │      │
│  │   (clap)     │  │   (stdio)    │  │   (HTTP)     │      │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘      │
│         │                 │                 │               │
│         └─────────────────┴─────────────────┘               │
│                           │                                 │
│         ┌─────────────────▼─────────────────┐               │
│         │      Core Library (arrowhead-core)  │             │
│         ├───────────────────────────────────┤               │
│         │  • Vault       • Indexer          │               │
│         │  • Search      • Embeddings       │               │
│         │  • Graph       • Metadata         │               │
│         └─────────────────┬─────────────────┘               │
│                           │                                 │
└───────────────────────────┼─────────────────────────────────┘
                            │
                 ┌──────────▼──────────┐
                 │  Obsidian Vault     │
                 ├─────────────────────┤
                 │  • *.md files       │
                 │  • .arrowhead/      │
                 │    - index.db       │
                 │    - vectors/       │
                 │  • Attachments/     │
                 └─────────────────────┘
```

### Project Structure

```
arrowhead/
├── rust-toolchain.toml        # Toolchain pin (Rust 1.85.0)
├── Cargo.toml                 # Workspace definition
├── crates/
│   ├── arrowhead-core/        # Core library
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── vault.rs
│   │   │   ├── indexer.rs
│   │   │   ├── search.rs
│   │   │   ├── embeddings.rs
│   │   │   ├── graph.rs
│   │   │   ├── metadata.rs
│   │   │   └── types.rs
│   │   └── Cargo.toml
│   │
│   ├── arrowhead-mcp/         # MCP protocol implementation
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── protocol.rs
│   │   │   ├── stdio.rs
│   │   │   ├── http.rs
│   │   │   ├── auth.rs
│   │   │   ├── tools.rs
│   │   │   └── handlers.rs
│   │   └── Cargo.toml
│   │
│   ├── arrowhead-deamon/      # Background runtime (watcher + control socket)
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── runtime.rs
│   │   │   ├── control.rs
│   │   │   ├── watcher.rs
│   │   │   └── main.rs
│   │   ├── tests/
│   │   │   └── runtime.rs
│   │   └── Cargo.toml
│   │
│   └── arrowhead-cli/         # CLI application
│       ├── src/
│       │   ├── main.rs
│       │   ├── commands/
│       │   │   ├── mod.rs
│       │   │   ├── init.rs
│       │   │   ├── index.rs
│       │   │   ├── search.rs
│       │   │   ├── notes.rs
│       │   │   ├── graph.rs
│       │   │   └── vault.rs
│       │   └── config.rs
│       └── Cargo.toml
│
├── docs/
│   ├── predev_synapse_rust_rewrite.md  # This document
│   ├── api.md                          # API documentation
│   └── mcp_protocol.md                 # MCP protocol spec
│
├── tests/
│   ├── integration/           # Integration tests
│   │   └── .gitkeep
│   └── fixtures/              # Test vaults
│
└── README.md
```

---

## Core Components

### 1. Vault Operations

**Responsibility:** Manage file system operations for markdown notes in an Obsidian vault.

**Core Functionality:**
- Read and write markdown files with YAML frontmatter
- Parse frontmatter metadata using YAML parser
- Preserve all Obsidian-compatible metadata fields
- Track file modification times for smart reindexing
- Auto-create `.arrowhead/` directory for indexes
- Respect Obsidian settings (attachments folder, link format)

**CRUD Operations:**
- Get note by ID (filename without .md)
- List all note IDs in vault
- Create new note with metadata
- Update existing note
- Delete note

### 2. Metadata Extraction

**Responsibility:** Extract structured metadata from note content and frontmatter.

**Extraction Rules:**
- **YAML Frontmatter**: All fields preserved as-is (flexible schema)
- **Inline tags**: Match pattern `#tag-name` (alphanumeric + hyphens)
- **WikiLinks**: Match `[[Note Title]]` or `[[note-id]]`
- **Embed exclusion**: Skip `![[image.png]]` (attachments, not notes)
- **Aliases**: Extract from frontmatter `aliases` field
- **Common fields**: Support `category`, `date`, `tags`, `author`, `status`, etc.

**Output:** Structured metadata combining frontmatter + extracted elements

### 3. Indexer

**Responsibility:** Index notes into search-optimized storage with smart staleness detection.

**Indexing Process:**

1. **Staleness Check:**
   - Compare file `modified_at` timestamp with `indexed_at` in database
   - Skip indexing if file hasn't changed
   - Support force reindex flag to override

2. **Metadata Processing:**
   - Parse YAML frontmatter
   - Extract inline tags and WikiLinks
   - Serialize metadata for FTS (field:value format)

3. **Embedding Generation:**
   - Async operation using configured embedding model
   - Combine note title + content for context
   - Store in dedicated vector storage

4. **Index Updates:**
   - Insert/update FTS5 virtual table for full-text search
   - Store metadata in key-value table
   - Update vector database with embeddings
   - Resolve and store WikiLink relationships

5. **Graph Updates:**
   - Resolve WikiLinks to note IDs (multi-strategy matching)
   - Store resolved/unresolved links in graph table

**Optimization Features:**
- Parallel indexing with configurable worker count
- Batch embedding generation for efficiency
- Transaction batching (commit every N notes)
- Progress callback support

### 4. Search

**Responsibility:** Provide full-text, semantic, and hybrid search capabilities.

**Search Modes:**

1. **Full-Text Search (FTS):**
   - SQLite FTS5-based keyword matching
   - Field:value query syntax for metadata filtering
   - Boolean operators (AND, OR, NOT)
   - Relevance ranking via BM25
   - Example queries:
     - `category:project photography`
     - `(tags:swift OR tags:rust) status:published`
     - `content:machine learning`

2. **Semantic Search:**
   - Vector similarity using cosine distance
   - Generate query embedding on-the-fly
   - Threshold-based filtering
   - K-nearest neighbor retrieval
   - Optional: HNSW index for approximate search

3. **Hybrid Search:**
   - Combine FTS and semantic results
   - Weighted score merging (configurable weights)
   - Confidence threshold filtering
   - Run both searches in parallel

### 5. Deamon Runtime

**Responsibility:** Maintain a hot index by reacting to filesystem changes and exposing a control surface for the CLI.

**Runtime Behaviour:**
- Launches from the `arrowhead-deamon` crate (Tokio binary) and expects `ARROWHEAD_VAULT` to point at the vault root.
- Records status snapshots under `.arrowhead/deamon/status.json`, including indexed/error counts, current activity, queued jobs, download progress, and surfaced issues.
- Writes structured logs to `.arrowhead/logs/arrowhead-deamon.log`.
- Watches the vault (excluding `.arrowhead/`, ignored folders, and attachments) via `notify`, coalescing events into a bounded queue before calling `Indexer::reindex_paths`.
- Persists a PID file and owns the control socket at `.arrowhead/deamon/control.sock`.

**Control Plane (Phase 2 scope):**
- JSON-over-Unix-socket protocol with `status` and `shutdown` commands.
- Responses reuse the shared `DeamonStatus` types so the CLI and future services share a schema.
- Socket binds with owner-only permissions; stale sockets are removed on startup/shutdown.

**Future Phases:**
- Auto-start integration (launchd/systemd) and richer command set (`health`, `reload-config`) are scheduled for Phase 3+.

**Search Results:**
- Note ID, metadata, relevance score
- Content preview (configurable length)
- Match explanation (e.g. FTS rank vs semantic similarity)
- Optional: Link statistics (backlinks, forward links)

### 5. Embeddings

**Responsibility:** Generate and manage vector embeddings for semantic search.

**Embedding Generation:**
- Support for multiple models via configurable backend
- Default: ONNX models via `fastembed` (all-MiniLM-L6-v2)
- Alternative models: BGE-small, BGE-base (better quality)
- Batch processing for multiple notes
- Query vs document embedding modes

**Similarity Calculation:**
- Cosine similarity for vector comparison
- Store normalized vectors for efficiency
- Support for dot product and Euclidean distance

**Performance Optimization:**
- HNSW index for approximate nearest neighbor (optional)
- SIMD acceleration for similarity calculation (if available)
- Vector storage optimized for read performance

### 6. Graph Navigation

**Responsibility:** Navigate and analyze WikiLinks relationships between notes.

**Graph Operations:**
- **Backlinks**: Find all notes linking TO a target note
- **Forward links**: Find all notes a source note links TO
- **Bidirectional links**: Find mutual links between notes
- **Orphan detection**: Find notes with no incoming/outgoing links
- **Unresolved links**: Track broken WikiLinks for later resolution

**Link Resolution Strategy:**
1. Direct ID match (exact filename)
2. Title match (search metadata table)
3. Alias match (search aliases field)
4. Fuzzy match (case-insensitive partial match)
5. Store as unresolved if no match found

**Graph Context:**
- Complete context: all links + statistics for a note
- Link counts and graph metrics
- Support for future graph analysis algorithms

---

## Storage Strategy

### Primary Index Database (SQLite)

**Location:** `.arrowhead/index.db`

**Purpose:** Full-text search, metadata, and graph relationships

**Tables:**

1. **notes**
   - Core note tracking with timestamps
   - Fields: id (PK), created_at, updated_at, indexed_at, file_modified_at

2. **metadata**
   - Key-value store for flexible metadata
   - Fields: note_id (FK), key, value
   - Indexed on key for fast filtering

3. **notes_fts** (FTS5 virtual table)
   - Full-text search index
   - Columns: id (unindexed), content, metadata
   - Tokenizer: porter unicode61
   - Metadata column stores serialized field:value pairs

4. **note_links**
   - WikiLinks graph relationships
   - Fields: source_id, target_id (nullable), link_text, link_type, created_at
   - Indexed on both source and target for bidirectional queries

**Triggers:**
- Auto-delete from FTS when note deleted (CASCADE)

### Vector Storage Options

**Requirement:** Efficient storage and retrieval of embedding vectors with similarity search support.

**Option A: LanceDB (Recommended)**

**Why LanceDB:**
- Native Rust support with official `lancedb` crate
- Persistent local file storage (no server required)
- Columnar format optimized for vector operations
- Built-in HNSW index for fast ANN search
- Supports metadata filtering alongside vector search
- Can scale to millions of vectors
- Zero-copy memory mapping for efficiency

**Structure:**
- Location: `.arrowhead/vectors/` directory
- Stores vectors + metadata in columnar Lance format
- Automatic HNSW index creation
- Native support for f32 vectors (384 or 768 dimensions)

**Implementation Approach:**
- Open LanceDB connection at `.arrowhead/vectors`
- Create table with schema: note_id, vector, model_name, created_at
- Use native ANN search with cosine similarity
- Batch upsert during indexing
- Query interface: vector search with optional metadata filters

> **Implementation note:** The Rust workspace exposes a `vector-lancedb`
> Cargo feature. It is disabled by default so the scaffolding builds on Rust
> 1.85. Enabling the feature pulls in LanceDB and its dependencies once the
> toolchain is bumped.

**Option B: Qdrant (Alternative - More Features)**

**Why Qdrant:**
- Pure Rust implementation
- Advanced filtering and hybrid search
- Better for large-scale deployments
- Can run embedded (no server) or client-server

**Trade-offs:**
- More complex than needed for single-user
- Heavier dependencies
- Overkill for typical vault sizes (<100k notes)

**Recommendation:** Start with LanceDB for simplicity, consider Qdrant for future if advanced features needed.

**Option C: Custom HNSW + SQLite (Fallback)**

**Why Custom:**
- Full control over implementation
- Lighter dependencies
- Store index in SQLite, vectors in binary files

**Trade-offs:**
- More implementation work
- Less optimized than specialized libraries
- Harder to maintain

**Decision Matrix:**

| Feature | LanceDB | Qdrant | Custom |
|---------|---------|--------|--------|
| **Rust Support** | Excellent | Excellent | N/A |
| **Local Storage** | Native | Embedded mode | Native |
| **Performance** | Excellent | Excellent | Good |
| **Complexity** | Low | Medium | High |
| **Scalability** | 10M+ vectors | 100M+ vectors | 1M vectors |
| **Dependencies** | Light | Heavy | Minimal |
| **Maintenance** | Low | Low | High |

**Final Recommendation:** Use LanceDB for initial implementation.

---

## Feature Specifications

### 1. Smart Indexing

**Goal:** Only reindex notes that changed since last indexing.

**Algorithm:**
- Iterate through all note IDs in vault
- For each note:
  - Read file modification time from filesystem
  - Query `indexed_at` timestamp from database
  - Skip if `file_mtime <= indexed_at` (unless force flag set)
  - Otherwise, perform full indexing pipeline
- Track statistics: total, indexed, skipped, errors

**Optimization:**
- Parallel processing with configurable worker pool
- Batch commits to reduce transaction overhead
- Async embedding generation with semaphore limiting

**Progress Reporting:**
- Optional callback with current progress
- Display: current file, percentage, files/sec

### 2. Full-Text Search with Field:Value Syntax

**Query Processing:**
- Parse query for `field:value` patterns using regex
- Distinguish between content and metadata fields
- Rewrite to FTS5 column-specific queries
- Special handling: `content:term` searches only content column
- Metadata fields: search for literal "field:value" in metadata column

**Examples:**
- Input: `category:project photography`
- Rewrite: `metadata:"category:project" AND photography`

**FTS5 Features:**
- Porter stemming for English
- Unicode61 tokenization
- BM25 relevance ranking
- Phrase search with quotes
- Boolean operators (AND, OR, NOT, parentheses)

**Metadata Serialization:**
- Convert metadata hash to searchable text
- Format: space-separated `field:value` pairs
- Handle arrays: `tags:swift tags:async tags:rust`
- Handle dates: `created:2024-10-21`
- Handle booleans: `published:true`

### 3. Semantic Search with Embeddings

**Query Flow:**
1. Generate embedding for search query using same model as notes
2. Load all note embeddings from vector database
3. Calculate similarity (cosine distance) for each
4. Filter by threshold
5. Sort by similarity descending
6. Return top K results

**With HNSW Index:**
- Use approximate nearest neighbor search
- Trade-off: slight accuracy loss for massive speed gain
- Configure: number of candidates, search depth

**Result Enrichment:**
- Load full metadata for matched notes
- Generate content preview (first N chars or smart extraction)
- Include similarity score

**Performance Considerations:**
- Normalize vectors at storage time (avoid runtime normalization)
- Use SIMD operations if available (via `simsimd` crate)
- Consider memory-mapping large vector collections

### 4. Hybrid Search

**Strategy:** Combine best of both FTS and semantic search.

**Process:**
1. Run FTS and semantic searches in parallel (tokio::join!)
2. Fetch more results than needed (2x limit) from each
3. Merge results with weighted scoring:
   - FTS weight: 0.7 (emphasize exact matches)
   - Semantic weight: 0.5 (include conceptual matches)
4. Sum scores for notes appearing in both result sets
5. Sort by combined score descending
6. Filter by confidence threshold
7. Return top K results

**Tuning Parameters:**
- FTS weight (default: 0.7)
- Semantic weight (default: 0.5)
- Confidence threshold (default: 0.3)
- Result multiplier for pre-filtering

### 5. Vault Conventions Discovery

**Goal:** Analyze vault to discover organizational patterns for AI agents.

**Analysis:**

1. **Naming Patterns:**
   - Regex analysis of note IDs
   - Detect date formats (YYYY-MM-DD, etc.)
   - Identify naming conventions (kebab-case, Title Case, etc.)

2. **Metadata Analysis:**
   - Aggregate all metadata fields across notes
   - Track field frequency and usage patterns
   - Infer value types (string, array, date, boolean)
   - Find most common values per field

3. **Obsidian Settings:**
   - Parse `.obsidian/app.json` if exists
   - Extract: attachments folder, link format, daily notes format
   - Read plugin settings if relevant

4. **User Style Guide:**
   - Check for `.arrowhead/STYLE_GUIDE.md`
   - Include contents in conventions response

**Output:**
- Naming patterns (list of detected patterns)
- Metadata field statistics (usage counts, types)
- Common field values (top 10 per field)
- Obsidian configuration
- User style guide text (optional)

---

## MCP Protocol Implementation

### Transport Modes

#### 1. Local MCP (stdio)

**Usage:** `arrowhead --mcp`

**Protocol:**
- JSON-RPC 2.0 over stdin/stdout
- One request per line (newline-delimited JSON)
- Responses written to stdout
- Logs written to stderr (separate stream)

**Behavior:**
- Read from stdin in loop
- Parse JSON-RPC request
- Dispatch to handler
- Write JSON-RPC response to stdout
- Continue until stdin closes

#### 2. Remote MCP (HTTP)

**Server Usage:** `arrowhead --mcp-server --bind 127.0.0.1:8080 --token <secret>`

**Protocol:**
- HTTP POST to `/rpc` endpoint
- JSON-RPC 2.0 in request body
- Bearer token authentication (Authorization header)
- CORS disabled by default (single-user)

**Authentication:**
- Require `Authorization: Bearer <token>` header
- Validate token against configuration
- IP allowlist enforcement (default: localhost only)
- Return 401 Unauthorized if invalid
- Return 403 Forbidden if IP not allowed

**Server Features:**
- Health check endpoint: GET `/health`
- Metrics endpoint (optional): GET `/metrics`
- Graceful shutdown on SIGTERM

### MCP Tools

**Included Tools:**

| Category | Method | Description |
|----------|--------|-------------|
| **Search** | `search_fts` | Full-text search with field:value syntax |
| | `search_similarity` | Semantic search with embeddings |
| | `search_hybrid` | Combined FTS + semantic search |
| **Notes** | `read_note` | Get complete note content |
| | `list_notes` | List notes with optional metadata filtering |
| | `get_note_metadata` | Get metadata without content |
| | `create_note` | Create new note (CRUD) |
| | `update_note` | Update existing note (CRUD) |
| | `delete_note` | Delete note (CRUD) |
| **Graph** | `get_note_graph` | Complete graph context for a note |
| | `get_backlinks` | Notes linking TO this note |
| | `get_forward_links` | Notes linked FROM this note |
| | `find_orphan_notes` | Notes with no WikiLinks |
| | `find_unresolved_links` | Broken WikiLinks |
| **Discovery** | `get_related_notes` | Semantically similar notes |
| | `get_vault_stats` | Vault overview and statistics |
| | `get_vault_conventions` | Naming patterns, metadata conventions |
| **Protocol** | `initialize` | Initialize connection, return server info |
| | `tools/list` | List all available tools |

**Excluded Tools** (from original Synapse):
- ❌ AI provider integrations (OpenAI, Anthropic, Ollama)
- ❌ TODO management (`list_todos`)
- ❌ Loose leaf ingestion

### Tool Schema Structure

Each tool defines:
- Name (string identifier)
- Description (what the tool does)
- Input schema (JSON Schema describing parameters)
  - Required parameters
  - Optional parameters with defaults
  - Type constraints and validation rules
  - Example values

### Error Handling

JSON-RPC error codes:
- `-32700`: Parse error
- `-32600`: Invalid request
- `-32601`: Method not found
- `-32602`: Invalid parameters
- `-32603`: Internal error
- `-32000`: Vault not configured
- `-32001`: Index not found
- `-32002`: Note not found
- `-32003`: Search error
- `-32004`: Write error
- `-32005`: Invalid note ID

---

## CLI Interface Design

### Command Structure

```
arrowhead [OPTIONS] [COMMAND]

OPTIONS:
    --vault <PATH>          Path to vault (default: from config)
    --config <PATH>         Config file path
    --mcp                   Run in MCP stdio mode
    --mcp-server            Run MCP HTTP server
    --bind <ADDR>           HTTP server bind address
    --token <TOKEN>         Authentication token for MCP
    -v, --verbose           Verbose logging
    -q, --quiet             Suppress output
    -h, --help              Show help

COMMANDS:
    init                    Initialize vault configuration
    index                   Index vault notes
    search                  Search notes
    notes                   Note operations
    graph                   Graph navigation
    vault                   Vault management commands
```

### Command Details

#### `init` - Initialize Configuration

Creates configuration file and vault directories.

**Options:**
- `--vault <PATH>`: Path to vault root (required)
- `--embeddings <MODEL>`: Embedding model name (default: all-MiniLM-L6-v2)
- `--force`: Overwrite existing config

**Actions:**
- Create config file at `~/.config/arrowhead/config.toml`
- Create `.arrowhead/` directory in vault
- Validate vault structure
- Delegate final setup to `arrowhead vault init` (which can launch the deamon)

#### `index` - Index Vault

Informational command that explains the deamon-managed indexing workflow.

**Behavior:**
- Prints guidance directing users to `arrowhead vault start` and `arrowhead vault status`
- Remains available for backwards compatibility but performs no indexing work

#### `search` - Search Notes

Search with multiple modes.

**Subcommands:**
- `fts <QUERY>`: Full-text search
- `semantic <QUERY>`: Semantic search
- `hybrid <QUERY>`: Hybrid search

**Common Options:**
- `--limit <N>`: Maximum results (default: 10)
- `--json`: Output as JSON
- `--ids-only`: Output only note IDs

**Mode-Specific Options:**
- FTS: `--offset <N>` for pagination
- Semantic: `--threshold <F>` for similarity threshold
- Hybrid: `--confidence <F>` for confidence threshold

#### `notes` - Note Operations

CRUD operations for notes.

**Subcommands:**
- `read <ID>`: Read note content
- `list`: List all notes
- `create`: Create new note
- `update <ID>`: Update existing note
- `delete <ID>`: Delete note

**Create Options:**
- `--id <ID>`: Note ID (auto-generated if not specified)
- `--title <TITLE>`: Note title
- `--category <CAT>`: Category
- `--content <TEXT>`: Note content (or read from stdin)
- `--file <PATH>`: Read content from file
- `--metadata <JSON>`: Additional metadata as JSON

**Update Options:**
- `--content <TEXT>`: New content
- `--file <PATH>`: Read content from file
- `--title <TITLE>`: New title
- `--metadata <JSON>`: Update metadata

#### `graph` - Graph Navigation

Navigate WikiLinks relationships.

**Subcommands:**
- `backlinks <ID>`: Show notes linking to this note
- `forward-links <ID>`: Show notes this note links to
- `orphans`: Find orphaned notes
- `unresolved`: Find broken WikiLinks
- `context <ID>`: Show complete graph context

#### `vault` - Vault Management

Manage the background deamon and Arrowhead working directories.

**Subcommands:**
- `init`: Prepare `.arrowhead/` directories and (by default) launch the deamon
- `start`: Launch or relaunch the deamon, waiting for the control socket
- `status`: Query the control socket or fallback status file for health and progress
- `stop`: Request a graceful shutdown via the control socket
- `cleanup`: Stop the deamon if running, then remove `.arrowhead/` caches (index, vectors, logs, status, socket, PID)

### Configuration File

**Location:** `~/.config/arrowhead/config.toml`

**Structure:**

```toml
[vault]
path = "/path/to/vault"
attachments_folder = "Attachments"

[indexing]
model = "all-MiniLM-L6-v2"
parallel = 8
batch_size = 32

[search]
default_limit = 10
default_threshold = 0.7
confidence_threshold = 0.3

[mcp]
bind = "127.0.0.1:8080"
allowed_ips = ["127.0.0.1"]

[logging]
level = "info"
```

---

## Technology Stack

### Core Dependencies

| Category | Crate | Purpose |
|----------|-------|---------|
| **CLI** | `clap` (4.5+) | Command-line argument parsing with derive |
| | `directories` (5.0+) | XDG config directory paths |
| **Config** | `toml` (0.8+) | TOML configuration parsing |
| | `serde` (1.0+) | Serialization framework |
| **Database** | `rusqlite` (0.32+) | SQLite bindings with FTS5 |
| | `r2d2` + `r2d2_sqlite` | Connection pooling |
| **Vectors** | `lancedb` (latest) | Vector database for embeddings |
| | `fastembed` (3.0+) | ONNX embedding models |
| | `ndarray` (0.15+) | N-dimensional arrays |
| **Async** | `tokio` (1.40+) | Async runtime (full features) |
| | `async-trait` (0.1+) | Async trait support |
| **HTTP** | `axum` (0.7+) | Web framework |
| | `tower` + `tower-http` | Middleware |
| **Parsing** | `serde_yaml` (0.9+) | YAML frontmatter |
| | `serde_json` (1.0+) | JSON support |
| | `regex` (1.10+) | Regular expressions |
| **Utilities** | `anyhow` (1.0+) | Error handling |
| | `thiserror` (1.0+) | Custom error types |
| | `chrono` (0.4+) | Date/time handling |
| | `tracing` + `tracing-subscriber` | Structured logging |
| **UI** | `indicatif` (0.17+) | Progress bars (optional) |
| | `colored` (2.1+) | Terminal colors (optional) |

### Optional Dependencies

| Feature | Crates |
|---------|--------|
| HNSW Index | `instant-distance` or `usearch` |
| SIMD Similarity | `simsimd` |
| Compression | `zstd` |

---

## Implementation Phases

### Phase 0: Scaffolding & Tooling (Complete)

**Goal:** Establish workspace, documentation, and foundational APIs.

**Deliverables:**
- Rust 1.85 / edition 2024 toolchain pinned via `rust-toolchain.toml`.
- Core/CLI/MCP crates laid out with typed modules returning descriptive `todo` errors.
- CLI command surface and shared config loader implemented.
- Initial documentation and test harness directories added.
- Optional `vector-lancedb` feature wired for future LanceDB enablement.

### Phase 1: Core Foundation (Weeks 1-2)

**Goal:** Basic vault operations and FTS indexing.

**Deliverables:**
- Vault file operations (read/write markdown)
- YAML frontmatter parsing
- Metadata extraction (tags, WikiLinks)
- SQLite database setup with FTS5
- Basic indexing without embeddings
- CLI framework with `clap`
- Configuration management
- Commands: `init`, `index`, `notes read/list`

### Phase 2: Search & Embeddings (Weeks 3-4)

**Goal:** Full search capabilities with vectors.

**Deliverables:**
- FTS search with field:value syntax
- LanceDB integration for vectors
- Embedding generation with `fastembed`
- Semantic search with cosine similarity
- Hybrid search implementation
- Smart indexing with mtime checks
- Commands: `search fts/semantic/hybrid`

### Phase 3: Graph Navigation (Week 5)

**Goal:** WikiLinks graph features.

**Deliverables:**
- WikiLink extraction and parsing
- Link resolution with multiple strategies
- Graph table implementation
- Backlinks/forward links queries
- Orphan detection
- Unresolved link tracking
- Commands: `graph backlinks/forward-links/orphans/unresolved/context`

### Phase 4: MCP stdio (Week 6)

**Goal:** Local MCP server for AI agents.

**Deliverables:**
- JSON-RPC 2.0 protocol implementation
- stdio transport (stdin/stdout)
- All MCP tool schemas
- Tool request handlers
- Error handling with proper codes
- Testing with Claude Desktop
- Mode: `--mcp`

### Phase 5: MCP HTTP (Week 7)

**Goal:** Remote MCP server.

**Deliverables:**
- Axum HTTP server setup
- Bearer token authentication
- IP address filtering
- JSON-RPC over HTTP POST
- Health check endpoint
- Graceful shutdown
- Mode: `--mcp-server`

### Phase 6: Optimization & Polish (Week 8)

**Goal:** Performance and UX improvements.

**Deliverables:**
- Parallel indexing optimization
- HNSW index for vectors (optional)
- Progress bars for long operations
- Vault conventions discovery
- Better error messages
- Integration tests
- Performance benchmarks
- Complete documentation

---

## Performance Targets

### Indexing Performance

| Metric | Target |
|--------|--------|
| Notes/sec (FTS only) | 100-200 |
| Notes/sec (with embeddings) | 20-50 |
| Parallel speedup (8 cores) | 3-5x |
| Memory usage (10k notes) | < 500 MB |

### Search Performance

| Operation | Target Latency |
|-----------|----------------|
| FTS search (10 results) | < 50ms |
| Semantic (no HNSW, 10k notes) | < 500ms |
| Semantic (with HNSW) | < 50ms |
| Hybrid search | < 200ms |
| Graph queries | < 100ms |

### Startup Performance

| Mode | Target |
|------|--------|
| CLI command | < 100ms |
| MCP server | < 500ms |
| Index staleness check | < 200ms |

---

## Security Considerations

### Local Security

1. **File Permissions:**
   - Config file: 0600 (user read/write only)
   - Database files: 0600
   - Ensure `.synapse/` not world-readable

2. **Token Storage:**
   - Never log authentication tokens
   - Support environment variables for tokens
   - Allow token file with secure permissions

### Remote MCP Security

1. **Authentication:**
   - Require bearer token for all requests
   - Generate random tokens (32+ bytes entropy)
   - Support token rotation

2. **Network Security:**
   - Default bind: 127.0.0.1 (localhost only)
   - IP allowlist enforcement
   - No CORS by default (single-origin)
   - Optional: Rate limiting per IP

3. **Production Deployment:**
   - Recommend reverse proxy (nginx) with TLS
   - Firewall rules for IP filtering
   - Monitor access logs
   - Never expose directly to internet

**Example Secure Setup:**
- Generate token: `openssl rand -hex 32`
- Run server: `arrowhead --mcp-server --bind 127.0.0.1:8080 --token $TOKEN`
- Use reverse proxy for TLS termination
- Configure firewall rules

---

## Open Questions & Future Enhancements

### Open Questions

1. **Model Distribution:**
   - Bundle default embedding model? (increases binary size)
   - Download on first run? (requires network)
   - User provides model path? (more flexible)

2. **Database Versioning:**
   - Include schema version in databases?
   - Migration system for future schema changes?
   - Backward compatibility strategy?

3. **Concurrent Access:**
   - Support multiple processes accessing same vault?
   - File locking for databases?
   - Read-only mode for safety?

### Future Enhancements

1. **Performance:**
   - SIMD acceleration for similarity calculations
   - Incremental indexing (watch mode)
   - Query result caching
   - Compression for vectors (quantization)

2. **Features:**
   - Export to various formats (JSON, CSV, HTML)
   - Backup/restore utilities
   - Vault statistics dashboard (web UI)
   - Plugin system for custom extractors
   - Multi-vault support

3. **Integrations:**
   - Raycast extension
   - Web UI for remote access
   - Browser extension
   - VS Code extension

4. **Advanced Search:**
   - Date range filtering
   - Regex search mode
   - Proximity search
   - Fuzzy matching

---

## Summary

This specification provides a blueprint for rewriting Synapse from Swift to Rust as **Arrowhead**:

✅ **Cross-platform CLI** (macOS + Linux)
✅ **Smart on-demand indexing** (mtime-based)
✅ **Dedicated vector storage** (LanceDB via optional feature)
✅ **Full search capabilities** (FTS, semantic, hybrid)
✅ **WikiLinks graph navigation**
✅ **Dual MCP modes** (stdio + HTTP)
✅ **Production-ready auth** (bearer tokens)
✅ **Clean architecture** (core library + CLI + MCP)
✅ **Obsidian compatible** (100% vault compatibility)
✅ **Performance focused** (parallel indexing, fast search)

Implementation proceeds in 8 phases (Phase 0 completed) over approximately 8 weeks, with each phase delivering working functionality. The result will be a powerful, standalone CLI for Obsidian vault indexing and search with AI integration via MCP.
