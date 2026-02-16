#!/bin/sh
set -eu

print_usage() {
  cat <<'EOF'
Usage: check-ascii.sh [--staged]

Options:
  --staged   Check only staged files in git index (for pre-commit hook use).
  -h, --help Show this help.
EOF
}

MODE="tracked"
if [ $# -gt 0 ]; then
  case "$1" in
    --staged)
      MODE="staged"
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
fi

if [ $# -gt 0 ]; then
  echo "Unexpected extra arguments." >&2
  print_usage >&2
  exit 1
fi

is_checked_file() {
  case "$1" in
    *.rs|*.md|*.toml|*.lock|*.sh|*.ps1|*.yml|*.yaml|.gitattributes)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

TMP_BASE="${TMPDIR:-/tmp}/comon-ascii-$$"
LIST_FILE="${TMP_BASE}.list"
HITS_FILE="${TMP_BASE}.hits"

cleanup() {
  rm -f "${LIST_FILE}" "${HITS_FILE}"
}
trap cleanup EXIT INT TERM

FOUND=0
PATTERN='[^ -~	]'

check_worktree_file() {
  file="$1"
  [ -f "$file" ] || return 0
  if LC_ALL=C grep -n "$PATTERN" "$file" >"${HITS_FILE}"; then
    echo "Non-ASCII characters found in: $file"
    cat "${HITS_FILE}"
    FOUND=1
  fi
}

check_staged_file() {
  file="$1"
  if ! git cat-file -e ":$file" 2>/dev/null; then
    return 0
  fi
  if git show ":$file" | LC_ALL=C grep -n "$PATTERN" >"${HITS_FILE}"; then
    echo "Non-ASCII characters found in staged file: $file"
    cat "${HITS_FILE}"
    FOUND=1
  fi
}

if [ "$MODE" = "staged" ]; then
  git diff --cached --name-only --diff-filter=ACMR >"${LIST_FILE}"
else
  git ls-files >"${LIST_FILE}"
fi

while IFS= read -r file || [ -n "$file" ]; do
  [ -n "$file" ] || continue
  if ! is_checked_file "$file"; then
    continue
  fi
  if [ "$MODE" = "staged" ]; then
    check_staged_file "$file"
  else
    check_worktree_file "$file"
  fi
done < "${LIST_FILE}"

if [ "$FOUND" -ne 0 ]; then
  echo "ASCII check failed."
  exit 1
fi

echo "ASCII check passed."
