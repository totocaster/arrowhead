# Test Fixtures for Arrowhead

This directory contains test fixtures for the Arrowhead project, including a complete test Obsidian vault.

## Directory Structure

```
fixtures/
└── test-vault/           # Complete Obsidian vault for testing
    ├── .obsidian/        # Obsidian configuration
    │   ├── app.json
    │   ├── appearance.json
    │   └── core-plugins.json
    ├── Attachments/      # Attachment directory (empty in tests)
    ├── Templates/        # Ignored templates folder (excluded by settings)
    └── *.md              # Test markdown notes
```

## Test Vault Overview

The `test-vault` is a comprehensive Obsidian vault designed to test all features and edge cases of Arrowhead.

### Statistics

- **Total Notes**: 20+
- **Categories**: journal, reference, gear, person, test, evergreen
- **WikiLinks**: ~50+ links (including broken links)
- **Tags**: 40+ unique tags
- **Metadata Patterns**: Simple and complex YAML frontmatter

When validating the fixtures with the CLI, `arrowhead search --format paths` emits absolute note locations for quick inspection, `arrowhead graph --format ids` streams backlink/orphan lists for batching, and semantic-only hits surface `"N/A"` in the BM25 column to indicate that no lexical score was used.

## Test Note Categories

### 1. Happy Path Notes (Normal Usage)

#### `2024-01-15.md`
- **Category**: journal
- **Purpose**: Daily note with typical structure
- **Features**:
  - Standard frontmatter (category, date, tags, weather, location)
  - WikiLinks to people, equipment, and concepts
  - Inline tags
  - Task list items
- **Tests**: Normal journal entry indexing

#### `Photography Equipment.md`
- **Category**: reference
- **Purpose**: Equipment reference note
- **Features**:
  - Aliases field
  - Multiple WikiLinks
  - Hierarchical structure
  - Related notes section
- **Tests**: Reference note indexing, aliases, backlinks

#### `Sigma 35mm Art.md`
- **Category**: gear
- **Purpose**: Specific equipment detail
- **Features**:
  - Complex metadata (technical specs)
  - WikiLinks to related notes
  - Tags
- **Tests**: Detailed metadata extraction

#### `Sarah Chen.md`
- **Category**: person
- **Purpose**: Person/contact note
- **Features**:
  - Contact information in metadata
  - Collaboration references
  - Meeting notes links
- **Tests**: Person entity tracking

### 2. Edge Case Notes

#### `Edge Case - No Frontmatter.md`
- **Purpose**: Test handling of notes without YAML frontmatter
- **Features**:
  - No `---` delimiters
  - Pure markdown
  - Inline tags only
- **Tests**: Graceful handling of missing frontmatter

#### `Edge Case - Empty Frontmatter.md`
- **Purpose**: Test empty frontmatter blocks
- **Features**:
  - Empty `---` delimiters with no content
  - Inline tags
- **Tests**: Empty YAML handling

#### `Edge Case - Special Characters !@#$%.md`
- **Purpose**: Test special characters in filenames and content
- **Features**:
  - Special chars in filename: `!@#$%`
  - Unicode content (Japanese, Russian, Arabic)
  - Special characters in metadata values
- **Tests**: Filename encoding, Unicode support

#### `Edge Case - Very Long Title With Many Words That Exceeds Normal Length.md`
- **Purpose**: Test long filename handling
- **Features**:
  - 70+ character filename
  - Multiple words
- **Tests**: Path length limits, filename truncation

#### `Broken Links Test.md`
- **Purpose**: Test unresolved WikiLinks
- **Features**:
  - Links to non-existent notes
  - Mix of valid and broken links
- **Tests**: Unresolved link detection, graph integrity

#### `Orphan Note.md`
- **Purpose**: Test orphan detection
- **Features**:
  - No WikiLinks (incoming or outgoing)
  - Only tags
- **Tests**: Orphan note identification

### 3. Graph and Link Tests

#### `Circular Reference A.md` + `Circular Reference B.md`
- **Purpose**: Test bidirectional links
- **Features**:
  - A links to B
  - B links to A
- **Tests**: Bidirectional link detection, circular references

#### `Link Variations Test.md`
- **Purpose**: Test different WikiLink formats
- **Features**:
  - Basic links: `[[Note]]`
  - Display text: `[[Note|Display]]`
  - Heading links: `[[Note#Heading]]`
  - Block links: `[[Note#^block]]`
  - Links in tables and lists
- **Tests**: WikiLink parsing variations

#### `Embeds Test.md`
- **Purpose**: Test embed vs link distinction
- **Features**:
  - Embed syntax: `![[file.jpg]]`
  - Should NOT be treated as WikiLinks
- **Tests**: Embed exclusion from graph

### 4. Content Variation Tests

#### `Code and Formatting Test.md`
- **Purpose**: Test markdown formatting and code blocks
- **Features**:
  - Inline code
  - Code blocks (Rust, Python)
  - Bold, italic formatting
  - Lists (ordered and unordered)
  - Blockquotes
