#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
OUTPUT_DIR="${SCRIPT_DIR}/dist"
BUNDLES="deb,appimage"
TARGET=""
SKIP_BUILD=0

usage() {
  cat <<'EOF'
Build Linux bundles for Cloudreve Desktop.

Usage:
  ./build-linux.sh [--skip-build] [--targets deb,appimage] [--target <rust-target>] [--output-dir <dir>]

Options:
  --skip-build          Skip `cargo tauri build` and only collect existing artifacts.
  --targets <list>      Tauri bundle targets (default: deb,appimage).
  --target <triple>     Rust target triple, e.g. x86_64-unknown-linux-gnu.
  --output-dir <path>   Output directory for collected bundle artifacts (default: ./dist).
  -h, --help            Show this help.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    --targets) BUNDLES="${2:-}"; shift 2 ;;
    --target) TARGET="${2:-}"; shift 2 ;;
    --output-dir) OUTPUT_DIR="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage; exit 1 ;;
  esac
done

if [[ "${SKIP_BUILD}" -eq 0 ]]; then
  echo "Building Linux bundles (targets: ${BUNDLES})..."
  pushd "${SCRIPT_DIR}" >/dev/null
  if [[ -n "${TARGET}" ]]; then
    cargo tauri build --target "${TARGET}" --bundles "${BUNDLES}"
  else
    cargo tauri build --bundles "${BUNDLES}"
  fi
  popd >/dev/null
else
  echo "Skipping build step."
fi

mkdir -p "${OUTPUT_DIR}"

declare -a bundle_roots=()
if [[ -n "${TARGET}" ]]; then
  bundle_roots+=("${SCRIPT_DIR}/target/${TARGET}/release/bundle")
  bundle_roots+=("${SCRIPT_DIR}/src-tauri/target/${TARGET}/release/bundle")
else
  bundle_roots+=("${SCRIPT_DIR}/target/release/bundle")
  bundle_roots+=("${SCRIPT_DIR}/src-tauri/target/release/bundle")
fi

copied=0
for root in "${bundle_roots[@]}"; do
  [[ -d "${root}" ]] || continue
  while IFS= read -r -d '' artifact; do
    cp -f "${artifact}" "${OUTPUT_DIR}/"
    copied=$((copied + 1))
  done < <(find "${root}" -type f \( -name "*.deb" -o -name "*.AppImage" -o -name "*.rpm" -o -name "*.tar.gz" \) -print0)
done

if [[ "${copied}" -eq 0 ]]; then
  echo "No bundle artifacts found. Checked:" >&2
  printf '  %s\n' "${bundle_roots[@]}" >&2
  exit 1
fi

echo "Collected ${copied} artifact(s) to ${OUTPUT_DIR}:"
find "${OUTPUT_DIR}" -maxdepth 1 -type f | sed 's|^|  |'
