#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "Usage: $0 <tap-dir> <version> <arm-url> <arm-sha> <intel-url> <intel-sha>" >&2
  exit 1
fi

TAP_DIR="$1"
VERSION="$2"
ARM_URL="$3"
ARM_SHA="$4"
INTEL_URL="$5"
INTEL_SHA="$6"

FORMULA_PATH="${TAP_DIR}/Formula/arrowhead.rb"

mkdir -p "$(dirname "${FORMULA_PATH}")"

cat >"${FORMULA_PATH}" <<EOF
class Arrowhead < Formula
  desc "Fast Obsidian search and discovery CLI and daemon"
  homepage "https://github.com/totocaster/arrowhead"
  version "${VERSION}"
  license "MIT"

  on_macos do
    on_arm do
      url "${ARM_URL}"
      sha256 "${ARM_SHA}"
    end

    on_intel do
      url "${INTEL_URL}"
      sha256 "${INTEL_SHA}"
    end
  end

  def install
    bin.install "bin/arrowhead"
    bin.install "bin/arrowheadd"
  end

  test do
    output = shell_output("#{bin}/arrowhead --help")
    assert_match "arrowhead", output

    daemon_output = shell_output("#{bin}/arrowheadd --help")
    assert_match "arrowheadd", daemon_output
  end
end
EOF
