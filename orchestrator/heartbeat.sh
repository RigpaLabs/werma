#!/usr/bin/env bash
# Werma Heartbeat — Layer 1 Orchestrator
# Zero-token bash watchdog. Runs every minute via launchd.
# Checks: stuck agents, resource limits, queue drain, stale detection.

set -euo pipefail

WERMA_DIR="$(cd "$(dirname "$0")/.." && pwd)"
AQ_DIR="$HOME/.agent-queue"
AQ_BIN="$HOME/projects/ai/aq/aq"
LOG_FILE="$AQ_DIR/logs/heartbeat.log"
SIGNALS_FILE="$WERMA_DIR/shared/signals.md"
LIMITS_FILE="$WERMA_DIR/limits.json"

# Dry run mode
DRY_RUN=false
[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=true

# Ensure log directory exists
mkdir -p "$(dirname "$LOG_FILE")"

log() {
    local msg="[$(date '+%Y-%m-%d %H:%M:%S')] [heartbeat] $1"
    echo "$msg" >> "$LOG_FILE"
    [[ "$DRY_RUN" == "true" ]] && echo "$msg"
}

signal() {
    local type="$1" msg="$2"
    local entry="[$(date '+%Y-%m-%d %H:%M')] [heartbeat] [$type] $msg"
    # Append under "## Active Signals" section
    if [[ -f "$SIGNALS_FILE" ]]; then
        # Remove placeholder if present, then append entry
        if grep -q "^_No active signals._" "$SIGNALS_FILE"; then
            awk -v entry="$entry" '/^_No active signals._/{print entry; next}{print}' "$SIGNALS_FILE" > "${SIGNALS_FILE}.tmp" \
                && mv "${SIGNALS_FILE}.tmp" "$SIGNALS_FILE"
        else
            # Append after "## Active Signals" line
            awk -v entry="$entry" '/^## Active Signals/{print; print entry; next}{print}' "$SIGNALS_FILE" > "${SIGNALS_FILE}.tmp" \
                && mv "${SIGNALS_FILE}.tmp" "$SIGNALS_FILE"
        fi
    fi
    log "$type: $msg"
}

# --- Check 1: Stuck agents (running > timeout) ---
check_stuck_agents() {
    log "Checking for stuck agents..."

    if ! command -v "$AQ_BIN" &>/dev/null; then
        log "WARN: aq binary not found at $AQ_BIN"
        return
    fi

    # Get running tasks
    local running
    running=$("$AQ_BIN" st 2>/dev/null | grep -i "running\|in.progress" || true)

    if [[ -z "$running" ]]; then
        log "No running agents"
        return
    fi

    # Check each running task for timeout (default 30 min)
    local max_minutes=30
    if [[ -f "$LIMITS_FILE" ]] && command -v jq &>/dev/null; then
        max_minutes=$(jq -r '.global.timeout_minutes // 30' "$LIMITS_FILE")
    fi

    log "Running agents found, max timeout: ${max_minutes}m"
    # Note: actual stuck detection requires aq to expose start times
    # For now, log running tasks for visibility
    echo "$running" | while read -r line; do
        log "  RUNNING: $line"
    done
}

# --- Check 2: Queue drain ---
check_queue() {
    log "Checking queue..."

    if ! command -v "$AQ_BIN" &>/dev/null; then
        return
    fi

    local pending
    pending=$("$AQ_BIN" st 2>/dev/null | grep -ci "pending" || echo "0")

    if [[ "$pending" -gt 0 ]]; then
        log "Queue: $pending pending tasks"
    else
        log "Queue: empty"
    fi
}

# --- Check 3: Stale detection (no activity in signals for > 1 hour) ---
check_stale() {
    if [[ ! -f "$SIGNALS_FILE" ]]; then
        log "WARN: signals file not found"
        return
    fi

    # Check if signals file was modified in last hour (macOS stat)
    local now last_mod age_minutes
    now=$(date +%s)
    last_mod=$(stat -f %m "$SIGNALS_FILE" 2>/dev/null || echo "$now")
    age_minutes=$(( (now - last_mod) / 60 ))

    if [[ $age_minutes -gt 60 ]]; then
        log "WARN: signals file stale (${age_minutes}m since last update)"
    fi
}

# --- Check 4: Disk space ---
check_disk() {
    local usage
    usage=$(df -h "$HOME" | tail -1 | awk '{print $5}' | tr -d '%')

    if [[ "$usage" -gt 90 ]]; then
        signal "ALERT" "Disk usage critical: ${usage}%"
    elif [[ "$usage" -gt 80 ]]; then
        log "WARN: Disk usage high: ${usage}%"
    fi
}

# --- Check 5: Log rotation ---
check_log_size() {
    if [[ -f "$LOG_FILE" ]]; then
        local size
        size=$(stat -f %z "$LOG_FILE" 2>/dev/null || echo "0")
        # Rotate if > 5MB
        if [[ "$size" -gt 5242880 ]]; then
            mv "$LOG_FILE" "${LOG_FILE}.old"
            log "Log rotated (was ${size} bytes)"
        fi
    fi
}

# --- Main ---
main() {
    log "=== Heartbeat start ==="

    check_stuck_agents
    check_queue
    check_stale
    check_disk
    check_log_size

    log "=== Heartbeat done ==="
}

if [[ "$DRY_RUN" == "true" ]]; then
    echo "Werma Heartbeat — dry run"
    echo "WERMA_DIR: $WERMA_DIR"
    echo "AQ_DIR: $AQ_DIR"
    echo "AQ_BIN: $AQ_BIN"
    echo "---"
    main
    exit 0
fi

main
