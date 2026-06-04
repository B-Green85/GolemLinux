#!/usr/bin/env bash
#
# run_phase4_demo.sh — One-command launcher for the Golem Linux Phase 4 demo.
#
# This orchestrates the full "Sentinel handshake + degradation" demo so the
# whole thing is a single command:
#
#   ./scripts/run_phase4_demo.sh
#
# It performs, in order:
#   1. Start QEMU with Golem Linux in the background, exposing the two
#      virtio-serial host sockets the Python tooling connects to:
#        * /tmp/golem-sentinel.sock  — the Sentinel registration channel
#                                      (golem_agent.py, Agent 2)
#        * /tmp/golem-audit.sock     — the audit-log streaming channel
#                                      (audit_viewer.py, Agent 4)
#   2. Poll the serial transcript (golem.log) until the kernel prints
#      "Golem Linux ready" — the boot milestone emitted by src/main.rs.
#   3. Open a NEW Terminal.app window (via osascript) running audit_viewer.py
#      so the operator watches the SHA-256 audit chain build in real time.
#   4. Wait 2 seconds so the viewer has time to attach to its channel.
#   5. Run golem_agent.py in the foreground — it registers with Sentinel and
#      then emits the escalating degradation scenarios.
#   6. Wait for the agent to finish (foreground exit == completion).
#   7. Print "DEMO COMPLETE".
#
# Everything after the initial launch is non-interactive.
#
# ---------------------------------------------------------------------------
# Configuration (all overridable via environment variables)
# ---------------------------------------------------------------------------
#   GOLEM_IMG        disk image to boot          (default: <repo>/golem.img)
#   GOLEM_LOG        serial transcript to poll   (default: <repo>/golem.log)
#   QEMU_BIN         qemu binary                 (default: qemu-system-x86_64)
#   QEMU_MEM         guest memory                (default: 256M)
#   OVMF             UEFI firmware path          (default: autodetected)
#   READY_STRING     boot milestone to wait for  (default: "Golem Linux ready")
#   BOOT_TIMEOUT     seconds to wait for boot    (default: 90)
#   SENTINEL_SOCK    handshake socket path       (default: /tmp/golem-sentinel.sock)
#   AUDIT_SOCK       audit stream socket path    (default: /tmp/golem-audit.sock)
#   AGENT_SCRIPT     host agent script           (default: <repo>/examples/golem_agent.py)
#   VIEWER_SCRIPT    audit viewer script         (default: <repo>/tools/audit_viewer.py)
#   PYTHON_BIN       python interpreter          (default: python3)
#   VIEWER_TERMINAL  1=open viewer in new window (default: 1; set 0 to skip)
#   KEEP_QEMU        1=leave QEMU running at end  (default: 0; we shut it down)
#
# Target host: macOS (the new-terminal step uses osascript/Terminal.app).
#
set -euo pipefail

