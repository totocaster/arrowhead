# Arrowhead Roadmap

## Completed
- ✅ Boolean/query parser overhaul with NOT support, field aliases (`m:`/`c:`), and relative date shorthands (`today`, `thisweek`, etc.)
- ✅ Filter-only searches across FTS, semantic, and hybrid modes with clear "Filter match" reasoning
- ✅ Date metadata indexed via `metadata_dates` table with integration tests and documentation updates

## In Progress
- 🔄 Search usability refinements (e.g., potential score display polish for filter-only queries)

## Up Next
1. Graph service enhancements and remaining MCP transport polish (Phase 3+ from rewrite spec)
2. Optional query explain/debug output to aid complex searches if demand warrants
3. Evaluate adding broader relative helpers (`thisyear`, `past3m`, `past6m`) based on user feedback
4. Continue semantic/FTS resilience work (error messaging, config surfacing) ahead of daemon hardening

---
Last updated: 2025-10-30
