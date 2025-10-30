# Boolean & Date Range Search Implementation Plan

This plan tracks the engineering work required to deliver richer boolean search and first-class date filters in Arrowhead. It reflects the settled decisions:

- Relative-date tokens (`past7d`, `next2w`, etc.) **are supported**.
- Additional indexed columns for metadata dates are acceptable to keep queries fast.
- Proximity/`NEAR` syntax is **not** part of this iteration.

## Phase 0 – Prep & Validation

1. Finalise syntax reference (`docs/query_syntax.md`) to match the decisions above (remove proximity, confirm relative-date vocabulary).
2. Audit existing schema for date fields; identify the metadata keys that will receive indexed numeric columns (initially `date`, `review_due`, extendable).
3. Capture any edge-case requirements from CLI/daemon surfaces (error messages, logging expectations).

## Phase 1 – Parser Infrastructure

1. Introduce a new module (e.g., `crates/arrowhead-core/src/query`) that:
- Tokenises the query string (fields, operators, quoted strings, range markers).
- Produces an AST capturing boolean logic, field filters, comparisons, and range expressions.
- Normalises tokens (operator casing, whitespace) and rejects malformed input with actionable errors.
   - Date-range filters will be treated as conjunction-only constraints (attempting to place them under `OR`/`NOT` surfaces an error) to keep the SQL execution tractable.
- **Status:** Completed — parser now supports field aliases, expanded relative shorthands, filter-only queries, and pulls NOT clauses into exclusion sets (2025-10-30).
2. Extend AST nodes to emit:
   - An FTS-compatible string representing the content/metadata search clause.
   - A `QueryFilters` struct containing structured constraints (filesystem date ranges, metadata date filters, comparisons).
3. Write exhaustive unit tests covering:
   - Operator precedence, nested parentheses, implicit AND handling.
   - Absolute and relative date ranges (`start..end`, `start..`, `..end`, `past7d`, etc.).
   - Comparison operators (`>=`, `<`, `>`), invalid tokens, unterminated strings, and error pathways.
   - Regression fixtures mapping sample queries to expected AST + filter outputs.

## Phase 2 – Index & Schema Enhancements

1. Add migration steps in `crates/arrowhead-core/src/sqlite.rs` to:
   - Create numeric columns (or a companion table) for metadata date fields, stored as microseconds since epoch.
   - Ensure indexes exist on `notes.file_modified_at`, `notes.created_at`, and each new metadata date column.
2. Update `Indexer::upsert_note` to populate the new columns when extracting metadata:
   - Parse ISO strings and relative formats in front-matter during indexing.
   - Persist raw JSON alongside numeric copies so existing behaviour remains intact.
3. Add migration tests to confirm schema upgrades succeed on pre-existing databases.

## Phase 3 – Search Execution

1. Update `SearchService::search_fts` to:
   - Invoke the new parser.
   - Pass the generated FTS string plus `QueryFilters` to the database layer.
2. Add a new `IndexDatabase::search_with_filters`:
   - Compose the `notes_fts MATCH ?` clause with extra `WHERE` predicates for filesystem and metadata date ranges.
   - Bind parameters safely; support open-ended ranges and comparison operators.
3. Ensure semantic and hybrid modes honour the same filters after scoring (post-filter results as needed).
4. Expand integration tests (using the fixture vault) to cover:
   - date-only filters,
   - date + metadata boolean combos,
   - relative-date queries,
   - invalid queries yielding informative errors.

## Phase 4 – CLI & Documentation

1. Update `crates/arrowhead-cli/src/commands/search.rs` help text and error handling to reflect the new syntax.
2. Refresh README examples and add a “Query grammar” section linking to the dedicated syntax doc.
3. Document any operational notes (e.g., schema migration version bumps) in `docs/feature_development_guide.md` or `docs/roadmap.md` if appropriate.

## Phase 5 – Verification

1. Run `cargo fmt`, `cargo check`, `cargo clippy --all-targets -- -D warnings`, and targeted `cargo test` suites.
2. Capture verification notes, update docs if new behaviours emerge, and stage changes for review.
