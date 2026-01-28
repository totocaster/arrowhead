# MCP Transport Guidance

## stdio vs HTTP
- `arrowhead --mcp`: newline-delimited JSON-RPC over stdin/stdout. Best for local IDEs (Claude Code, Codex CLI) and avoids network auth setup.
- `arrowhead --mcp-server`: HTTP JSON-RPC on `POST /rpc`, plus `GET /health` readiness checks. Use when serving remote agents or Claude.app over the network.

## Authentication
| Option | Description |
|--------|-------------|
| `--token <raw>` | Provide bearer token directly (also `ARROWHEAD_MCP_TOKEN`). Raw value never stored. |
| `--token-file <path>` | Read token from file (chmod 600). |
| `--token-hash <sha256>` | Provide hashed token if raw cannot be shared. |
| `--generate-token` | Prints one-time secret, stores only hash; copy immediately. |

`--auth-mode bearer` (default) expects `Authorization: Bearer <token>`. `--auth-mode link-token` lets clients hit `/rpc/<token>` when headers aren’t possible—wrap with HTTPS or a secure tunnel.

## Network Policy
- Default allowlist: `127.0.0.0/8` + `::1/128`. Extend with `--allow 10.0.0.0/8` or `--allow-file ranges.txt` when exposing beyond localhost.
- Bind address defaults to `127.0.0.1:3911`; override via `--bind host:port` or `ARROWHEAD_MCP_BIND`.
- Combine with Tailscale/ssh tunnels instead of opening public ports whenever possible.

## Reverse Proxy Pattern
```
arrowhead --mcp-server --bind 127.0.0.1:3911 --token $TOKEN --allow 127.0.0.0/8
# nginx/caddy/traefik terminates TLS and forwards to 3911.
```
- Keep Arrowhead allowlist scoped to loopback; rely on the proxy for external access control.
- Layer OAuth/OIDC (oauth2-proxy, Cloudflare Access) when sharing with multiple users.

## Rate Limiting & Backpressure
Both transports share the same bounded worker pool. If you hit queue saturation you’ll see `RateLimited` errors—back off and inspect daemon load.

## Quick Test Matrix
| Test | stdio | HTTP |
|------|-------|------|
| Liveness | send `{"jsonrpc":"2.0","id":1,"method":"mcp.protocol.initialize","params":{...}}` | `GET /health` expects `200 OK` |
| Auth | ensure invalid token returns `401/403` | ensure missing header gets `401` |
| Search | `mcp.search.hybrid` sample call | `POST /rpc` with same payload |

Log authentication + allowlist changes in summaries to keep auditors informed.
