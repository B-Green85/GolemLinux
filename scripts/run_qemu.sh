#!/usr/bin/env bash
#
# run_qemu.sh — Boot the Golem Linux image (golem.img) under QEMU with UEFI firmware.
#
# Usage:
#   ./scripts/run_qemu.sh [path/to/golem.img]
#
# Defaults to ./golem.img relative to the current working directory if no
# argument is given. Serial console is wired to stdio so kernel output appears
# in your terminal, and a full transcript is written to qemu.log.
#
# Works on macOS with a brew-installed QEMU (`brew install qemu`). Linux hosts
# with QEMU + OVMF installed are also supported via firmware autodetection.

set -euo pipefail

# --- Resolve the disk image ------------------------------------------------
IMG="${1:-golem.img}"
if [[ ! -f "$IMG" ]]; then
  echo "error: disk image not found: $IMG" >&2
  echo "       build it first, or pass the path as the first argument." >&2
  exit 1
fi

# --- Locate the UEFI firmware (OVMF) ---------------------------------------
# The QEMU x86_64 machine needs UEFI firmware to boot a GPT/UEFI image. The
# original spec points at /usr/local/share/qemu/OVMF.fd, but that file does not
# exist on every host:
#   * Apple Silicon brew installs under /opt/homebrew, not /usr/local.
#   * brew's QEMU ships the OVMF build as edk2-x86_64-code.fd, not OVMF.fd.
# So we probe a list of well-known locations and, failing that, search the
# common install prefixes for either naming scheme.
find_ovmf() {
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
    [[ -f "$c" ]] && { echo "$c"; return 0; }
  done

  # Fall back to a filesystem search across the usual prefixes, matching either
  # the classic OVMF*.fd name or brew's edk2-x86_64-code.fd.
  local hit
  hit="$(find /usr/local /opt/homebrew /usr/share \
            \( -name 'OVMF*.fd' -o -name 'edk2-x86_64-code.fd' \) \
            2>/dev/null | head -n 1)"
  [[ -n "$hit" ]] && { echo "$hit"; return 0; }
  return 1
}

if ! OVMF="$(find_ovmf)"; then
  echo "error: could not find OVMF/UEFI firmware." >&2
  echo "       install it (macOS: 'brew install qemu') or set the path manually." >&2
  exit 1
fi
echo "Using UEFI firmware: $OVMF" >&2

# --- Launch QEMU -----------------------------------------------------------
# Flag-by-flag explanation:
#
#   -drive if=pflash,format=raw,readonly=on,file="$OVMF"
#       Maps the OVMF firmware as a read-only pflash device so the VM boots via
#       UEFI. readonly=on prevents the firmware blob from being modified.
#   -drive format=raw,file="$IMG"
#       Attaches golem.img as the primary raw disk (the thing we want to boot).
#   -m 256M
#       Gives the guest 256 MiB of RAM.
#   -serial stdio
#       Connects the guest's first serial port to this terminal's stdin/stdout,
#       so the kernel's serial console output is visible (and interactive).
#   -display none
#       Disables the graphical window — this is a headless, serial-only run.
#   -no-reboot
#       Makes QEMU exit instead of rebooting on a guest reset/triple-fault,
#       so a boot loop doesn't trap us and panics are observable.
#   -d int,cpu_reset
#       Enables QEMU's debug logging for interrupts/exceptions and CPU resets —
#       invaluable for diagnosing early-boot faults and triple faults.
#   2>&1 | tee qemu.log
#       Merges stderr into stdout and tees the whole session to qemu.log while
#       still showing it live in the terminal.
qemu-system-x86_64 \
  -drive if=pflash,format=raw,readonly=on,file="$OVMF" \
  -drive format=raw,file="$IMG" \
  -m 256M \
  -serial stdio \
  -display none \
  -no-reboot \
  -d int,cpu_reset \
  2>&1 | tee qemu.log
