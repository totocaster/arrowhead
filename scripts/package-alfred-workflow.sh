#!/usr/bin/env bash
set -euo pipefail

# Package the Arrowhead Alfred workflow into a .alfredworkflow archive.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW_ROOT="$ROOT_DIR/integrations/alfred-workflow"
SOURCE_DIR="$WORKFLOW_ROOT/src"
WORKFLOW_DIR="$WORKFLOW_ROOT/workflow"
INFO_PLIST="$WORKFLOW_DIR/info.plist"
OUTPUT="$WORKFLOW_DIR/arrowhead-search.alfredworkflow"

mkdir -p "$(dirname "$OUTPUT")"

if [[ ! -f "$INFO_PLIST" ]]; then
  echo "Missing workflow info.plist at $INFO_PLIST" >&2
  exit 1
fi

if [[ ! -d "$SOURCE_DIR" ]]; then
  echo "Missing workflow sources at $SOURCE_DIR" >&2
  exit 1
fi

export WORKFLOW_ROOT OUTPUT WORKFLOW_DIR ROOT_DIR
python3 - <<'PY'
import os
import zipfile
import plistlib
import re
from pathlib import Path

root = Path(os.environ["WORKFLOW_ROOT"])
workflow_dir = Path(os.environ["WORKFLOW_DIR"])
info_plist = workflow_dir / "info.plist"
source_dir = root / "src"
output = Path(os.environ["OUTPUT"])
workspace_root = Path(os.environ["ROOT_DIR"])
cargo_toml = workspace_root / "Cargo.toml"

workflow_version = None
if cargo_toml.exists():
    in_workspace_pkg = False
    version_pattern = re.compile(r'version\s*=\s*"([^"]+)"')
    for line in cargo_toml.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped == "[workspace.package]":
            in_workspace_pkg = True
            continue
        if in_workspace_pkg and stripped.startswith("["):
            break
        if in_workspace_pkg:
            match = version_pattern.search(stripped)
            if match:
                workflow_version = match.group(1)
                break

with info_plist.open("rb") as fp:
    plist_data = plistlib.load(fp)

if workflow_version:
    plist_data["version"] = workflow_version

plist_bytes = plistlib.dumps(plist_data)

with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as zf:
    zf.writestr("info.plist", plist_bytes)
    for path in sorted(source_dir.rglob("*")):
        if path.is_file():
            zf.write(path, arcname=str(Path("src") / path.relative_to(source_dir)))
    for asset in sorted(workflow_dir.iterdir()):
        if asset.name in {"info.plist", output.name}:
            continue
        if asset.is_file():
            zf.write(asset, arcname=asset.name)
PY

echo "Packaged workflow at $OUTPUT"
