#!/usr/bin/env bash
set -euo pipefail

print_usage() {
  cat <<'EOF'
Usage: install-prebuilt.sh [root]

Args:
  root    Install root (default: ~/.local)

Installs the bundled `comon` binary from this package into <root>/bin.
EOF
}

ROOT="${1:-$HOME/.local}"

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
  print_usage
  exit 0
fi

if [ $# -gt 1 ]; then
  echo "Too many arguments" >&2
  print_usage >&2
  exit 1
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SRC_BIN="${SCRIPT_DIR}/comon"
COMON_HOME_DIR="${COMON_HOME:-$HOME/.comon}"

if [ ! -f "${SRC_BIN}" ]; then
  echo "Missing binary: ${SRC_BIN}" >&2
  exit 1
fi

if [ -L "${COMON_HOME_DIR}" ]; then
  echo "Refusing to use COMON_HOME (${COMON_HOME_DIR}): symlink is not allowed." >&2
  exit 1
fi

if [ -e "${COMON_HOME_DIR}" ] && [ ! -d "${COMON_HOME_DIR}" ]; then
  echo "Refusing to use COMON_HOME (${COMON_HOME_DIR}): expected a directory." >&2
  exit 1
fi

mkdir -p "${COMON_HOME_DIR}"
chmod 700 "${COMON_HOME_DIR}" 2>/dev/null || true

BIN_DIR="${ROOT}/bin"
DST_BIN="${BIN_DIR}/comon"

mkdir -p "${BIN_DIR}"

if [ -L "${DST_BIN}" ]; then
  echo "Refusing to overwrite symlink: ${DST_BIN}" >&2
  exit 1
fi

install -m 755 "${SRC_BIN}" "${DST_BIN}"

echo "Installed comon to ${DST_BIN}"
echo "Prepared COMON_HOME at ${COMON_HOME_DIR}"
case ":${PATH}:" in
  *":${BIN_DIR}:"*) ;;
  *) echo "Add to PATH: export PATH=\"${BIN_DIR}:\$PATH\"" ;;
esac
