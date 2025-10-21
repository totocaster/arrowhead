//! Metadata extraction utilities.

use std::collections::BTreeSet;

use anyhow::Result;
use serde_json::Value;

use crate::{MetadataMap, NoteRecord};

/// Output of metadata extraction combining frontmatter and inline discoveries.
#[derive(Debug, Clone, Default)]
pub struct MetadataExtraction {
    /// Metadata fields as key/value pairs ready for persistence.
    pub metadata: MetadataMap,
    /// WikiLink targets discovered in the note body.
    pub wikilinks: Vec<String>,
    /// Inline tags extracted from content.
    pub tags: Vec<String>,
}

/// Parses notes and produces structured metadata for indexing.
#[derive(Debug, Default, Clone)]
pub struct MetadataExtractor;

impl MetadataExtractor {
    /// Create a new metadata extractor instance.
    pub fn new() -> Self {
        Self
    }

    /// Extract metadata from the supplied note.
    pub fn extract(&self, note: &NoteRecord) -> Result<MetadataExtraction> {
        let mut metadata = note.metadata.clone();

        let existing_tags_value = metadata.remove("tags");
        let mut tags = existing_tags_value
            .as_ref()
            .map(extract_tags_from_value)
            .unwrap_or_default();

        if let Some(aliases) = metadata.remove("aliases") {
            if let Some(normalised) = normalise_aliases_value(aliases) {
                metadata.insert("aliases".to_string(), normalised);
            }
        }

        let inline_tags = extract_inline_tags(&note.content);
        tags.extend(inline_tags.iter().cloned());
        let tags_vec: Vec<String> = tags.iter().cloned().collect();

        metadata.insert(
            "tags".to_string(),
            Value::Array(tags_vec.iter().cloned().map(Value::String).collect()),
        );

        let wikilinks = extract_wikilinks(&note.content);
        metadata.insert(
            "wikilinks".to_string(),
            Value::Array(wikilinks.iter().cloned().map(Value::String).collect()),
        );

        Ok(MetadataExtraction {
            metadata,
            wikilinks,
            tags: tags_vec,
        })
    }
}

fn extract_tags_from_value(value: &Value) -> BTreeSet<String> {
    let mut tags = BTreeSet::new();

    match value {
        Value::Null => {}
        Value::String(s) => {
            for part in split_tag_items(s) {
                if let Some(tag) = normalise_tag(part) {
                    tags.insert(tag);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                tags.extend(extract_tags_from_value(item));
            }
        }
        other => {
            if let Some(tag) = value_to_string(other) {
                tags.insert(tag);
            }
        }
    }

    tags
}

fn normalise_aliases_value(value: Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::String(s) => {
            let alias = s.trim();
            if alias.is_empty() {
                None
            } else {
                Some(Value::Array(vec![Value::String(alias.to_string())]))
            }
        }
        Value::Array(items) => {
            let mut cleaned = Vec::new();
            for item in items {
                match item {
                    Value::String(s) => {
                        let alias = s.trim();
                        if !alias.is_empty() {
                            cleaned.push(Value::String(alias.to_string()));
                        }
                    }
                    Value::Null => {}
                    other => cleaned.push(other),
                }
            }

            if cleaned.is_empty() {
                None
            } else {
                Some(Value::Array(cleaned))
            }
        }
        other => Some(Value::Array(vec![other])),
    }
}

fn split_tag_items(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .filter(|part| !part.trim().is_empty())
}

fn normalise_tag(value: &str) -> Option<String> {
    let tag = value.trim();
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(boolean) => Some(boolean.to_string()),
        other => Some(other.to_string()),
    }
}

fn extract_inline_tags(content: &str) -> BTreeSet<String> {
    let mut tags = BTreeSet::new();
    let chars: Vec<(usize, char)> = content.char_indices().collect();
    let total = chars.len();
    let mut idx = 0;

    while idx < total {
        let (_, ch) = chars[idx];
        if ch == '#' {
            let prev_is_boundary = if idx == 0 {
                true
            } else {
                let prev = chars[idx - 1].1;
                !prev.is_alphanumeric() && prev != '_' && prev != '-' && prev != '!' && prev != '['
            };

            if prev_is_boundary {
                let mut end = idx + 1;
                let mut tag = String::new();

                while end < total {
                    let c = chars[end].1;
                    if c.is_alphanumeric() || c == '-' || c == '_' {
                        tag.push(c);
                        end += 1;
                    } else {
                        break;
                    }
                }

                if !tag.is_empty() {
                    tags.insert(tag);
                    idx = end;
                    continue;
                }
            }
        }

        idx += 1;
    }

    tags
}

fn extract_wikilinks(content: &str) -> Vec<String> {
    let mut links = BTreeSet::new();
    let bytes = content.as_bytes();
    let mut cursor = 0;

    while let Some(relative_start) = content[cursor..].find("[[") {
        let start = cursor + relative_start;

        if start > 0 && bytes[start - 1] == b'!' {
            cursor = start + 2;
            continue;
        }

        let search_start = start + 2;
        if let Some(relative_end) = content[search_start..].find("]]") {
            let end = search_start + relative_end;
            let mut target = &content[search_start..end];
            if let Some((before_pipe, _)) = target.split_once('|') {
                target = before_pipe;
            }
            if let Some((before_hash, _)) = target.split_once('#') {
                target = before_hash;
            }
            let trimmed = target.trim();
            if !trimmed.is_empty() {
                links.insert(trimmed.to_string());
            }
            cursor = end + 2;
        } else {
            break;
        }
    }

    links.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::vault::{Vault, VaultConfig};

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("test-vault")
    }

    fn load_note(id: &str) -> NoteRecord {
        let vault =
            Vault::new(VaultConfig::new(fixture_root())).expect("fixture vault must initialise");
        vault
            .load_note(id)
            .unwrap_or_else(|_| panic!("expected note {id} to load"))
    }

    #[test]
    fn extracts_frontmatter_and_inline_tags() {
        let note = load_note("Photography Equipment");
        let extractor = MetadataExtractor::new();
        let extraction = extractor
            .extract(&note)
            .expect("metadata extraction succeeds");

        let category = extraction
            .metadata
            .get("category")
            .and_then(|value| value.as_str())
            .expect("category present");
        assert_eq!(category, "reference");

        let tags = extraction.tags;
        assert!(tags.contains(&"photography".to_string()));
        assert!(tags.contains(&"gear".to_string()));
        assert!(tags.contains(&"equipment".to_string()));
        assert!(tags.contains(&"reference".to_string()));

        let wikilinks = extraction.wikilinks;
        assert!(wikilinks.contains(&"Sigma 35mm Art".to_string()));
        assert!(wikilinks.contains(&"2024-01-15".to_string()));
    }

    #[test]
    fn ignores_embedded_links() {
        let note = load_note("Embeds Test");
        let extractor = MetadataExtractor::new();
        let extraction = extractor
            .extract(&note)
            .expect("metadata extraction succeeds");

        assert!(
            extraction
                .wikilinks
                .iter()
                .all(|link| !link.ends_with(".jpg")
                    && !link.ends_with(".png")
                    && !link.ends_with(".pdf"))
        );
    }

    #[test]
    fn captures_unicode_inline_tags() {
        let note = load_note("Many Tags Test");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("metadata extraction succeeds");

        assert!(extraction.tags.contains(&"測試標籤".to_string()));
        assert!(extraction.tags.contains(&"тег".to_string()));
    }
}
