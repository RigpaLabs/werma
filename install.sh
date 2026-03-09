#!/usr/bin/env bash
# Werma Install — build engine + symlink + optional daemon
set -euo pipefail

WERMA_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Werma Install"
echo "============="
echo "WERMA_DIR: $WERMA_DIR"
echo ""

# --- Step 0: Sync repo ---
echo "→ Syncing repo..."
git -C "$WERMA_DIR" fetch origin main --quiet
LOCAL=$(git -C "$WERMA_DIR" rev-parse main 2>/dev/null)
REMOTE=$(git -C "$WERMA_DIR" rev-parse origin/main 2>/dev/null)
if [ "$LOCAL" != "$REMOTE" ]; then
    git -C "$WERMA_DIR" checkout main --quiet
    git -C "$WERMA_DIR" pull --ff-only origin main --quiet
    echo "  ✓ Updated main ($LOCAL → $REMOTE)"
else
    echo "  ✓ Already up to date"
fi
echo ""

# --- Step 1: Build ---
echo "→ Building werma engine..."
cargo build --release --manifest-path "$WERMA_DIR/engine/Cargo.toml"
echo "  ✓ Built successfully"

# --- Step 1.5: Smoke test (before installing) ---
BINARY="$WERMA_DIR/engine/target/release/werma"
echo ""
echo "→ Smoke testing new binary..."
FAIL=0
$BINARY --help >/dev/null 2>&1 || { echo "  ✗ FAIL: --help crashed"; FAIL=1; }
$BINARY st >/dev/null 2>&1 || { echo "  ✗ FAIL: st crashed"; FAIL=1; }
$BINARY list >/dev/null 2>&1 || { echo "  ✗ FAIL: list crashed"; FAIL=1; }
$BINARY sched ls >/dev/null 2>&1 || { echo "  ✗ FAIL: sched ls crashed"; FAIL=1; }
if [ "$FAIL" -ne 0 ]; then
    echo ""
    echo "  ✗ SMOKE TEST FAILED — aborting install. Old binary preserved."
    exit 1
fi
echo "  ✓ All smoke tests passed"

# --- Step 2: Symlink ---
echo ""
echo "→ Creating symlink..."
mkdir -p "$HOME/.local/bin"
ln -sf "$BINARY" "$HOME/.local/bin/werma"
echo "  ✓ werma → $HOME/.local/bin/werma"

# --- Step 3: Create runtime directories ---
echo ""
echo "→ Creating runtime directories..."
mkdir -p "$HOME/.werma/logs" "$HOME/.werma/backups" "$HOME/.werma/completed"
echo "  ✓ ~/.werma/ ready"

# --- Step 4: Daemon (optional) ---
echo ""
echo "To install the daemon (heartbeat + scheduler):"
echo "  werma daemon install"
echo ""
echo "To migrate from old aq system:"
echo "  werma migrate"
echo ""
echo "Done ⚔️"
