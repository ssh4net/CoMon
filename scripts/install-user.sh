#!/usr/bin/env bash
set -euo pipefail

print_usage() {
  cat <<'EOF'
Usage: install-user.sh [--musl] [--target <triple>] [root]

Options:
  --musl              Build/install with musl target for a more portable Linux binary.
  --target <triple>   Override Rust target triple (use with or without --musl).
  -h, --help          Show this help.

Args:
  root                Install root (default: ~/.local).
EOF
}

ROOT="$HOME/.local"
USE_MUSL=0
BUILD_TARGET=""
POSITIONAL=()

while [ $# -gt 0 ]; do
  case "$1" in
    --musl)
      USE_MUSL=1
      shift
      ;;
    --target)
      if [ $# -lt 2 ]; then
        echo "Missing value for --target" >&2
        exit 1
      fi
      BUILD_TARGET="$2"
      shift 2
      ;;
    -h|--help)
      print_usage
      exit 0
      ;;
    -*)
      echo "Unknown option: $1" >&2
      print_usage >&2
      exit 1
      ;;
    *)
      POSITIONAL+=("$1")
      shift
      ;;
  esac
done

if [ "${#POSITIONAL[@]}" -gt 1 ]; then
  echo "Too many positional arguments" >&2
  print_usage >&2
  exit 1
fi

if [ "${#POSITIONAL[@]}" -eq 1 ]; then
  ROOT="${POSITIONAL[0]}"
fi

COMON_HOME_DIR="${COMON_HOME:-$HOME/.comon}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found in PATH. Install Rust first: https://rustup.rs" >&2
  exit 1
fi

if [ "${USE_MUSL}" -eq 1 ]; then
  if [ "$(uname -s)" != "Linux" ]; then
    echo "--musl is supported only on Linux." >&2
    exit 1
  fi

  if [ -z "${BUILD_TARGET}" ]; then
    case "$(uname -m)" in
      x86_64)
        BUILD_TARGET="x86_64-unknown-linux-musl"
        ;;
      aarch64|arm64)
        BUILD_TARGET="aarch64-unknown-linux-musl"
        ;;
      *)
        echo "Unsupported architecture for auto musl target: $(uname -m)." >&2
        echo "Use --target <triple> explicitly." >&2
        exit 1
        ;;
    esac
  fi
fi

if [ -n "${BUILD_TARGET}" ]; then
  if ! command -v rustup >/dev/null 2>&1; then
    echo "rustup is required to add target ${BUILD_TARGET}. Install via https://rustup.rs" >&2
    exit 1
  fi
  rustup target add "${BUILD_TARGET}"
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

INSTALL_ARGS=(
  --path "${REPO_DIR}"
  --locked
  --force
  --root "${ROOT}"
)

if [ -n "${BUILD_TARGET}" ]; then
  INSTALL_ARGS+=(--target "${BUILD_TARGET}")
fi

cargo install "${INSTALL_ARGS[@]}"

BIN_DIR="${ROOT}/bin"
echo "Installed comon to ${BIN_DIR}/comon"
if [ -n "${BUILD_TARGET}" ]; then
  echo "Build target: ${BUILD_TARGET}"
fi
echo "Prepared COMON_HOME at ${COMON_HOME_DIR}"
case ":${PATH}:" in
  *":${BIN_DIR}:"*) ;;
  *) echo "Add to PATH: export PATH=\"${BIN_DIR}:\$PATH\"" ;;
esac
