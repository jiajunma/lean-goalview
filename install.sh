#!/bin/sh
# Build and install lean-goalview: the LSP proxy, the native webview window,
# and the official @leanprover/infoview frontend it serves.
set -eu

ROOT=$(cd "$(dirname "$0")" && pwd)
BIN="${PREFIX:-$HOME/.local/bin}"
WEBUI="${LEAN_GOALVIEW_WEBUI:-$HOME/.local/share/lean-goalview/webui}"

mkdir -p "$BIN"

echo "==> building proxy"
( cd "$ROOT" && cargo build --release )
install -m 755 "$ROOT/target/release/lean-goalview" "$BIN/lean-goalview"

echo "==> building window (embedded webview)"
( cd "$ROOT" && cargo build --release -p lean-goalview-window )
install -m 755 "$ROOT/target/release/lean-goalview-window" "$BIN/lean-goalview-window"

echo "==> building the official infoview frontend"
( cd "$ROOT/webui" && npm install && npx vite build )

echo "==> assembling $WEBUI"
rm -rf "$WEBUI"
mkdir -p "$WEBUI/imports"
cp "$ROOT/webui/dist/index.html" "$ROOT/webui/dist/main.js" "$WEBUI/"
D="$ROOT/webui/node_modules/@leanprover/infoview/dist"
cp "$D/index.production.min.js" \
   "$D/react.production.min.js" \
   "$D/react-dom.production.min.js" \
   "$D/react-jsx-runtime.production.min.js" \
   "$D/index.css" \
   "$D/codicon.ttf" \
   "$WEBUI/imports/"

echo "==> done"
echo "installed: $BIN/lean-goalview, $BIN/lean-goalview-window"
echo "infoview:  $WEBUI"
echo
echo "Point Zed's Lean server at the proxy (settings.json):"
echo '  "lsp": { "lean4-lsp": { "binary": { "path": "'"$BIN"'/lean-goalview" } } }'
echo
command -v cargo >/dev/null 2>&1 || echo "warning: cargo (Rust) not found" >&2
command -v npm   >/dev/null 2>&1 || echo "warning: npm (Node) not found" >&2
