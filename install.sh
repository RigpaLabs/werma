#!/usr/bin/env bash
# Werma Install — symlinks + launchd registration
set -euo pipefail

WERMA_DIR="$(cd "$(dirname "$0")" && pwd)"
AQ_DIR="$HOME/.agent-queue"
LAUNCHD_DIR="$HOME/Library/LaunchAgents"
PLIST_NAME="com.rigpalabs.werma.heartbeat"

echo "Werma Install"
echo "============="
echo "WERMA_DIR: $WERMA_DIR"
echo ""

# --- Step 1: Symlinks ---
echo "→ Creating symlinks..."

mkdir -p "$AQ_DIR/prompts"

# Link werma orchestrator prompt
ln -sf "$WERMA_DIR/orchestrator/werma.md" "$AQ_DIR/prompts/werma-orchestrator.md"
echo "  ✓ werma-orchestrator.md → $AQ_DIR/prompts/"

# Link agent characters for quick access
for agent_dir in "$WERMA_DIR"/agents/*/; do
    [[ -d "$agent_dir" ]] || continue
    agent_name=$(basename "$agent_dir")
    [[ -f "$agent_dir/character.md" ]] || { echo "  ⚠ $agent_name/character.md not found, skipping"; continue; }
    ln -sf "$agent_dir/character.md" "$AQ_DIR/prompts/werma-${agent_name}-character.md"
    echo "  ✓ werma-${agent_name}-character.md → $AQ_DIR/prompts/"
done

# --- Step 2: Make heartbeat executable ---
echo ""
echo "→ Setting permissions..."
chmod +x "$WERMA_DIR/orchestrator/heartbeat.sh"
echo "  ✓ heartbeat.sh is executable"

# --- Step 3: Launchd plist ---
echo ""
echo "→ Installing launchd agent..."

mkdir -p "$LAUNCHD_DIR"

cat > "$LAUNCHD_DIR/${PLIST_NAME}.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${PLIST_NAME}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${WERMA_DIR}/orchestrator/heartbeat.sh</string>
    </array>
    <key>StartInterval</key>
    <integer>60</integer>
    <key>StandardOutPath</key>
    <string>${AQ_DIR}/logs/heartbeat-stdout.log</string>
    <key>StandardErrorPath</key>
    <string>${AQ_DIR}/logs/heartbeat-stderr.log</string>
    <key>RunAtLoad</key>
    <false/>
    <key>KeepAlive</key>
    <false/>
</dict>
</plist>
EOF

echo "  ✓ Plist written to $LAUNCHD_DIR/${PLIST_NAME}.plist"

# --- Step 4: Load (optional) ---
echo ""
echo "To activate heartbeat:"
echo "  launchctl load $LAUNCHD_DIR/${PLIST_NAME}.plist"
echo ""
echo "To deactivate:"
echo "  launchctl unload $LAUNCHD_DIR/${PLIST_NAME}.plist"
echo ""
echo "To test:"
echo "  bash $WERMA_DIR/orchestrator/heartbeat.sh --dry-run"
echo ""
echo "Done ⚔️"
