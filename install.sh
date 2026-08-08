#!/bin/sh
# Build merma from source and install the binary. The hook install itself is
# done by the binary (`merma install`), which edits ~/.claude/settings.json
# with a backup and prints the rollback value.
set -eu

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "error: $1 is required" >&2
    exit 1
  }
}
need cargo
need git

cd "$(dirname "$0")"

echo "==> build (release)"
cargo build --release

BIN_DIR="${HOME}/.local/bin"
BIN="${BIN_DIR}/merma"
mkdir -p "$BIN_DIR"
install -m 755 target/release/merma "$BIN"
"$BIN" --version >/dev/null || {
  echo "error: the installed binary does not run" >&2
  exit 1
}
echo "==> installed $BIN"

echo
echo "==> preview of the hook install (no changes yet):"
"$BIN" install --print
echo

if [ -t 0 ]; then
  printf "Install the statusline hook now? It edits ~/.claude/settings.json and keeps a backup. [y/N] "
  read -r ans
  case "$ans" in
  y | Y | yes | YES) "$BIN" install ;;
  *) echo "Skipped. Run 'merma install' when you are ready." ;;
  esac
else
  echo "Non-interactive shell: hook not installed. Run 'merma install' when you are ready."
fi

case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*) echo "note: $BIN_DIR is not on your PATH" ;;
esac

echo
echo "next steps:"
echo "  merma scan      # first ingest of local history"
echo "  merma report    # the waste report"
echo "  merma           # the live dashboard"
echo "  merma doctor    # check every data source"
