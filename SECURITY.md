# Security Policy

Arrowhead ships semantic search, indexing, and MCP transports that interact with your personal Obsidian vault. Please review the guidance below before deploying the tooling in sensitive environments.

## Reporting a Vulnerability

- Use GitHub’s “Report a vulnerability” workflow (Security Advisories) or reach out privately to the repository owner (@totocaster) using the contact details listed on their GitHub profile.
- Include reproduction steps, impacted components (CLI, daemon, MCP stdio, MCP HTTP), and any relevant logs or stack traces.

## MCP Transport Considerations

- **Authentication & network exposure**: The HTTP transport should stay behind a trusted reverse proxy. Always require bearer tokens (default) or carefully scoped link tokens, and restrict inbound traffic with the CIDR allow-list.
- **Request saturation**: Both stdio and HTTP transports use bounded queues.
- **Tool surface**: MCP handlers expose note CRUD, graph exploration, and vault discovery endpoints. Compromised MCP clients can read or mutate personal notes in bulk. Grant access only to trusted agent runtimes.

## Local Vault Integrity

Arrowhead can create, update, and delete notes through both the CLI and MCP tools. Maintain a separate backup strategy before experimenting with automation—versioning the vault in a Git repository is a practical baseline.

## Dependency Updates

Arrowhead depends on SQLite, sqlite-vec, fastembed, tokio, and hf-hub. Keep the toolchain current and plan for regular dependency audits (`cargo audit`, `cargo deny`) to stay ahead of upstream security advisories.
