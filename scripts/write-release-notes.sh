#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "Usage: $0 <output-file> <version> <arm-url> <arm-sha> <intel-url> <intel-sha>" >&2
  exit 1
fi

OUTPUT_FILE="$1"
VERSION="$2"
ARM_URL="$3"
ARM_SHA="$4"
INTEL_URL="$5"
INTEL_SHA="$6"

REPOSITORY="${GITHUB_REPOSITORY:-totocaster/arrowhead}"
TAG_NAME="${ARROWHEAD_RELEASE_TAG:-${GITHUB_REF_NAME:-v${VERSION}}}"
PREVIOUS_TAG="${ARROWHEAD_PREVIOUS_TAG:-}"

if [[ -z "${PREVIOUS_TAG}" ]]; then
  PREVIOUS_TAG="$(git describe --tags --abbrev=0 "${TAG_NAME}^" 2>/dev/null || true)"
fi

if [[ -n "${PREVIOUS_TAG}" ]]; then
  RANGE="${PREVIOUS_TAG}..${TAG_NAME}"
  CHANGELOG_URL="https://github.com/${REPOSITORY}/compare/${PREVIOUS_TAG}...${TAG_NAME}"
else
  RANGE="${TAG_NAME}"
  CHANGELOG_URL="https://github.com/${REPOSITORY}/commits/${TAG_NAME}"
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

features="${TMP_DIR}/features.md"
fixes="${TMP_DIR}/fixes.md"
docs="${TMP_DIR}/docs.md"
maintenance="${TMP_DIR}/maintenance.md"
other="${TMP_DIR}/other.md"

touch "${features}" "${fixes}" "${docs}" "${maintenance}" "${other}"

while IFS=$'\t' read -r sha subject; do
  [[ -n "${sha}" && -n "${subject}" ]] || continue

  short_sha="${sha:0:7}"
  entry="- ${subject} ([${short_sha}](https://github.com/${REPOSITORY}/commit/${sha}))"

  case "${subject}" in
    feat:*|feat\(*|feature:*|feature\(*)
      printf '%s\n' "${entry}" >> "${features}"
      ;;
    fix:*|fix\(*)
      printf '%s\n' "${entry}" >> "${fixes}"
      ;;
    docs:*|docs\(*)
      printf '%s\n' "${entry}" >> "${docs}"
      ;;
    chore:*|chore\(*|ci:*|ci\(*|build:*|build\(*|test:*|test\(*|refactor:*|refactor\(*|perf:*|perf\(*)
      printf '%s\n' "${entry}" >> "${maintenance}"
      ;;
    *)
      printf '%s\n' "${entry}" >> "${other}"
      ;;
  esac
done < <(git log --no-merges --format='%H%x09%s' "${RANGE}")

append_section() {
  local title="$1"
  local file="$2"

  if [[ -s "${file}" ]]; then
    {
      printf '\n### %s\n\n' "${title}"
      cat "${file}"
    } >> "${OUTPUT_FILE}"
  fi
}

cat > "${OUTPUT_FILE}" <<EOF
## Install

\`\`\`sh
brew tap totocaster/tap
brew install totocaster/tap/arrowhead
arrowhead --version
\`\`\`

## Highlights

\`arrowhead\` is a cross-platform CLI and daemon for indexing Obsidian and Markdown vaults. It keeps full-text, semantic, hybrid, graph, notes, and MCP discovery tools ready for local CLIs and AI agents.

This release archive includes both \`arrowhead\` and \`arrowheadd\`.

## macOS Artifacts

| Platform | Archive | SHA256 |
| --- | --- | --- |
| Apple Silicon | [arrowhead-${VERSION}-aarch64-apple-darwin.tar.gz](${ARM_URL}) | \`${ARM_SHA}\` |
| Intel | [arrowhead-${VERSION}-x86_64-apple-darwin.tar.gz](${INTEL_URL}) | \`${INTEL_SHA}\` |

## Changelog
EOF

append_section "Features" "${features}"
append_section "Fixes" "${fixes}"
append_section "Documentation" "${docs}"
append_section "Maintenance" "${maintenance}"
append_section "Other Changes" "${other}"

cat >> "${OUTPUT_FILE}" <<EOF

**Full Changelog**: ${CHANGELOG_URL}
EOF