# ---------------------------------------------------------------------------
# Locate the repository root from this script's own location, so the demo can
# be launched from anywhere (./scripts/run_phase4_demo.sh, an absolute path,
# a symlink, etc.) and still resolve golem.img and the Python tooling.
# ---------------------------------------------------------------------------
SCRIPT_SOURCE="${BASH_SOURCE[0]}"
while [ -h "$SCRIPT_SOURCE" ]; do
  dir="$(cd -P "$(dirname "$SCRIPT_SOURCE")" >/dev/null 2>&1 && pwd)"
  SCRIPT_SOURCE="$(readlink "$SCRIPT_SOURCE")"
  [[ "$SCRIPT_SOURCE" != /* ]] && SCRIPT_SOURCE="$dir/$SCRIPT_SOURCE"
done
SCRIPT_DIR="$(cd -P "$(dirname "$SCRIPT_SOURCE")" >/dev/null 2>&1 && pwd)"
REPO_ROOT="$(cd -P "$SCRIPT_DIR/.." >/dev/null 2>&1 && pwd)"

# ---------------------------------------------------------------------------
# Configuration with environment overrides.
# ---------------------------------------------------------------------------
GOLEM_IMG="${GOLEM_IMG:-$REPO_ROOT/golem.img}"
GOLEM_LOG="${GOLEM_LOG:-$REPO_ROOT/golem.log}"
QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
QEMU_MEM="${QEMU_MEM:-256M}"
READY_STRING="${READY_STRING:-Golem Linux ready}"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-90}"
SENTINEL_SOCK="${SENTINEL_SOCK:-/tmp/golem-sentinel.sock}"
AUDIT_SOCK="${AUDIT_SOCK:-/tmp/golem-audit.sock}"
AGENT_SCRIPT="${AGENT_SCRIPT:-$REPO_ROOT/examples/golem_agent.py}"
VIEWER_SCRIPT="${VIEWER_SCRIPT:-$REPO_ROOT/tools/audit_viewer.py}"
PYTHON_BIN="${PYTHON_BIN:-python3}"
VIEWER_TERMINAL="${VIEWER_TERMINAL:-1}"
KEEP_QEMU="${KEEP_QEMU:-0}"

# virtio-serial port names. The Sentinel name matches Agent 1's IPC interface
# (org.truesystems.sentinel.0); the audit name is a sibling channel on the same
# virtio-serial bus dedicated to streaming the audit log to audit_viewer.py.
SENTINEL_PORT_NAME="${SENTINEL_PORT_NAME:-org.truesystems.sentinel.0}"
AUDIT_PORT_NAME="${AUDIT_PORT_NAME:-org.truesystems.sentinel.audit.0}"

# ---------------------------------------------------------------------------
# Small logging helpers.
# ---------------------------------------------------------------------------
say()  { printf '\033[1;36m[demo]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[demo] warning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[demo] error:\033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Locate the UEFI firmware (OVMF). Golem boots as a UEFI/GPT image, so QEMU
# needs OVMF firmware. The naming/location differs across hosts (Apple Silicon
# brew vs Intel brew vs Linux distros), so probe known paths then fall back to
# a search. Mirrors scripts/run_qemu.sh so both stay in sync.
# ---------------------------------------------------------------------------
find_ovmf() {
  if [ -n "${OVMF:-}" ]; then
    [ -f "$OVMF" ] && { echo "$OVMF"; return 0; }
    return 1
  fi
  local candidates=(
    /usr/local/share/qemu/OVMF.fd
    /opt/homebrew/opt/qemu/share/qemu/edk2-x86_64-code.fd
    /opt/homebrew/share/qemu/edk2-x86_64-code.fd
    /usr/local/opt/qemu/share/qemu/edk2-x86_64-code.fd
    /usr/local/share/qemu/edk2-x86_64-code.fd
    /usr/share/OVMF/OVMF_CODE.fd
    /usr/share/edk2/x64/OVMF_CODE.fd
    /usr/share/qemu/OVMF.fd
  )
  local c
  for c in "${candidates[@]}"; do
    [ -f "$c" ] && { echo "$c"; return 0; }
  done
  local hit
  hit="$(find /usr/local /opt/homebrew /usr/share \
            \( -name 'OVMF*.fd' -o -name 'edk2-x86_64-code.fd' \) \
            2>/dev/null | head -n 1)"
  [ -n "$hit" ] && { echo "$hit"; return 0; }
  return 1
}

# ---------------------------------------------------------------------------
# Preflight checks — fail early with a clear message rather than a cryptic
# QEMU/Python error halfway through.
# ---------------------------------------------------------------------------
command -v "$QEMU_BIN"   >/dev/null 2>&1 || die "QEMU binary '$QEMU_BIN' not found in PATH (macOS: 'brew install qemu')."
command -v "$PYTHON_BIN" >/dev/null 2>&1 || die "Python interpreter '$PYTHON_BIN' not found in PATH."
command -v osascript     >/dev/null 2>&1 || warn "osascript not found — the audit viewer cannot open in a new window (is this macOS?)."

[ -f "$GOLEM_IMG" ] || die "disk image not found: $GOLEM_IMG (build it with scripts/build_image.sh, or set GOLEM_IMG=...)."

if [ ! -f "$VIEWER_SCRIPT" ]; then
  warn "audit viewer not found: $VIEWER_SCRIPT — the demo will run without the live viewer."
  VIEWER_TERMINAL=0
fi
[ -f "$AGENT_SCRIPT" ] || die "host agent not found: $AGENT_SCRIPT (Agent 2's examples/golem_agent.py). Set AGENT_SCRIPT=... to override."

OVMF_PATH="$(find_ovmf)" || die "could not find OVMF/UEFI firmware (macOS: 'brew install qemu', or set OVMF=/path/to/firmware)."
say "Using UEFI firmware: $OVMF_PATH"

# ---------------------------------------------------------------------------
# Cleanup: ensure we tear QEMU down on exit / Ctrl-C so the demo never leaves
# an orphaned VM (and a stale socket) behind. The audit viewer lives in its own
# Terminal window; it exits on its own when its channel closes.
# ---------------------------------------------------------------------------
QEMU_PID=""
cleanup() {
  if [ "$KEEP_QEMU" != "1" ] && [ -n "$QEMU_PID" ] && kill -0 "$QEMU_PID" 2>/dev/null; then
    say "Shutting down QEMU (pid $QEMU_PID)..."
    kill "$QEMU_PID" 2>/dev/null || true
    for _ in 1 2 3 4 5; do
      kill -0 "$QEMU_PID" 2>/dev/null || break
      sleep 1
    done
    kill -9 "$QEMU_PID" 2>/dev/null || true
  fi
  # Remove the unix sockets QEMU created so a re-run can bind them cleanly.
  rm -f "$SENTINEL_SOCK" "$AUDIT_SOCK" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Pre-clean any stale sockets from a previous (crashed) run so QEMU's
# server=on can bind them.
rm -f "$SENTINEL_SOCK" "$AUDIT_SOCK" 2>/dev/null || true

# ===========================================================================
# STEP 1 — Start QEMU with Golem Linux in the background.
# ===========================================================================
# Flags:
#   -drive if=pflash,...,file="$OVMF_PATH"   UEFI firmware (read-only).
#   -drive format=raw,file="$GOLEM_IMG"      the bootable Golem disk image.
#   -m "$QEMU_MEM"                           guest RAM.
#   -serial stdio                            kernel serial console -> our stdout,
#                                            which we redirect to golem.log.
#   -display none                            headless (serial only).
#   -no-reboot                               exit (don't loop) on guest reset,
#                                            so panics are observable.
#   -device virtio-serial                    one virtio-serial bus carrying the
#                                            two governance channels below.
#   -chardev socket,...,server=on,wait=off   host-side unix sockets; server=on
#                                            so the Python tools are the clients,
#                                            wait=off so QEMU boots without
#                                            blocking for a connection.
#   -device virtserialport,chardev=...,name= the named guest ports the kernel
#                                            (Agent 1) opens for Sentinel + audit.
# stdin is /dev/null and stdout/stderr go to golem.log so the run is fully
# non-interactive and the boot transcript is pollable.
say "Booting Golem Linux under QEMU (timeout ${BOOT_TIMEOUT}s)..."
say "  image:        $GOLEM_IMG"
say "  serial log:   $GOLEM_LOG"
say "  sentinel sock: $SENTINEL_SOCK"
say "  audit sock:    $AUDIT_SOCK"

: > "$GOLEM_LOG"   # truncate any previous transcript

"$QEMU_BIN" \
  -drive "if=pflash,format=raw,readonly=on,file=$OVMF_PATH" \
  -drive "format=raw,file=$GOLEM_IMG" \
  -m "$QEMU_MEM" \
  -serial stdio \
  -display none \
  -no-reboot \
  -device virtio-serial \
  -chardev "socket,path=$SENTINEL_SOCK,server=on,wait=off,id=golem" \
  -device "virtserialport,chardev=golem,name=$SENTINEL_PORT_NAME" \
  -chardev "socket,path=$AUDIT_SOCK,server=on,wait=off,id=golemaudit" \
  -device "virtserialport,chardev=golemaudit,name=$AUDIT_PORT_NAME" \
  </dev/null >"$GOLEM_LOG" 2>&1 &
QEMU_PID=$!
say "QEMU started (pid $QEMU_PID)."

# ===========================================================================
# STEP 2 — Wait for "Golem Linux ready" in the serial output (poll golem.log).
# ===========================================================================
say "Waiting for milestone: \"$READY_STRING\""
ready=0
elapsed=0
while [ "$elapsed" -lt "$BOOT_TIMEOUT" ]; do
  if grep -qF "$READY_STRING" "$GOLEM_LOG" 2>/dev/null; then
    ready=1
    break
  fi
  # If QEMU died, do one last check then stop waiting — no point polling a log
  # that will never grow again.
  if ! kill -0 "$QEMU_PID" 2>/dev/null; then
    grep -qF "$READY_STRING" "$GOLEM_LOG" 2>/dev/null && ready=1
    break
  fi
  sleep 1
  elapsed=$((elapsed + 1))
done

if [ "$ready" -ne 1 ]; then
  echo "----- golem.log (tail) -----" >&2
  tail -n 40 "$GOLEM_LOG" >&2 2>/dev/null || true
  echo "----------------------------" >&2
  die "kernel did not report \"$READY_STRING\" within ${BOOT_TIMEOUT}s (boot failure or timeout)."
fi
say "Golem Linux is ready (booted in ${elapsed}s)."

# ===========================================================================
# STEP 3 — Start audit_viewer.py in a NEW Terminal window (macOS osascript).
# ===========================================================================
# The viewer streams the SHA-256 audit chain in real time so the operator can
# watch governance happen. We launch it in its own window via Terminal.app so
# this orchestrator's terminal stays free for the agent's output.
if [ "$VIEWER_TERMINAL" = "1" ]; then
  if command -v osascript >/dev/null 2>&1; then
    say "Opening audit viewer in a new Terminal window..."
    # Build the shell command the new window will run. Single-quote each path
    # for the inner shell, then escape embedded double quotes for AppleScript's
    # "do script" string literal.
    viewer_cmd="cd '$REPO_ROOT' && AUDIT_SOCK='$AUDIT_SOCK' '$PYTHON_BIN' '$VIEWER_SCRIPT'"
    osa_cmd="${viewer_cmd//\\/\\\\}"   # escape backslashes
    osa_cmd="${osa_cmd//\"/\\\"}"      # escape double quotes for AppleScript
    osascript \
      -e "tell application \"Terminal\" to do script \"$osa_cmd\"" \
      -e 'tell application "Terminal" to activate' \
      >/dev/null 2>&1 \
      || warn "failed to open the audit viewer window (continuing without it)."
  else
    warn "osascript unavailable — skipping the live audit viewer window."
  fi
else
  say "Audit viewer window disabled (VIEWER_TERMINAL=$VIEWER_TERMINAL)."
fi

# ===========================================================================
# STEP 4 — Wait 2 seconds for the viewer to attach to its channel.
# ===========================================================================
say "Giving the audit viewer a moment to connect..."
sleep 2

# ===========================================================================
# STEP 5 — Start golem_agent.py (foreground).
# ===========================================================================
# The agent registers with Sentinel over the handshake socket, then emits the
# escalating degradation scenarios. Running it in the foreground means STEP 6
# (wait for completion) is just "wait for this command to return".
say "Launching the Golem demo agent..."
echo
agent_rc=0
SENTINEL_SOCK="$SENTINEL_SOCK" "$PYTHON_BIN" "$AGENT_SCRIPT" || agent_rc=$?
echo

# ===========================================================================
# STEP 6 — Agent completion is the foreground command returning above.
# ===========================================================================
if [ "$agent_rc" -ne 0 ]; then
  warn "golem_agent.py exited with status $agent_rc (this may be expected if Sentinel terminated the agent)."
fi

# ===========================================================================
# STEP 7 — Done.
# ===========================================================================
say "Serial transcript saved to: $GOLEM_LOG"
echo
echo "============================================================"
echo "                      DEMO COMPLETE"
echo "============================================================"

# cleanup() runs on EXIT and tears QEMU down.
exit 0