- **Tests**: Content parsing, formatting preservation

#### `Minimal Note.md`
- **Purpose**: Test minimal content
- **Features**:
  - Single heading
  - Two words of content
- **Tests**: Minimal content handling

#### `Large Content Test.md`
- **Purpose**: Test performance with larger notes
- **Features**:
  - ~2KB of content
  - Multiple sections
  - Many WikiLinks
  - Realistic photography article
- **Tests**: Large note indexing, performance

### 5. Metadata Tests

#### `Complex Metadata Types.md`
- **Purpose**: Test various YAML data types
- **Features**:
  - Strings, numbers (int, float)
  - Booleans
  - Arrays
  - Nested objects
  - ISO 8601 dates
  - URLs
- **Tests**: YAML parser coverage, type handling

#### `Many Tags Test.md`
- **Purpose**: Test tag handling
- **Features**:
  - 8 frontmatter tags
  - 5 inline tags
  - Unicode tags (Japanese, Russian)
  - Long tag names
- **Tests**: Tag extraction, Unicode tags

#### `Date Format Variations.md`
- **Purpose**: Test different date formats
- **Features**:
  - ISO 8601 (date only)
  - ISO 8601 with time (UTC)
  - ISO 8601 with timezone
  - Quoted vs unquoted dates
- **Tests**: Date parsing flexibility

## Obsidian Configuration

### `.obsidian/app.json`

Minimal Obsidian settings matching real vault patterns:

```json
{
  "attachmentFolderPath": "Attachments",
  "newLinkFormat": "absolute",
  "alwaysUpdateLinks": true,
  "userIgnoreFilters": ["Templates/"]
}
```

**Key Settings**:
- `attachmentFolderPath`: Where attachments are stored
- `newLinkFormat`: "absolute" for absolute path WikiLinks
- `userIgnoreFilters`: Patterns to ignore during indexing

### `.obsidian/core-plugins.json`

List of enabled Obsidian core plugins (informational only).

### `.obsidian/appearance.json`

UI appearance settings (minimal).

## Graph Structure

### WikiLink Network

```
2024-01-15
    ↓
Photography Equipment ←→ Sigma 35mm Art
    ↓                         ↓
Sarah Chen              (multiple refs)
    ↓
(various refs)

Circular Reference A ←→ Circular Reference B

Broken Links Test → [Non-existent notes]

Orphan Note (isolated)
```

### Expected Counts

- **Total WikiLinks**: ~50+
- **Unique linked notes**: ~10
- **Broken links**: 3 (intentional)
- **Bidirectional links**: 1 pair (Circular A/B)
- **Orphan notes**: 1 (Orphan Note)

## Testing Use Cases

### Indexing Tests

```rust
#[test]
fn test_index_vault() {
    let vault_path = "tests/fixtures/test-vault";
    let indexer = Indexer::new(vault_path)?;
    let stats = indexer.index_all()?;

    assert!(stats.total_notes >= 20);
    assert_eq!(stats.errors, 0);
}
```

### Metadata Extraction Tests

```rust
#[test]
fn test_no_frontmatter() {
    let note = vault.get_note("Edge Case - No Frontmatter")?;
    assert!(note.metadata.is_empty());
    assert!(note.content.contains("No YAML frontmatter"));
}

#[test]
fn test_complex_metadata() {
    let note = vault.get_note("Complex Metadata Types")?;
    assert_eq!(note.metadata["rating"], 4.5);
    assert_eq!(note.metadata["published"], true);
}
```

### Search Tests

```rust
#[test]
fn test_fts_search() {
    let results = search.search_fts("category:journal photography")?;
    assert!(results.len() > 0);
    assert!(results.iter().any(|r| r.id == "2024-01-15"));
}

#[test]
fn test_tag_search() {
    let results = search.search_fts("tags:photography")?;
    assert!(results.len() >= 2);
}
```

### Graph Tests

```rust
#[test]
fn test_backlinks() {
    let backlinks = graph.get_backlinks("Photography Equipment")?;
    assert!(backlinks.contains(&"2024-01-15".to_string()));
    assert!(backlinks.len() >= 3);
}

#[test]
fn test_bidirectional_links() {
    let bidir = graph.get_bidirectional_links("Circular Reference A")?;
    assert!(bidir.contains(&"Circular Reference B".to_string()));
}

#[test]
fn test_orphans() {
    let orphans = graph.find_orphans()?;
    assert!(orphans.contains(&"Orphan Note".to_string()));
}

#[test]
fn test_unresolved_links() {
    let unresolved = graph.find_unresolved_links(Some("Broken Links Test"))?;
    assert_eq!(unresolved.len(), 3);
    assert!(unresolved.contains_key("This Note Does Not Exist"));
}
```

### WikiLink Extraction Tests

