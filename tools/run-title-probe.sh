#!/usr/bin/env bash
# Launch the original title once, with the overlay DLL provably replaced and the log provably
# growing, so a failed launch can never be mistaken for a finding.
#
#   tools/run-title-probe.sh [log-name] [extra TOMBOX_PASSCAP spec]
#
# Exits non-zero with one reason line on any of: stale title process still alive, DLL copy
# mismatch (SHA-256), launch failed, or the log not growing. No && chains: every step is checked.
set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OVERLAY="${TOMBOX_OVERLAY:-/c/Hannibal-lecter/run-overlay-gta6-dev-build-xbox}"
GAME="$OVERLAY/game_scarlett_master.exe"
NAME="${1:-probe}"
LOG="$OVERLAY/$NAME.log"
CONSOLE="$OVERLAY/$NAME.console"
DLL="$REPO/target/release/d3d12_xs.dll"
MDBG="$REPO/target/release/mdbg_hr.exe"

fail() { echo "RUNNER: FAIL: $*" >&2; exit 1; }

[ -f "$DLL" ] || fail "no $DLL (cargo build --release -p d3d12_xs)"
[ -f "$MDBG" ] || fail "no $MDBG (cargo build --release --bin mdbg)"
[ -f "$GAME" ] || fail "no $GAME"

# 1. Kill a stale title by PID (never by name pattern) and prove it is gone.
PIDS=$(tasklist 2>/dev/null | grep -i "game_scarlett_master" | awk '{print $2}')
for p in $PIDS; do
  echo "RUNNER: killing stale title pid $p"
  MSYS_NO_PATHCONV=1 taskkill /F /PID "$p" >/dev/null 2>&1
done
sleep 3
LEFT=$(tasklist 2>/dev/null | grep -ci "game_scarlett_master")
[ "$LEFT" = "0" ] || fail "stale title process still alive ($LEFT)"

# 2. Replace the overlay DLL and prove the bytes match.
cp "$DLL" "$OVERLAY/d3d12_xs.dll" || fail "copying d3d12_xs.dll into $OVERLAY"
A=$(sha256sum "$DLL" | awk '{print $1}')
B=$(sha256sum "$OVERLAY/d3d12_xs.dll" | awk '{print $1}')
[ "$A" = "$B" ] || fail "overlay d3d12_xs.dll does not match the build ($A vs $B)"
echo "RUNNER: overlay dll sha256 $A"

# 3. Launch, record the log size first so growth is measurable.
rm -f "$LOG"
: > "$LOG"
BEFORE=$(wc -c < "$LOG")
SPEC="${2:-}"
if [ -n "$SPEC" ]; then
  TOMBOX_TITLE_HEAP_TBL=1 TOMBOX_LOG="$(cygpath -w "$LOG" 2>/dev/null || echo "$LOG")" \
  TOMBOX_PASSCAP="$SPEC" TOMBOX_PASSCAP_DIR="$(cygpath -w "$OVERLAY/$NAME" 2>/dev/null || echo "$OVERLAY/$NAME")" \
  MDBG_LIMIT=300 "$MDBG" "$GAME" > "$CONSOLE" 2>&1 &
else
  TOMBOX_TITLE_HEAP_TBL=1 TOMBOX_LOG="$(cygpath -w "$LOG" 2>/dev/null || echo "$LOG")" \
  MDBG_LIMIT=300 "$MDBG" "$GAME" > "$CONSOLE" 2>&1 &
fi
PID=$!
echo "RUNNER: launched mdbg pid $PID, log $LOG"

# 4. Block until the log grows past its pre-launch size, or fail loudly.
for _ in $(seq 1 120); do
  kill -0 "$PID" 2>/dev/null || break
  NOW=$(wc -c < "$LOG" 2>/dev/null || echo 0)
  if [ "$NOW" -gt "$BEFORE" ]; then
    echo "RUNNER: log grew to $NOW bytes"
    echo "RUNNER: OK pid=$PID log=$LOG console=$CONSOLE"
    exit 0
  fi
  sleep 1
done
NOW=$(wc -c < "$LOG" 2>/dev/null || echo 0)
[ "$NOW" -gt "$BEFORE" ] || fail "log did not grow (size $NOW, was $BEFORE); see $CONSOLE"
echo "RUNNER: OK pid=$PID log=$LOG console=$CONSOLE"
