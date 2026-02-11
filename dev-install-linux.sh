#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SKIP_BUILD=0
TARGET=""
DESKTOP_NAME="cloudreve-desktop-dev.desktop"
ICON_NAME="cloudreve-desktop"

usage() {
  cat <<'EOF'
Build and install a local desktop entry for Cloudreve Desktop on Linux.

Usage:
  ./dev-install-linux.sh [--skip-build] [--target <rust-target>]

Options:
  --skip-build        Skip `cargo tauri build --no-bundle`.
  --target <triple>   Rust target triple, e.g. x86_64-unknown-linux-gnu.
  -h, --help          Show this help.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    --target) TARGET="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage; exit 1 ;;
  esac
done

if [[ "${SKIP_BUILD}" -eq 0 ]]; then
  echo "Building release binary for development install..."
  pushd "${SCRIPT_DIR}" >/dev/null
  if [[ -n "${TARGET}" ]]; then
    cargo tauri build --no-bundle --target "${TARGET}"
  else
    cargo tauri build --no-bundle
  fi
  popd >/dev/null
else
  echo "Skipping build step."
fi

declare -a bin_candidates=()
if [[ -n "${TARGET}" ]]; then
  bin_candidates+=("${SCRIPT_DIR}/target/${TARGET}/release/cloudreve-desktop")
  bin_candidates+=("${SCRIPT_DIR}/src-tauri/target/${TARGET}/release/cloudreve-desktop")
else
  bin_candidates+=("${SCRIPT_DIR}/target/release/cloudreve-desktop")
  bin_candidates+=("${SCRIPT_DIR}/src-tauri/target/release/cloudreve-desktop")
fi

BIN_PATH=""
for candidate in "${bin_candidates[@]}"; do
  if [[ -x "${candidate}" ]]; then
    BIN_PATH="${candidate}"
    break
  fi
done

if [[ -z "${BIN_PATH}" ]]; then
  echo "Built binary not found. Checked:" >&2
  printf '  %s\n' "${bin_candidates[@]}" >&2
  exit 1
fi

ICON_SRC="${SCRIPT_DIR}/src-tauri/icons/128x128.png"
if [[ ! -f "${ICON_SRC}" ]]; then
  echo "Icon file not found: ${ICON_SRC}" >&2
  exit 1
fi

XDG_DATA_HOME="${XDG_DATA_HOME:-${HOME}/.local/share}"
APP_DIR="${XDG_DATA_HOME}/applications"
ICON_DIR="${XDG_DATA_HOME}/icons/hicolor/128x128/apps"
DESKTOP_FILE="${APP_DIR}/${DESKTOP_NAME}"
ICON_FILE="${ICON_DIR}/${ICON_NAME}.png"

mkdir -p "${APP_DIR}" "${ICON_DIR}"
cp -f "${ICON_SRC}" "${ICON_FILE}"

cat > "${DESKTOP_FILE}" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=Cloudreve Desktop (Dev)
Comment=Cloudreve Desktop Sync Client (Development)
Exec=${BIN_PATH}
Icon=${ICON_NAME}
Terminal=false
Categories=Network;Utility;
StartupNotify=true
StartupWMClass=cloudreve.desktop
EOF

chmod 0644 "${DESKTOP_FILE}"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "${APP_DIR}" || true
fi

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -f "${XDG_DATA_HOME}/icons/hicolor" || true
fi

echo "Installed development desktop entry:"
echo "  ${DESKTOP_FILE}"
echo "Using binary:"
echo "  ${BIN_PATH}"
echo "Using icon:"
echo "  ${ICON_FILE}"