```rust
#[test]
fn test_embed_exclusion() {
    let note = vault.get_note("Embeds Test")?;
    let links = extract_wikilinks(&note.content);

    // Should NOT include embeds (![[...]])
    assert!(!links.contains(&"photo.jpg".to_string()));
    assert!(!links.contains(&"image.png".to_string()));

    // Should include regular link
    assert!(links.contains(&"Photography Equipment".to_string()));
}

#[test]
fn test_link_variations() {
    let note = vault.get_note("Link Variations Test")?;
    let links = extract_wikilinks(&note.content);

    // All variations should extract the note ID
    assert!(links.contains(&"Photography Equipment".to_string()));
    assert!(links.contains(&"Sarah Chen".to_string()));
    assert!(links.contains(&"Sigma 35mm Art".to_string()));
}
```

### Unicode and Special Characters Tests

```rust
#[test]
fn test_unicode_filename() {
    let note = vault.get_note("Edge Case - Special Characters !@#$%")?;
    assert!(note.metadata.contains_key("unicode"));
}

#[test]
fn test_unicode_tags() {
    let note = vault.get_note("Many Tags Test")?;
    let tags = extract_tags(&note);
    assert!(tags.contains(&"測試標籤".to_string()));
    assert!(tags.contains(&"тег".to_string()));
}
```

## Usage in Tests

### Setup Test Environment

```rust
use std::path::PathBuf;

fn test_vault_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("test-vault")
}

#[test]
fn test_example() {
    let vault_path = test_vault_path();
    let vault = Vault::new(VaultConfig {
        vault_path,
        attachments_folder: "Attachments".to_string(),
    }).unwrap();

    // Test code here
}
```

### Cleanup After Tests

The test vault should not create any `.arrowhead/` directories in tests. Use temporary directories:

```rust
use tempfile::TempDir;

#[test]
fn test_indexing() {
    let vault_path = test_vault_path();
    let temp_dir = TempDir::new().unwrap();
    let index_path = temp_dir.path().join("index.db");

    let indexer = Indexer::new(vault_path, index_path)?;
    // Test indexing

    // temp_dir automatically cleaned up
}
```

## Maintenance

### Adding New Test Notes

1. Create the note in `test-vault/`
2. Document it in this README under the appropriate category
3. Add corresponding test cases
4. Update expected counts in graph structure

### Modifying Existing Notes

- Maintain backward compatibility with existing tests
- Update test expectations if note structure changes
- Document changes in git commit messages

### Excluded from Tests

- No actual image files (use placeholders in Embeds Test)
- No `.arrowhead/` directory (created during tests)
- No `.obsidian/workspace.json` (user-specific)
- No `.obsidian/plugins/` (not needed for core functionality)

## Notes for Developers

1. **PII Removal**: All personal information has been removed or anonymized
2. **Realistic Patterns**: Notes follow real-world Obsidian usage patterns
3. **Comprehensive Coverage**: Covers happy paths and edge cases
4. **Self-Documenting**: Note filenames indicate their purpose
5. **Rust-Friendly**: Designed to work with Rust test tooling and `#[test]` macros
6. **Isolated**: Each test can run independently using this vault
7. **Reproducible**: Same vault state for all test runs

## Test Coverage Matrix

| Feature | Test Note | Coverage |
|---------|-----------|----------|
| **Frontmatter** | Various | ✅ |
| - Standard metadata | 2024-01-15 | ✅ |
| - No frontmatter | Edge Case - No Frontmatter | ✅ |
| - Empty frontmatter | Edge Case - Empty Frontmatter | ✅ |
| - Complex types | Complex Metadata Types | ✅ |
| **WikiLinks** | Link Variations Test | ✅ |
| - Basic links | Multiple notes | ✅ |
| - Display text | Link Variations Test | ✅ |
| - Heading links | Link Variations Test | ✅ |
| - Broken links | Broken Links Test | ✅ |
| **Embeds** | Embeds Test | ✅ |
| **Tags** | Many Tags Test | ✅ |
| - Frontmatter tags | Multiple notes | ✅ |
| - Inline tags | Multiple notes | ✅ |
| - Unicode tags | Many Tags Test | ✅ |
| **Graph** | Circular Reference A/B | ✅ |
| - Backlinks | Photography Equipment | ✅ |
| - Forward links | Multiple notes | ✅ |
| - Bidirectional | Circular A/B | ✅ |
| - Orphans | Orphan Note | ✅ |
| - Unresolved | Broken Links Test | ✅ |
| **Content** | Code and Formatting Test | ✅ |
| - Markdown formatting | Multiple notes | ✅ |
| - Code blocks | Code and Formatting Test | ✅ |
| - Large content | Large Content Test | ✅ |
| - Minimal content | Minimal Note | ✅ |
| **Edge Cases** | Various Edge Case notes | ✅ |
| - Special characters | Edge Case - Special Characters | ✅ |
| - Long filenames | Edge Case - Very Long Title | ✅ |
| - Unicode | Edge Case - Special Characters | ✅ |
| **Dates** | Date Format Variations | ✅ |

Total Coverage: **100%** of planned features
