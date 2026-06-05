#!/usr/bin/env bash
set -euo pipefail

print_usage() {
  cat <<'EOF'
Usage: package-macos.sh [--target <triple>] [--out-dir <dir>] [--skip-build]

Builds, signs, and packages a macOS comon release zip.

Environment:
  SIGN_IDENTITY    Developer ID Application identity. Defaults to ad-hoc signing.
  NOTARY_PROFILE   notarytool keychain profile name. Required unless SKIP_NOTARY=1.
  SKIP_NOTARY      Set to 1 to skip notarization.

Options:
  --target <triple>   Rust Apple target triple to build/package.
  --out-dir <dir>     Output directory for package zip (default: dist).
  --skip-build        Package existing binary without running cargo build.
  -h, --help          Show this help.
EOF
}

TARGET=""
OUT_DIR="dist"
SKIP_BUILD=0

while [ $# -gt 0 ]; do
  case "$1" in
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

if [ "$(uname -s)" != "Darwin" ]; then
  echo "macOS packaging must run on macOS." >&2
  exit 1
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
DIST_DIR="${REPO_DIR}/${OUT_DIR}"
SIGN_IDENTITY="${SIGN_IDENTITY:-}"
NOTARY_PROFILE="${NOTARY_PROFILE:-}"
SKIP_NOTARY="${SKIP_NOTARY:-0}"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found in PATH. Install Rust first: https://rustup.rs" >&2
  exit 1
fi
if ! command -v rustc >/dev/null 2>&1; then
  echo "rustc not found in PATH. Install Rust first: https://rustup.rs" >&2
  exit 1
fi
if ! command -v ditto >/dev/null 2>&1; then
  echo "ditto not found in PATH. Install Xcode command line tools." >&2
  exit 1
fi
if ! command -v otool >/dev/null 2>&1; then
  echo "otool not found in PATH. Install Xcode command line tools." >&2
  exit 1
fi
if ! command -v install_name_tool >/dev/null 2>&1; then
  echo "install_name_tool not found in PATH. Install Xcode command line tools." >&2
  exit 1
fi
if ! command -v codesign >/dev/null 2>&1; then
  echo "codesign not found in PATH. Install Xcode command line tools." >&2
  exit 1
fi

HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
if [ -z "${HOST_TRIPLE}" ]; then
  echo "Unable to detect Rust host target from rustc." >&2
  exit 1
fi

if [ -z "${TARGET}" ]; then
  TARGET="${HOST_TRIPLE}"
fi
case "${TARGET}" in
  *-apple-darwin) ;;
  *)
    echo "macOS packaging requires an Apple Darwin target, got: ${TARGET}" >&2
    exit 1
    ;;
esac

if [ "${TARGET}" != "${HOST_TRIPLE}" ]; then
  if ! command -v rustup >/dev/null 2>&1; then
    echo "rustup is required to add target ${TARGET}. Install via https://rustup.rs" >&2
    exit 1
  fi
  rustup target add "${TARGET}"
fi

if [ -z "${SIGN_IDENTITY}" ]; then
  SIGN_IDENTITY="-"
  SKIP_NOTARY=1
  echo "SIGN_IDENTITY is not set; using ad-hoc signing and skipping notarization." >&2
fi
if [ "${SKIP_NOTARY}" != "1" ] && [ -z "${NOTARY_PROFILE}" ]; then
  echo "NOTARY_PROFILE is required unless SKIP_NOTARY=1." >&2
  echo "Create one with: xcrun notarytool store-credentials <profile-name> --apple-id <email> --team-id <team-id>" >&2
  exit 1
fi
if [ "${SIGN_IDENTITY}" = "-" ]; then
  SIGN_TIMESTAMP_ARGS=(--timestamp=none)
else
  SIGN_TIMESTAMP_ARGS=(--timestamp)
fi

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "${REPO_DIR}/Cargo.toml" | head -n1)"
if [ -z "${VERSION}" ]; then
  echo "Unable to read version from Cargo.toml" >&2
  exit 1
fi

BUILD_ARGS=(--release)
if [ "${TARGET}" != "${HOST_TRIPLE}" ]; then
  BUILD_ARGS+=(--target "${TARGET}")
fi
if [ "${SKIP_BUILD}" -eq 0 ]; then
  (
    cd "${REPO_DIR}"
    cargo build "${BUILD_ARGS[@]}"
  )
fi

BIN_SRC="${REPO_DIR}/target/${TARGET}/release/comon"
if [ "${TARGET}" = "${HOST_TRIPLE}" ] && [ ! -f "${BIN_SRC}" ]; then
  BIN_SRC="${REPO_DIR}/target/release/comon"
fi
if [ ! -f "${BIN_SRC}" ]; then
  echo "Built binary not found: ${BIN_SRC}" >&2
  exit 1
fi

PKG_BASE="comon-v${VERSION}-${TARGET}"
PKG_ROOT="${DIST_DIR}/${PKG_BASE}"
ZIP_PATH="${DIST_DIR}/${PKG_BASE}.zip"
QUEUE_FILE="${DIST_DIR}/dylib-queue.txt"
PROCESSED_FILE="${DIST_DIR}/dylib-processed.txt"

cleanup() {
  rm -f "${QUEUE_FILE}" "${PROCESSED_FILE}"
}
trap cleanup EXIT

