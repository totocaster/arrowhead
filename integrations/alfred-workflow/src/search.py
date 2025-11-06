#!/usr/bin/env python3
"""
Script Filter entrypoint for the Arrowhead Alfred workflow.

Reads the query supplied by Alfred, executes the Arrowhead CLI with the
configured search mode, and maps the JSON response into Alfred Script Filter
items. Designed to run with the system Python (`/usr/bin/python3`) and to
require no third-party dependencies.
"""

from __future__ import annotations

import ast
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional

# Alfred workflow variables (fall back to sane defaults if unset).
DEFAULT_SEARCH_MODE = "hybrid"
DEFAULT_RESULT_LIMIT = 15
DEFAULT_PRIMARY_EDITOR = "obsidian"
ALTERNATE_EDITOR = {
    "obsidian": "default",
    "default": "obsidian",
}
ADDITIONAL_PATH_DIRS = [
    Path.home() / ".local" / "bin",
    Path.home() / ".local" / "sbin",
    Path.home() / ".cargo" / "bin",
    Path.home() / "bin",
    Path("/opt/homebrew/bin"),
    Path("/opt/homebrew/sbin"),
    Path("/usr/local/opt/arrowhead/bin"),
    Path("/usr/local/bin"),
    Path("/usr/local/sbin"),
]


@dataclass
class WorkflowConfig:
    search_mode: str
    result_limit: int
    primary_editor: str
    cli_path: str
    vault_path: Optional[Path]


def main() -> None:
    query = " ".join(sys.argv[1:]).strip()
    config = load_workflow_config()

    if not query:
        respond(render_placeholder_items(config))
        return

    cli_payload = run_arrowhead_search(query, config)
    if isinstance(cli_payload, ErrorPayload):
        respond(error_items(cli_payload))
        return

    items = render_search_items(cli_payload.results, config)
    respond(items if items["items"] else no_results_items(query, config))


def respond(payload: Dict[str, Any]) -> None:
    """Emit JSON payload expected by Alfred."""
    sys.stdout.write(json.dumps(payload, ensure_ascii=False))


def render_placeholder_items(config: WorkflowConfig) -> Dict[str, Any]:
    """Show guidance before the user starts typing."""
    mode_label = config.search_mode.upper()
    editor_label = describe_editor(config.primary_editor)
    return {
        "items": [
            {
                "title": "Search Arrowhead notes",
                "subtitle": f"{mode_label} search • Press space to begin typing",
                "valid": False,
                "text": {"copy": "Type a search query to fetch Arrowhead notes."},
                "mods": {
                    "cmd": {
                        "subtitle": f"Enter opens results in {editor_label}",
                        "valid": False,
                    }
                },
            }
        ]
    }


def render_search_items(results: List[Dict[str, Any]], config: WorkflowConfig) -> Dict[str, Any]:
    """Transform Arrowhead results into Alfred Script Filter items."""
    items: List[Dict[str, Any]] = []
    secondary_editor = ALTERNATE_EDITOR.get(config.primary_editor, "default")
    vault_path = config.vault_path

    for result in results:
        item = build_item(result, config, vault_path, secondary_editor)
        items.append(item)

    return {"items": items}


def no_results_items(query: str, config: WorkflowConfig) -> Dict[str, Any]:
    """Display a friendly empty state when no hits are returned."""
    return {
        "items": [
            {
                "title": "No notes matched",
                "subtitle": f"Query `{query}` returned no results (mode: {config.search_mode})",
                "valid": False,
                "text": {
                    "copy": query,
                    "largetype": query,
                },
            }
        ]
    }


def error_items(payload: "ErrorPayload") -> Dict[str, Any]:
    """Return a single Alfred item describing an error."""
    subtitle = payload.detail or "Check Arrowhead daemon status."
    return {
        "items": [
            {
                "title": payload.title,
                "subtitle": subtitle,
                "valid": False,
                "text": {
                    "copy": f"{payload.title}\n\n{subtitle}",
                    "largetype": payload.title,
                },
            }
        ]
    }


