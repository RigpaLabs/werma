#!/usr/bin/env bash
# Werma Install — download pre-built binary from GitHub Releases (cargo build fallback)
set -euo pipefail

WERMA_DIR="$(cd "$(dirname "$0")" && pwd)"
GITHUB_REPO="RigpaLabs/werma"

echo "Werma Install"
echo "============="
echo ""

# --- Detect platform ---
detect_target() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin)
            case "$arch" in
                arm64|aarch64) echo "aarch64-apple-darwin" ;;
                x86_64)        echo "x86_64-apple-darwin" ;;
                *)             echo "unknown" ;;
            esac
            ;;
        Linux)
            case "$arch" in
                x86_64) echo "x86_64-unknown-linux-gnu" ;;
                *)      echo "unknown" ;;
            esac
            ;;
        *) echo "unknown" ;;
    esac
}

TARGET=$(detect_target)

# --- Try GitHub Release download ---
download_release() {
    echo "→ Checking latest release..."
    local tag url tmp_dir

    tag=$(curl -sSf "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" \
        | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')

    if [ -z "$tag" ]; then
        echo "  ! Could not fetch latest release tag"
        return 1
    fi

    echo "  Latest release: $tag"

    url="https://github.com/${GITHUB_REPO}/releases/download/${tag}/werma-${TARGET}.tar.gz"
    tmp_dir=$(mktemp -d)

    echo "→ Downloading werma-${TARGET}..."
    if curl -sSfL "$url" -o "$tmp_dir/werma.tar.gz" 2>/dev/null; then
        tar xzf "$tmp_dir/werma.tar.gz" -C "$tmp_dir"
        if [ -f "$tmp_dir/werma" ]; then
            BINARY="$tmp_dir/werma"
            chmod +x "$BINARY"
            echo "  ✓ Downloaded $tag for $TARGET"
            return 0
        fi
    fi

    rm -rf "$tmp_dir"
    echo "  ! Download failed, falling back to cargo build"
    return 1
}

# --- Cargo build fallback ---
cargo_build() {
    echo "→ Building werma engine from source..."

    # Sync repo if this is a git checkout
    if [ -d "$WERMA_DIR/.git" ]; then
        echo "→ Syncing repo..."
        git -C "$WERMA_DIR" fetch origin main --quiet 2>/dev/null || true
        LOCAL=$(git -C "$WERMA_DIR" rev-parse main 2>/dev/null || echo "")
        REMOTE=$(git -C "$WERMA_DIR" rev-parse origin/main 2>/dev/null || echo "")
        if [ -n "$LOCAL" ] && [ -n "$REMOTE" ] && [ "$LOCAL" != "$REMOTE" ]; then
            git -C "$WERMA_DIR" checkout main --quiet
            git -C "$WERMA_DIR" pull --ff-only origin main --quiet
            echo "  ✓ Updated main"
        else
            echo "  ✓ Already up to date"
        fi
        echo ""
    fi

    if ! command -v cargo &>/dev/null; then
        echo "  ✗ cargo not found. Install Rust: https://rustup.rs"
        exit 1
    fi

    cargo build --release --manifest-path "$WERMA_DIR/engine/Cargo.toml"
    BINARY="$WERMA_DIR/engine/target/release/werma"
    echo "  ✓ Built successfully"

    # macOS: ad-hoc codesign so Gatekeeper does not SIGKILL the binary
    if [ "$(uname -s)" = "Darwin" ]; then
        echo "→ Codesigning binary (macOS)..."
        codesign --force --sign - "$BINARY"
        echo "  ✓ Codesigned"
    fi
}

# --- Main flow ---
BINARY=""

if [ "$TARGET" != "unknown" ]; then
    download_release || cargo_build
else
    echo "→ Unknown platform ($TARGET), building from source..."
    cargo_build
fi

# --- Smoke test ---
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

# --- Symlink ---
echo ""
echo "→ Installing binary..."
mkdir -p "$HOME/.local/bin"
cp "$BINARY" "$HOME/.local/bin/werma"
chmod +x "$HOME/.local/bin/werma"
# macOS: cp creates a new inode, invalidating the ad-hoc signature — re-sign
if [ "$(uname -s)" = "Darwin" ]; then
    codesign --force --sign - "$HOME/.local/bin/werma"
fi
echo "  ✓ werma → $HOME/.local/bin/werma"

# --- Create runtime directories ---
echo ""
echo "→ Creating runtime directories..."
mkdir -p "$HOME/.werma/logs" "$HOME/.werma/backups" "$HOME/.werma/completed"
echo "  ✓ ~/.werma/ ready"

# --- Restart daemon (if running) ---
DAEMON_LABEL="io.rigpalabs.werma.daemon"
if launchctl list "$DAEMON_LABEL" &>/dev/null 2>&1; then
    echo ""
    echo "→ Restarting daemon..."
    launchctl kickstart -k "gui/$(id -u)/$DAEMON_LABEL"
    echo "  ✓ Daemon restarted"
fi

echo ""
echo "To install the daemon (heartbeat + scheduler):"
echo "  werma daemon install"
echo ""
echo "Done ⚔️"
