# Arrowhead Tests

This directory contains test infrastructure for the Arrowhead project.

## Directory Structure

```
tests/
├── README.md                 # This file
├── fixtures/                 # Test data and vaults
│   ├── README.md            # Detailed fixture documentation
│   └── test-vault/          # Complete Obsidian vault for testing
│       ├── .obsidian/       # Obsidian configuration
│       ├── Attachments/     # Attachment directory
│       ├── *.md             # 20 test notes
│       └── .gitignore       # Ignores .arrowhead/ directory
└── integration/             # Integration tests (tracked on the roadmap)
```

## Test Vault

The `fixtures/test-vault` directory contains a complete, realistic Obsidian vault designed for testing all Arrowhead features.

**Key Features:**
- 20 markdown notes covering happy paths and edge cases
- Obsidian configuration files (.obsidian/)
- WikiLinks graph with ~50+ links
- Broken links, orphan notes, circular references
- Unicode content, special characters
- Various metadata patterns
- No PII (all data anonymized)

See [`fixtures/README.md`](fixtures/README.md) for complete documentation.

## Using Test Vault in Rust Tests

### Basic Setup

```rust
use std::path::PathBuf;

fn test_vault_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("test-vault")
}

#[test]
fn test_vault_reading() {
    let vault_path = test_vault_path();
    let vault = Vault::new(VaultConfig {
        vault_path,
        attachments_folder: "Attachments".to_string(),
    }).unwrap();

    let note = vault.get_note("2024-01-15").unwrap();
    assert_eq!(note.metadata.get("category"), Some(&"journal".into()));
}
```

### Using Temporary Directories for Indexing

The test vault itself should remain read-only. Create indexes in temporary directories:

```rust
use tempfile::TempDir;

#[test]
fn test_indexing() {
    let vault_path = test_vault_path();
    let temp_dir = TempDir::new().unwrap();
    let index_path = temp_dir.path().join("index.db");
    let vectors_path = temp_dir.path().join("vectors");

    let indexer = Indexer::new(vault_path, index_path, vectors_path).unwrap();
    let stats = indexer.index_all().await.unwrap();

    assert!(stats.indexed >= 20);
    assert_eq!(stats.errors, 0);

    // temp_dir automatically cleaned up on drop
}
```

### Common Test Patterns

#### Testing Metadata Extraction

```rust
#[test]
fn test_frontmatter_variations() {
    let vault_path = test_vault_path();
    let vault = Vault::new(vault_path).unwrap();

    // No frontmatter
    let note = vault.get_note("Edge Case - No Frontmatter").unwrap();
    assert!(note.metadata.is_empty());

    // Complex metadata
    let note = vault.get_note("Complex Metadata Types").unwrap();
    assert_eq!(note.metadata["rating"], 4.5);
    assert_eq!(note.metadata["published"], true);
}
```

#### Testing WikiLink Extraction

```rust
#[test]
fn test_wikilink_extraction() {
    let vault_path = test_vault_path();
    let vault = Vault::new(vault_path).unwrap();

    let note = vault.get_note("2024-01-15").unwrap();
    let links = extract_wikilinks(&note.content);

    assert!(links.contains("Photography Equipment"));
    assert!(links.contains("Sarah Chen"));
}
```

#### Testing Graph Operations

```rust
#[test]
fn test_graph_backlinks() {
    let vault_path = test_vault_path();
    // ... setup indexer and index vault ...

    let graph = Graph::new(&index_path).unwrap();
    let backlinks = graph.get_backlinks("Photography Equipment").unwrap();

    assert!(backlinks.contains(&"2024-01-15".to_string()));
    assert!(backlinks.len() >= 3);
}

#[test]
fn test_orphan_detection() {
    let graph = Graph::new(&index_path).unwrap();
    let orphans = graph.find_orphans().unwrap();

    assert!(orphans.contains(&"Orphan Note".to_string()));
    assert_eq!(orphans.len(), 1);
}

#[test]
fn test_unresolved_links() {
    let graph = Graph::new(&index_path).unwrap();
    let unresolved = graph.find_unresolved_links(None).unwrap();

    assert!(unresolved.contains_key("This Note Does Not Exist"));
    assert!(unresolved.contains_key("Another Missing Note"));
}
```

