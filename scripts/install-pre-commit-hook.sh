#!/bin/sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
REPO_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
HOOK_DIR="${REPO_DIR}/.git/hooks"
HOOK_PATH="${HOOK_DIR}/pre-commit"

mkdir -p "${HOOK_DIR}"

cat > "${HOOK_PATH}" <<'EOF'
#!/bin/sh
set -eu

REPO_ROOT="$(git rev-parse --show-toplevel)"
sh "${REPO_ROOT}/scripts/check-ascii.sh" --staged
EOF

chmod +x "${HOOK_PATH}"
echo "Installed pre-commit hook: ${HOOK_PATH}"
