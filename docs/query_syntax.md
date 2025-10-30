# Arrowhead Query Syntax

This document captures the planned extensions to Arrowhead search syntax so designers and contributors can review the surface area before implementation. The goal is to support richer boolean logic and first-class date filtering while keeping backwards compatibility with today’s keyword queries.

## Overview

- Boolean operators with explicit precedence, parenthesised grouping, and implicit AND handling.
- Typed date range filters that target both filesystem timestamps and front-matter fields.
- Extended literal handling (quoted phrases, escaped characters, URLs) required to make the parser robust.
- Conversion expectations for the query parser tests.
- Filters apply uniformly across FTS, semantic, and hybrid search modes.
- Field aliases and relative-date shorthands for faster authoring.

> **Status:** Implemented (Arrowhead Core v0.3) — keep this doc in sync as the syntax evolves.

## Boolean Expressions

### Operators & Precedence

- `NOT` > `AND` > `OR`
- Adjacent terms without an explicit operator are interpreted as `AND`.
- Parentheses may be nested to override default precedence.
- Operators are case-insensitive; the parser normalises them.
- `NOT` must appear alongside at least one positive clause (e.g. `foo NOT bar` is treated as `foo AND NOT bar`). Placing `NOT` directly inside an `OR` expression is not supported.

| Example | Meaning |
| --- | --- |
| `project plan AND status:active` | Both tokens must match. |
| `content:"status update" OR tags:weekly` | Either clause may match. |
| `NOT archived:true` | Exclude notes where `archived:true` appears. |
| `(owner:alice OR owner:bob) AND team:operations` | `team:operations` plus either owner. |

### Field Scoping

- `content:` targets the body (default scope when no field is given).
- Any other `field:value` pair is rewritten into the metadata column.
- Multiple fields can nest within boolean groups.
- Date filters (`modified:…`, `created:…`, etc.) act as additional constraints and must be combined with other terms using `AND` (placing them under `OR`/`NOT` is rejected).

## Date Filters

### Absolute Ranges

```
modified:2023-01-01..2023-12-31
created:>=2024-02-15
review_due:2024-03-01..   # open upper bound
```

- ISO-8601 date or datetime values.
- Missing endpoints imply open-ended ranges.
- Inclusive bounds by default.

### Relative Shorthands

```
modified:past7d
created:past30d
due:next2w
modified:today
created:yesterday
m:thisweek
c:lastmonth
```

- `pastNd`, `nextNw`, `nextNm` style tokens (N ≥ 1).
- Named helpers: `today`, `yesterday`, `thisweek`, `lastweek`, `thismonth`, `lastmonth`.
- Evaluated relative to the current time at query execution.

### Metadata Dates

- Front-matter dates (e.g., `date:` or `review_due:`) are allowed.
- Implementation will populate indexed numeric columns alongside the raw JSON to keep queries fast.
- Short aliases are available: `m:` for `modified:` and `c:` for `created:`.
- Date-only queries (e.g., `modified:past7d`) are valid and return notes sorted by modification time.

### Field Aliases

- `m:` → `modified:`
- `c:` → `created:`

Field aliases are only expanded when they are used as `field:value` pairs.

## Syntax Reference

```
query        := expression
expression   := term ( (OR|AND) term )*
term         := (NOT)? factor
factor       := primary
primary      := literal | phrase | field_expr | "(" expression ")"
field_expr   := identifier ":" value | identifier comparison value | identifier ":" range
range        := value ".." value | value ".." | ".." value
comparison   := ">=" | "<=" | ">" | "<"
literal      := unquoted token
phrase       := quoted string with escapes
value        := literal | phrase | relative_date
relative_date:= "past" number ("d"|"w"|"m") | "next" number ("d"|"w"|"m")
```

## Parser Test Coverage

Parser and converter unit tests must exhaustively exercise:

- All operator permutations (`NOT`, `AND`, `OR`) with and without parentheses.
- Implicit AND resolution for whitespace-separated terms.
- Nested groups containing fielded clauses and mixed literals.
- Absolute date ranges (`start..end`, `start..`, `..end`) across filesystem fields and metadata fields.
- Relative date tokens (`past7d`, `next2w`) and error paths for invalid suffixes or zero lengths.
- Comparison operators (`>=`, `<`, etc.) applied to both timestamps and general metadata.
- Quoted phrases with escaped quotes, hyphenated tokens, URLs that must bypass escaping.
- Failure cases (unmatched parentheses, malformed ranges, unknown comparison tokens) yielding descriptive errors.

Integration tests should verify that the parser output drives FTS queries and SQL filters correctly once implementation lands, especially for combined boolean/date expressions.

## Open Decisions

1. Decide whether to add further relative-date shorthands (e.g., `thisyear`, `nextquarter`) beyond the current set.