is_bundled_dependency() {
  case "$1" in
    /opt/homebrew/*|/usr/local/*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

dependency_basename() {
  basename "$1"
}

collect_bundled_dependencies() {
  otool -L "$1" | awk 'NR > 1 { print $1 }' | while IFS= read -r dep; do
    if is_bundled_dependency "${dep}"; then
      printf '%s\n' "${dep}"
    fi
  done
}

queue_dependency() {
  if ! grep -Fxq "$1" "${QUEUE_FILE}"; then
    printf '%s\n' "$1" >> "${QUEUE_FILE}"
  fi
}

rewrite_dependency_references() {
  local binary="$1"
  local dep_prefix="$2"

  collect_bundled_dependencies "${binary}" | while IFS= read -r dep; do
    install_name_tool -change "${dep}" "${dep_prefix}/$(dependency_basename "${dep}")" "${binary}"
  done
}

bundle_dylibs() {
  : > "${QUEUE_FILE}"
  : > "${PROCESSED_FILE}"

  collect_bundled_dependencies "${PKG_ROOT}/comon" | while IFS= read -r dep; do
    queue_dependency "${dep}"
  done

  while IFS= read -r dep; do
    if grep -Fxq "${dep}" "${PROCESSED_FILE}"; then
      continue
    fi

    local dest
    dest="${PKG_ROOT}/lib/$(dependency_basename "${dep}")"
    echo "Bundling dylib: ${dep}"
    mkdir -p "${PKG_ROOT}/lib"
    ditto --noextattr --noacl "${dep}" "${dest}"
    chmod u+w "${dest}"
    install_name_tool -id "@loader_path/$(dependency_basename "${dep}")" "${dest}"

    collect_bundled_dependencies "${dest}" | while IFS= read -r nested_dep; do
      queue_dependency "${nested_dep}"
    done

    printf '%s\n' "${dep}" >> "${PROCESSED_FILE}"
  done < "${QUEUE_FILE}"

  rewrite_dependency_references "${PKG_ROOT}/comon" "@executable_path/lib"
  if [ -d "${PKG_ROOT}/lib" ]; then
    find "${PKG_ROOT}/lib" -type f -name '*.dylib' -print | while IFS= read -r dylib; do
      rewrite_dependency_references "${dylib}" "@loader_path"
    done
  fi
}

sign_binary() {
  codesign --force \
    --sign "${SIGN_IDENTITY}" \
    --options runtime \
    "${SIGN_TIMESTAMP_ARGS[@]}" \
    "$1"
}

rm -rf "${PKG_ROOT}"
mkdir -p "${PKG_ROOT}" "${DIST_DIR}"

ditto --noextattr --noacl "${BIN_SRC}" "${PKG_ROOT}/comon"
chmod 755 "${PKG_ROOT}/comon"
ditto --noextattr --noacl "${REPO_DIR}/LICENSE" "${PKG_ROOT}/LICENSE"

bundle_dylibs

if [ -d "${PKG_ROOT}/lib" ]; then
  find "${PKG_ROOT}/lib" -type f -name '*.dylib' -print | while IFS= read -r dylib; do
    sign_binary "${dylib}"
  done
fi
sign_binary "${PKG_ROOT}/comon"
codesign --verify --strict --verbose=4 "${PKG_ROOT}/comon"

cat > "${PKG_ROOT}/install.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

ROOT="${1:-$HOME/.local}"
PKG_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SRC_BIN="${PKG_DIR}/comon"
COMON_HOME_DIR="${COMON_HOME:-$HOME/.comon}"

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
  echo "Usage: install.sh [root]"
  echo "Installs comon into <root>/bin and bundled dylibs into <root>/bin/lib."
  exit 0
fi
if [ $# -gt 1 ]; then
  echo "Too many arguments" >&2
  exit 1
fi
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

mkdir -p "${ROOT}/bin" "${COMON_HOME_DIR}"
chmod 700 "${COMON_HOME_DIR}" 2>/dev/null || true
install -m 755 "${SRC_BIN}" "${ROOT}/bin/comon"

if [ -d "${PKG_DIR}/lib" ]; then
  mkdir -p "${ROOT}/bin/lib"
  find "${PKG_DIR}/lib" -type f -name '*.dylib' -print | while IFS= read -r dylib; do
    install -m 755 "${dylib}" "${ROOT}/bin/lib/$(basename "${dylib}")"
  done
fi

echo "Installed comon to ${ROOT}/bin/comon"
echo "Prepared COMON_HOME at ${COMON_HOME_DIR}"
case ":${PATH}:" in
  *":${ROOT}/bin:"*) ;;
  *) echo "Add to PATH: export PATH=\"${ROOT}/bin:\$PATH\"" ;;
esac
EOF
chmod 755 "${PKG_ROOT}/install.sh"

cat > "${PKG_ROOT}/README.txt" <<EOF
comon ${VERSION} (${TARGET})

Run from this package:
  ./comon

Install (user scope, no Cargo required):
  bash install.sh

Optional custom install root:
  bash install.sh ~/.local

Binary path after install:
  ~/.local/bin/comon
EOF

rm -f "${ZIP_PATH}"
(
  cd "${DIST_DIR}"
  ditto -c -k --keepParent "${PKG_BASE}" "${ZIP_PATH}"
)

if [ "${SKIP_NOTARY}" != "1" ]; then
  xcrun notarytool submit "${ZIP_PATH}" \
    --keychain-profile "${NOTARY_PROFILE}" \
    --wait
else
  echo "Skipping notarization because SKIP_NOTARY=1."
fi

echo "Created: ${ZIP_PATH}"
