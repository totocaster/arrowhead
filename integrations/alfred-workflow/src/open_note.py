#!/usr/bin/env python3
"""
Run Script handler for opening a selected Arrowhead note from Alfred.

Receives a JSON payload describing the selected note (note ID, relative path,
absolute path when available) and opens it in the requested editor.
"""

from __future__ import annotations

import ast
import json
import os
import shlex
import subprocess
import sys
from pathlib import Path
from typing import Dict, Optional

DEFAULT_PRIMARY_EDITOR = "obsidian"


def main() -> None:
    if len(sys.argv) < 2 or not sys.argv[1].strip():
        sys.exit("No note payload was provided.")

    payload = parse_payload(sys.argv[1])
    note_id = payload.get("note_id")
    absolute_path = payload.get("absolute_path")
    relative_path = payload.get("relative_path")

    vault_path = resolve_vault_path()
    note_path = resolve_note_path(absolute_path, relative_path, note_id, vault_path)
    if note_path is None:
        sys.exit("Unable to resolve note path; is the vault configured?")

    editor = normalise_editor(os.environ.get("open_editor"))
    if editor == "obsidian":
        command = ["open", "-b", "md.obsidian", note_path]
    elif editor == "default":
        command = ["open", note_path]
    else:
        custom = shlex.split(editor)
        command = [*custom, note_path]

    subprocess.run(command, check=False)


def parse_payload(raw: str) -> Dict[str, Optional[str]]:
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        # Alfred may HTML-encode JSON, so fall back to literal_eval.
        try:
            value = ast.literal_eval(raw)
        except (ValueError, SyntaxError):
            return {}
        if isinstance(value, dict):
            return value
        return {}


def resolve_vault_path() -> Optional[Path]:
    vault_override = os.environ.get("VAULT_PATH") or os.environ.get("ARROWHEAD_VAULT_PATH")
    if vault_override:
        path = Path(vault_override).expanduser()
        if path.exists():
            return path

    config_override = os.environ.get("ARROWHEAD_CONFIG_PATH")
    for candidate in config_candidates(config_override):
        path = parse_vault_from_config(candidate)
        if path:
            return path

    return None


def config_candidates(override: Optional[str]):
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
        xdg = os.environ.get("XDG_CONFIG_HOME")
        base = Path(xdg) if xdg else home / ".config"
        yield base / "Arrowhead" / "config.toml"

    yield home / ".config" / "arrowhead" / "config.toml"


def parse_vault_from_config(path: Path) -> Optional[Path]:
    try:
        content = path.read_text(encoding="utf-8")
    except (FileNotFoundError, OSError):
        return None

    for line in content.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if stripped.lower().startswith("vault"):
            raw_value = stripped.split("=", 1)[1].strip()
            if "#" in raw_value:
                raw_value = raw_value.split("#", 1)[0].strip()
            try:
                value = ast.literal_eval(raw_value)
            except (ValueError, SyntaxError):
                return None
            if isinstance(value, str):
                candidate = Path(value).expanduser()
                return candidate if candidate.exists() else None
    return None


def resolve_note_path(
    absolute_path: Optional[str],
    relative_path: Optional[str],
    note_id: Optional[str],
    vault_path: Optional[Path],
) -> Optional[str]:
    if absolute_path:
        return absolute_path

    if vault_path and relative_path:
        return str((vault_path / relative_path).resolve())

    if vault_path and note_id:
        candidate = vault_path / f"{note_id}.md"
        if candidate.exists():
            return str(candidate.resolve())

    return None


def normalise_editor(raw: Optional[str]) -> str:
    editor = (raw or DEFAULT_PRIMARY_EDITOR).strip().lower()
    if editor in {"obsidian", "default"}:
        return editor
    return editor


if __name__ == "__main__":
    main()