def build_item(
    result: Dict[str, Any],
    config: WorkflowConfig,
    vault_path: Optional[Path],
    secondary_editor: str,
) -> Dict[str, Any]:
    note_id = result.get("note_id", "")
    title = select_title(result)
    subtitle = select_subtitle(result)
    relative_path = result.get("relative_path")
    absolute_path_field = result.get("absolute_path")
    if isinstance(absolute_path_field, str) and absolute_path_field.strip():
        absolute_path = absolute_path_field.strip()
    else:
        absolute_path = resolve_absolute_path(relative_path, note_id, vault_path)
    reason = result.get("reason")
    copy_text = absolute_path if absolute_path else relative_path or note_id
    preview = result.get("preview")
    match_terms = " ".join(
        filter(
            None,
            [
                title,
                note_id,
                relative_path or "",
                reason or "",
                preview or "",
            ],
        )
    )

    arg_payload = json.dumps(
        {
            "note_id": note_id,
            "relative_path": relative_path,
            "absolute_path": absolute_path,
        }
    )

    primary_editor = config.primary_editor
    item: Dict[str, Any] = {
        "uid": note_id or title,
        "title": title or "(untitled note)",
        "subtitle": subtitle,
        "arg": arg_payload,
        "match": match_terms,
        "text": {
            "copy": copy_text,
            "largetype": title,
        },
        "variables": {
            "open_editor": primary_editor,
            "primary_editor": primary_editor,
            "secondary_editor": secondary_editor,
            "arrowhead_note_id": note_id,
        },
    }

    if absolute_path:
        item["mods"] = {
            "cmd": {
                "subtitle": f"Open in {describe_editor(secondary_editor)}",
                "variables": {
                    "open_editor": secondary_editor,
                    "primary_editor": primary_editor,
                    "secondary_editor": secondary_editor,
                    "arrowhead_note_id": note_id,
                },
                "arg": arg_payload,
            }
        }
    elif reason:
        item.setdefault("mods", {})
        item["mods"]["cmd"] = {
            "subtitle": reason,
            "valid": False,
        }

    return item


def select_title(result: Dict[str, Any]) -> str:
    """Pick the best title candidate for a result."""
    title = result.get("title")
    if isinstance(title, str) and title.strip():
        return title.strip()

    metadata = result.get("metadata") or {}
    meta_title = metadata.get("title")
    if isinstance(meta_title, str) and meta_title.strip():
        return meta_title.strip()

    return result.get("note_id", "")


def select_subtitle(result: Dict[str, Any]) -> str:
    """Compose the subtitle shown in Alfred."""
    preview = (result.get("preview") or "").strip()
    reason = result.get("reason")
    score = result.get("score")
    bm25 = result.get("bm25")
    parts: List[str] = []

    if preview:
        parts.append(condense_whitespace(preview))

    score_bits: List[str] = []
    if isinstance(score, (int, float)):
        score_bits.append(f"score {score:.3f}")
    if isinstance(bm25, (int, float)):
        score_bits.append(f"BM25 {bm25:.2f}")
    if score_bits:
        parts.append(" • ".join(score_bits))

    if reason and not preview:
        parts.append(reason)

    return " — ".join(parts) if parts else "Result from Arrowhead"


def condense_whitespace(text: str) -> str:
    """Normalize whitespace for Alfred subtitles."""
    return re.sub(r"\s+", " ", text).strip()


def resolve_absolute_path(
    relative_path: Optional[str],
    note_id: str,
    vault_path: Optional[Path],
) -> Optional[str]:
    """Compute the absolute path to a note if possible."""
    if vault_path and relative_path:
        return str((vault_path / relative_path).resolve())
    if vault_path and note_id:
        candidate = vault_path / f"{note_id}.md"
        if candidate.exists():
            return str(candidate.resolve())
    return None


def load_workflow_config() -> WorkflowConfig:
    env = os.environ
    search_mode = normalize_search_mode(env.get("SEARCH_MODE", DEFAULT_SEARCH_MODE))
    result_limit = parse_int(env.get("RESULT_LIMIT"), DEFAULT_RESULT_LIMIT)
    primary_editor = normalize_editor(env.get("PRIMARY_EDITOR", DEFAULT_PRIMARY_EDITOR))
    cli_path = resolve_cli_path(env)
    vault_path = infer_vault_path(env)
    return WorkflowConfig(
        search_mode=search_mode,
        result_limit=result_limit,
        primary_editor=primary_editor,
        cli_path=cli_path,
        vault_path=vault_path,
    )


def normalize_search_mode(value: str) -> str:
    """Validate the requested search mode."""
    mode = (value or "").strip().lower()
    return mode if mode in {"fts", "semantic", "hybrid"} else DEFAULT_SEARCH_MODE


def parse_int(raw: Optional[str], fallback: int) -> int:
    try:
        value = int(raw) if raw else fallback
        return value if value > 0 else fallback
    except ValueError:
        return fallback


def normalize_editor(value: str) -> str:
    editor = (value or "").strip().lower()
    if editor in {"obsidian", "default"}:
        return editor
    return DEFAULT_PRIMARY_EDITOR


def resolve_cli_path(env: Dict[str, str]) -> str:
    override = env.get("ARROWHEAD_CLI_PATH")
    if override:
        expanded = Path(override).expanduser()
        return str(expanded)

    which_path = shutil.which("arrowhead")
    if which_path:
        return which_path

    # Attempt to resolve using a login shell PATH (common for brew installations).
    for shell in ("/bin/zsh", "/bin/bash"):
        if Path(shell).exists():
            try:
                result = subprocess.run(
                    [shell, "-lc", "command -v arrowhead"],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL,
                    text=True,
                    check=False,
                )
            except OSError:
                continue
            candidate = result.stdout.strip()
            if candidate:
                return candidate

    fallback_candidates = [
        Path.home() / ".local" / "bin" / "arrowhead",
        Path.home() / ".local" / "sbin" / "arrowhead",
        Path.home() / ".cargo" / "bin" / "arrowhead",
        Path.home() / "bin" / "arrowhead",
        Path("/opt/homebrew/bin/arrowhead"),
        Path("/opt/homebrew/sbin/arrowhead"),
        Path("/usr/local/opt/arrowhead/bin/arrowhead"),
        Path("/usr/local/bin/arrowhead"),
        Path("/usr/local/sbin/arrowhead"),
        Path("/usr/bin/arrowhead"),
        Path("/bin/arrowhead"),
    ]

    for candidate in fallback_candidates:
        if candidate.exists():
            return str(candidate)

    return "arrowhead"