#### Testing Search

```rust
#[test]
fn test_fts_search() {
    let search = Search::new(&index_path, &vectors_path).unwrap();

    // Field:value search
    let results = search.search_fts("category:journal photography", 10, 0).unwrap();
    assert!(results.iter().any(|r| r.id == "2024-01-15"));

    // Tag search
    let results = search.search_fts("tags:photography", 10, 0).unwrap();
    assert!(results.len() >= 2);
}

#[test]
fn test_semantic_search() {
    let search = Search::new(&index_path, &vectors_path).unwrap();
    let embeddings = OnnxEmbeddings::new().unwrap();

    let results = search.search_semantic(
        "camera equipment",
        &embeddings,
        0.7,
        5
    ).await.unwrap();

    assert!(results.iter().any(|r| r.id.contains("Equipment")));
}
```

For shell-driven smoke tests, the CLI exposes `--format ids`/`--format paths` on `arrowhead search`, and `arrowhead graph` mirrors `--format ids` on backlinks, forward-links, orphans, and unresolved listings. Semantic-only matches display `"N/A"` in the BM25 column when no lexical rank exists.

## Test Organization

### Unit Tests

Place unit tests alongside implementation code:

```rust
// crates/arrowhead-core/src/vault.rs

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter() {
        // Test specific functions
    }
}
```

### Integration Tests

Place integration tests in `tests/integration/`:

```
tests/integration/
├── indexing_tests.rs
├── search_tests.rs
├── graph_tests.rs
└── mcp_tests.rs
```

Example integration test:

```rust
// tests/integration/indexing_tests.rs

use arrowhead_core::{Vault, Indexer};
use std::path::PathBuf;
use tempfile::TempDir;

fn test_vault_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("test-vault")
}

#[tokio::test]
async fn test_full_vault_indexing() {
    let vault_path = test_vault_path();
    let temp_dir = TempDir::new().unwrap();

    let indexer = Indexer::new(
        vault_path,
        temp_dir.path().join("index.db"),
        temp_dir.path().join("vectors"),
    ).unwrap();

    let stats = indexer.index_all(None).await.unwrap();

    assert_eq!(stats.total_notes, 20);
    assert!(stats.indexed > 0);
    assert_eq!(stats.errors, 0);
}
```

## Running Tests

```bash
# Run all tests
cargo test

# Run specific test
cargo test test_vault_reading

# Run integration tests only
cargo test --test integration

# Run with output
cargo test -- --nocapture

# Run with specific thread count
cargo test -- --test-threads=1
```

## Test Coverage

The test vault provides 100% coverage for:

- ✅ YAML frontmatter parsing (all variations)
- ✅ WikiLink extraction (all formats)
- ✅ Tag extraction (frontmatter + inline + Unicode)
- ✅ Graph operations (backlinks, forward links, orphans, broken links)
- ✅ Metadata types (strings, numbers, booleans, arrays, objects, dates)
- ✅ Edge cases (no frontmatter, empty frontmatter, special chars, Unicode)
- ✅ Content variations (minimal, large, code blocks, formatting)
- ✅ Search scenarios (FTS, semantic, hybrid)

See [`fixtures/README.md`](fixtures/README.md) for detailed coverage matrix.

## Best Practices

1. **Use temporary directories** for any file writes
2. **Never modify test vault** - keep it read-only
3. **Clean up after tests** - use TempDir for auto-cleanup
4. **Parallel test safety** - avoid shared state
5. **Descriptive test names** - clearly indicate what's being tested
6. **Test both happy and edge cases** - vault provides both

## Adding New Test Cases

1. Add note to `fixtures/test-vault/`
2. Document in `fixtures/README.md`
3. Create corresponding test in appropriate file
4. Update coverage matrix
5. Verify with `cargo test`

## Continuous Integration

Tests are designed to work in CI environments:

```yaml
# .github/workflows/test.yml
- name: Run tests
  run: cargo test --all-features
```

No external dependencies required - everything is self-contained in the test vault.
