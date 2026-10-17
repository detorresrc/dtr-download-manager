#!/usr/bin/env bash
# Register the Grab native messaging host with Chrome/Chromium on Linux.
#
# Usage:
#   ./scripts/install-host-linux.sh <EXTENSION_ID> [path-to-grab-host-binary]
#
# If the binary path is omitted, we use the release build at
# target/release/grab-host (run `cargo build --release -p grab-host` first).

set -euo pipefail

EXTENSION_ID="${1:-}"
if [[ -z "$EXTENSION_ID" ]]; then
  echo "error: pass the extension ID as the first argument." >&2
  echo "       (find it at chrome://extensions with Developer mode on)" >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOST_BIN="${2:-$REPO_ROOT/target/release/grab-host}"

if [[ ! -x "$HOST_BIN" ]]; then
  echo "error: host binary not found or not executable: $HOST_BIN" >&2
  echo "       build it with: cargo build --release -p grab-host" >&2
  exit 1
fi

HOST_NAME="com.grab.download_manager"
TEMPLATE="$REPO_ROOT/host/manifest.template.json"

# Chrome, Chromium, and Chrome Beta look in their own NativeMessagingHosts dirs.
TARGET_DIRS=(
  "$HOME/.config/google-chrome/NativeMessagingHosts"
  "$HOME/.config/chromium/NativeMessagingHosts"
  "$HOME/.config/google-chrome-beta/NativeMessagingHosts"
)

installed=0
for dir in "${TARGET_DIRS[@]}"; do
  parent="$(dirname "$dir")"
  # Only install where the browser's config dir already exists.
  if [[ -d "$parent" ]]; then
    mkdir -p "$dir"
    manifest="$dir/$HOST_NAME.json"
    sed -e "s#__HOST_PATH__#$HOST_BIN#g" \
        -e "s#__EXTENSION_ID__#$EXTENSION_ID#g" \
        "$TEMPLATE" > "$manifest"
    echo "installed: $manifest"
    installed=1
  fi
done

if [[ "$installed" -eq 0 ]]; then
  echo "warning: no Chrome/Chromium config dir found. Installing to google-chrome anyway." >&2
  dir="$HOME/.config/google-chrome/NativeMessagingHosts"
  mkdir -p "$dir"
  manifest="$dir/$HOST_NAME.json"
  sed -e "s#__HOST_PATH__#$HOST_BIN#g" \
      -e "s#__EXTENSION_ID__#$EXTENSION_ID#g" \
      "$TEMPLATE" > "$manifest"
  echo "installed: $manifest"
fi

echo
echo "Done. Restart Chrome, then try a download."
