#!/usr/bin/env bash
set -euo pipefail

ROOT="${1:-$HOME/.local}"
COMON_HOME_DIR="${COMON_HOME:-$HOME/.comon}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found in PATH. Install Rust first: https://rustup.rs" >&2
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

cargo install --path "${REPO_DIR}" --locked --force --root "${ROOT}"

BIN_DIR="${ROOT}/bin"
echo "Installed comon to ${BIN_DIR}/comon"
echo "Prepared COMON_HOME at ${COMON_HOME_DIR}"
case ":${PATH}:" in
  *":${BIN_DIR}:"*) ;;
  *) echo "Add to PATH: export PATH=\"${BIN_DIR}:\$PATH\"" ;;
esac
