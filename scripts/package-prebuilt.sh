#!/usr/bin/env bash
set -euo pipefail

print_usage() {
  cat <<'EOF'
Usage: package-prebuilt.sh [--musl|--gnu] [--target <triple>] [--out-dir <dir>] [--skip-build]

Options:
  --musl              Build portable Linux binary using musl target.
  --gnu               Build using Rust host target (glibc-linked on Linux).
  --target <triple>   Rust target triple to build/package.
  --out-dir <dir>     Output directory for package zip (default: dist).
  --skip-build        Package existing binary without running cargo build.
  -h, --help          Show this help.
EOF
}

USE_MUSL=0
USE_GNU=0
TARGET=""
OUT_DIR="dist"
SKIP_BUILD=0

while [ $# -gt 0 ]; do
  case "$1" in
    --musl)
      USE_MUSL=1
      shift
      ;;
    --gnu)
      USE_GNU=1
      shift
      ;;
    --target)
      if [ $# -lt 2 ]; then
        echo "Missing value for --target" >&2
        exit 1
      fi
      TARGET="$2"
      shift 2
      ;;
    --out-dir)
      if [ $# -lt 2 ]; then
        echo "Missing value for --out-dir" >&2
        exit 1
      fi
      OUT_DIR="$2"
      shift 2
      ;;
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    -h|--help)
      print_usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      print_usage >&2
      exit 1
      ;;
  esac
done

if [ "${USE_MUSL}" -eq 1 ] && [ "${USE_GNU}" -eq 1 ]; then
  echo "Use only one of --musl or --gnu." >&2
  exit 1
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found in PATH. Install Rust first: https://rustup.rs" >&2
  exit 1
fi

if ! command -v zip >/dev/null 2>&1; then
  echo "zip not found in PATH. Install zip package from your distro." >&2
  exit 1
fi

HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
if [ -z "${HOST_TRIPLE}" ]; then
  echo "Unable to detect Rust host target from rustc." >&2
  exit 1
fi

if [ -z "${TARGET}" ] && [ "${USE_MUSL}" -eq 0 ] && [ "${USE_GNU}" -eq 0 ] && [ "$(uname -s)" = "Linux" ]; then
  USE_MUSL=1
  echo "No target specified: defaulting to portable musl package on Linux." >&2
fi

if [ "${USE_GNU}" -eq 1 ] && [ -z "${TARGET}" ]; then
  TARGET="${HOST_TRIPLE}"
fi

if [ "${USE_MUSL}" -eq 1 ] && [ -n "${TARGET}" ] && [[ "${TARGET}" != *-musl ]]; then
  echo "--musl conflicts with non-musl target: ${TARGET}" >&2
  exit 1
fi

if [ "${USE_MUSL}" -eq 1 ] && [ -z "${TARGET}" ]; then
  if [ "$(uname -s)" != "Linux" ]; then
    echo "--musl packaging is supported only on Linux." >&2
    exit 1
  fi

  case "$(uname -m)" in
    x86_64)
      TARGET="x86_64-unknown-linux-musl"
      ;;
    aarch64|arm64)
      TARGET="aarch64-unknown-linux-musl"
      ;;
    *)
      echo "Unsupported architecture for auto musl target: $(uname -m)." >&2
      echo "Use --target <triple> explicitly." >&2
      exit 1
      ;;
  esac
fi

if [ -n "${TARGET}" ]; then
  if ! command -v rustup >/dev/null 2>&1; then
    echo "rustup is required to add target ${TARGET}. Install via https://rustup.rs" >&2
    exit 1
  fi
  rustup target add "${TARGET}"
else
  TARGET="${HOST_TRIPLE}"
fi

BUILD_ARGS=(--release)
if [ "${TARGET}" != "${HOST_TRIPLE}" ]; then
  BUILD_ARGS+=(--target "${TARGET}")
fi

if [ "${SKIP_BUILD}" -eq 0 ]; then
  if ! (
    cd "${REPO_DIR}"
    cargo build "${BUILD_ARGS[@]}"
  ); then
    if [[ "${TARGET}" == *-musl ]]; then
      echo "musl build failed; see the compiler or linker error above." >&2
      echo "If the musl compiler/linker is missing, install it and retry (Debian/Ubuntu: sudo apt install musl-tools)." >&2
    fi
    exit 1
  fi
fi

BIN_NAME="comon"
BIN_SRC="${REPO_DIR}/target/${TARGET}/release/${BIN_NAME}"
if [ "${TARGET}" = "${HOST_TRIPLE}" ] && [ ! -f "${BIN_SRC}" ]; then
  BIN_SRC="${REPO_DIR}/target/release/${BIN_NAME}"
fi

if [ ! -f "${BIN_SRC}" ]; then
  echo "Built binary not found: ${BIN_SRC}" >&2
  exit 1
fi

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "${REPO_DIR}/Cargo.toml" | head -n1)"
if [ -z "${VERSION}" ]; then
  echo "Unable to read version from Cargo.toml" >&2
  exit 1
fi

PKG_BASE="comon-v${VERSION}-${TARGET}"
PKG_ROOT="${REPO_DIR}/${OUT_DIR}/${PKG_BASE}"
ZIP_PATH="${REPO_DIR}/${OUT_DIR}/${PKG_BASE}.zip"

rm -rf "${PKG_ROOT}"
mkdir -p "${PKG_ROOT}"

install -m 755 "${BIN_SRC}" "${PKG_ROOT}/comon"
install -m 755 "${REPO_DIR}/scripts/install-prebuilt.sh" "${PKG_ROOT}/install.sh"
install -m 644 "${REPO_DIR}/LICENSE" "${PKG_ROOT}/LICENSE"

cat > "${PKG_ROOT}/README.txt" <<EOF
comon ${VERSION} (${TARGET})

Install (user scope, no Cargo required):
  bash install.sh

Optional custom install root:
  bash install.sh ~/.local

Binary path after install:
  ~/.local/bin/comon
EOF

mkdir -p "${REPO_DIR}/${OUT_DIR}"
rm -f "${ZIP_PATH}"
(
  cd "${REPO_DIR}/${OUT_DIR}"
  zip -rq "${PKG_BASE}.zip" "${PKG_BASE}"
)

echo "Created package: ${ZIP_PATH}"
