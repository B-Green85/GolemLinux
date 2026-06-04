#!/usr/bin/env bash
#
# verify_boot.sh — Automated boot verification test for Golem Linux (Phase 3)
#
# Boots the Golem Linux image under QEMU (headless), captures the serial
# console to qemu.log, and verifies that the kernel/init reaches the
# "Golem Linux ready" milestone within a hard 30-second time limit.
#
#   exit 0  -> "Golem Linux ready" was seen   (PASS — boot verified)
#   exit 1  -> not seen within the deadline   (FAIL — boot failure or timeout)
#
# This script is fully non-interactive and is intended to be run after every
# Phase 3 change (locally or in CI). All paths/binaries can be overridden via
# environment variables so it adapts to whatever the other agents produce.
#
# Configuration (environment overrides):
#   QEMU_BIN     qemu binary           (default: qemu-system-x86_64)
#   KERNEL       kernel image          (default: build/bzImage)
#   INITRD       optional initrd/initramfs image
#   DISK         optional raw disk image
#   KERNEL_APPEND  kernel cmdline      (default: "console=ttyS0 panic=-1")
#   QEMU_MEM     guest memory          (default: 256M)
#   QEMU_EXTRA   extra raw qemu args   (default: empty)
#   QEMU_LOG     serial log path       (default: qemu.log)
#   READY_STRING marker to look for    (default: "Golem Linux ready")
#   BOOT_TIMEOUT timeout in seconds    (default: 30, hard cap)

set -u

# --- Configuration ----------------------------------------------------------
BOOT_TIMEOUT="${BOOT_TIMEOUT:-30}"
READY_STRING="${READY_STRING:-Golem Linux ready}"
LOGFILE="${QEMU_LOG:-qemu.log}"

QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
# Use ${KERNEL-...} (not :-) so an explicitly empty KERNEL="" means "boot from
# DISK, no -kernel" rather than silently falling back to the default image.
KERNEL="${KERNEL-build/bzImage}"
KERNEL_APPEND="${KERNEL_APPEND:-console=ttyS0 panic=-1}"
QEMU_MEM="${QEMU_MEM:-256M}"

# --- Build the QEMU command -------------------------------------------------
# Assemble argv as an array so paths with spaces survive. -nographic routes the
# serial console to stdout (which we capture); -monitor none and stdin from
# /dev/null guarantee QEMU never blocks waiting for input; -no-reboot/-no-shutdown
# stop the guest from looping after it reaches the milestone.
qemu_cmd=(
  "$QEMU_BIN"
  -nographic
  -monitor none
  -no-reboot
  -no-shutdown
  -m "$QEMU_MEM"
)

if [ -n "${KERNEL:-}" ]; then
  qemu_cmd+=(-kernel "$KERNEL")
fi
if [ -n "${INITRD:-}" ]; then
  qemu_cmd+=(-initrd "$INITRD")
fi
if [ -n "${DISK:-}" ]; then
  qemu_cmd+=(-drive "file=$DISK,format=raw")
fi
if [ -n "${KERNEL:-}" ]; then
  qemu_cmd+=(-append "$KERNEL_APPEND")
fi
# QEMU_EXTRA lets a caller append arbitrary extra args (word-split intentionally).
if [ -n "${QEMU_EXTRA:-}" ]; then
  # shellcheck disable=SC2206
  qemu_cmd+=(${QEMU_EXTRA})
fi

# --- Sanity checks ----------------------------------------------------------
fail() {
  echo "===== qemu.log ====="
  [ -f "$LOGFILE" ] && cat "$LOGFILE"
  echo "===================="
  echo "FAIL: $1"
  exit 1
}

if ! command -v "$QEMU_BIN" >/dev/null 2>&1; then
  : > "$LOGFILE"
  fail "QEMU binary '$QEMU_BIN' not found in PATH"
fi
if [ -n "${KERNEL:-}" ] && [ ! -f "$KERNEL" ]; then
  : > "$LOGFILE"
  fail "kernel image '$KERNEL' not found (set KERNEL=... to override)"
fi

# --- Launch QEMU (background) so we can enforce the hard timeout ourselves ---
: > "$LOGFILE"   # truncate any previous log

qemu_pid=""
cleanup() {
  if [ -n "$qemu_pid" ] && kill -0 "$qemu_pid" 2>/dev/null; then
    kill "$qemu_pid" 2>/dev/null
    # Give QEMU a moment to exit, then force-kill.
    for _ in 1 2 3; do
      kill -0 "$qemu_pid" 2>/dev/null || break
      sleep 1
    done
    kill -9 "$qemu_pid" 2>/dev/null
  fi
}
trap cleanup EXIT INT TERM

echo "Booting Golem Linux under QEMU (timeout: ${BOOT_TIMEOUT}s)..."
echo "  cmd: ${qemu_cmd[*]}"

"${qemu_cmd[@]}" </dev/null >"$LOGFILE" 2>&1 &
qemu_pid=$!

# --- Poll the serial log for the readiness marker, bounded by the deadline ---
found=0
SECONDS=0
while [ "$SECONDS" -lt "$BOOT_TIMEOUT" ]; do
  if grep -qF "$READY_STRING" "$LOGFILE" 2>/dev/null; then
    found=1
    break
  fi
  # If QEMU exited on its own, do one last check and stop waiting.
  if ! kill -0 "$qemu_pid" 2>/dev/null; then
    grep -qF "$READY_STRING" "$LOGFILE" 2>/dev/null && found=1
    break
  fi
  sleep 1
done

# Final check guards against the marker landing in the last fraction of a second.
if [ "$found" -eq 0 ] && grep -qF "$READY_STRING" "$LOGFILE" 2>/dev/null; then
  found=1
fi

# Stop QEMU now that we have a verdict (trap also covers abnormal exits).
cleanup
qemu_pid=""
trap - EXIT INT TERM

# --- Report -----------------------------------------------------------------
echo "===== qemu.log ====="
cat "$LOGFILE"
echo "===================="

if [ "$found" -eq 1 ]; then
  echo "PASS: found '$READY_STRING' — boot verified in ${SECONDS}s"
  exit 0
else
  echo "FAIL: '$READY_STRING' not found within ${BOOT_TIMEOUT}s (boot failure or timeout)"
  exit 1
fi