def run_arrowhead_search(query: str, config: WorkflowConfig) -> "SearchPayload | ErrorPayload":
    """Invoke the CLI and parse JSON output."""
    cmd = [
        config.cli_path,
        "search",
        config.search_mode,
        query,
        "--json",
        "--limit",
        str(config.result_limit),
        "--include-paths",
    ]

    if config.vault_path and "--vault" not in cmd:
        cmd.extend(["--vault", str(config.vault_path)])

    try:
        env = os.environ.copy()
        path_entries = []
        if env.get("PATH"):
            path_entries.extend(env["PATH"].split(os.pathsep))
        for candidate in ADDITIONAL_PATH_DIRS:
            candidate_str = str(candidate)
            if candidate_str not in path_entries:
                path_entries.insert(0, candidate_str)
        env["PATH"] = os.pathsep.join(path_entries)

        completed = subprocess.run(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
            check=False,
            env=env,
        )
    except FileNotFoundError:
        return ErrorPayload(
            title="arrowhead CLI not found",
            detail="Install Arrowhead or adjust ARROWHEAD_CLI_PATH in the workflow settings.",
        )
    except subprocess.TimeoutExpired:
        return ErrorPayload(
            title="arrowhead search timed out",
            detail="Reduce RESULT_LIMIT or check daemon performance.",
        )

    if completed.returncode != 0:
        stderr = completed.stderr.strip() or completed.stdout.strip()
        return ErrorPayload(
            title="arrowhead search failed",
            detail=stderr.splitlines()[0] if stderr else None,
        )

    try:
        results = json.loads(completed.stdout)
    except json.JSONDecodeError as err:
        return ErrorPayload(
            title="Invalid JSON from arrowhead",
            detail=str(err),
        )

    if not isinstance(results, list):
        return ErrorPayload(
            title="Unexpected search payload",
            detail="Expected a JSON array from arrowhead search.",
        )

    return SearchPayload(results=results)


def infer_vault_path(env: Dict[str, str]) -> Optional[Path]:
    """Resolve the active vault path via environment hints or config file."""
    vault_override = env.get("VAULT_PATH") or env.get("ARROWHEAD_VAULT_PATH")
    if vault_override:
        path = Path(vault_override).expanduser()
        if path.exists():
            return path

    config_override = env.get("ARROWHEAD_CONFIG_PATH")
    candidates = list(config_candidates(config_override))

    for candidate in candidates:
        vault_path = parse_vault_from_config(candidate)
        if vault_path:
            return vault_path

    return None


def config_candidates(override: Optional[str]) -> Iterable[Path]:
    """Yield possible config file locations."""
    if override:
        yield Path(override).expanduser()
        return

    home = Path.home()
    platform = sys.platform

    if platform == "darwin":
        yield home / "Library" / "Application Support" / "Arrowhead" / "config.toml"
    elif platform == "win32":
        appdata = os.environ.get("APPDATA")
        base = Path(appdata) if appdata else home / "AppData" / "Roaming"
        yield base / "Arrowhead" / "config.toml"
    else:
        xdg_config = os.environ.get("XDG_CONFIG_HOME")
        base = Path(xdg_config) if xdg_config else home / ".config"
        yield base / "Arrowhead" / "config.toml"

    # Legacy path fallback.
    yield home / ".config" / "arrowhead" / "config.toml"


def parse_vault_from_config(path: Path) -> Optional[Path]:
    """Extract the vault path from the Arrowhead TOML config."""
    try:
        content = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return None
    except OSError:
        return None

    vault_line = None
    for line in content.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if stripped.lower().startswith("vault"):
            vault_line = stripped.split("=", 1)[1].strip()
            break

    if not vault_line:
        return None

    # Remove trailing comments.
    if "#" in vault_line:
        vault_line = vault_line.split("#", 1)[0].strip()

    try:
        value = ast.literal_eval(vault_line)
    except (SyntaxError, ValueError):
        return None

    if not isinstance(value, str):
        return None

    path = Path(value).expanduser()
    return path if path.exists() else None


def describe_editor(editor: str) -> str:
    if editor == "obsidian":
        return "Obsidian"
    if editor == "default":
        return "the default editor"
    return editor


@dataclass
class SearchPayload:
    results: List[Dict[str, Any]]


@dataclass
class ErrorPayload:
    title: str
    detail: Optional[str]


if __name__ == "__main__":
    main()
