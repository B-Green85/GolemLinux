#!/usr/bin/env bash
#
# run_safemode.sh — Boot the Golem Linux SAFE MODE image (golem-safemode.img)
#                   under QEMU with UEFI firmware.
#
# Usage:
#   ./scripts/run_safemode.sh [path/to/golem-safemode.img]
#
# This is the Safe Mode counterpart to run_qemu.sh and is modelled on its QEMU
# invocation. Differences from the main launcher:
#   * Defaults to ./golem-safemode.img (NOT golem.img). Safe Mode and the main
#     OS image are kept strictly separate and must never be mixed.
#   * Prints a clear Safe Mode banner on launch.
#   * Sets a distinct QEMU guest name / window title and a distinct host
#     terminal title so this instance is visually unmistakable next to a main
#     OS instance running in another window.
#
# The serial console is wired to stdio so the kernel's serial output appears in
# this terminal and the operator can type Safe Mode configuration commands here.
# A full transcript is written to qemu-safemode.log.
#
# Works on macOS with a brew-installed QEMU (`brew install qemu`). Linux hosts
# with QEMU + OVMF installed are also supported via firmware autodetection.

set -euo pipefail

# --- Resolve the disk image ------------------------------------------------
IMG="${1:-golem-safemode.img}"
if [[ ! -f "$IMG" ]]; then
  echo "error: safe mode disk image not found: $IMG" >&2
  echo "       build it first with scripts/build_safemode_image.sh," >&2
  echo "       or pass the path as the first argument." >&2
  exit 1
fi

# Guard against mixing images: this launcher is for Safe Mode only. If someone
# points it at the main OS image (golem.img), stop — they almost certainly want
# scripts/run_qemu.sh instead.
if [[ "$(basename "$IMG")" == "golem.img" ]]; then
  echo "error: '$IMG' is the MAIN OS image, not the Safe Mode image." >&2
  echo "       Use scripts/run_qemu.sh to boot the main OS, or pass" >&2
  echo "       golem-safemode.img here. Safe Mode and the main OS must" >&2
  echo "       never be mixed." >&2
  exit 1
fi

# --- Safe Mode banner ------------------------------------------------------
# A loud, unmistakable banner so the operator always knows which instance this
# terminal is driving.
print_banner() {
  printf '\033[1;33m'   # bold yellow
  printf '======================================================\n'
  printf '   GOLEM SAFE MODE — Agent processes disabled\n'
  printf '======================================================\033[0m\n'
}
print_banner

# Set a distinct HOST terminal window/tab title (visible even with QEMU running
# headless), so this terminal is easy to tell apart from a main OS terminal.
printf '\033]0;GOLEM SAFE MODE\007'

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
echo "Booting Safe Mode image: $IMG" >&2

# --- Launch QEMU -----------------------------------------------------------
# Flag-by-flag explanation (matches run_qemu.sh, with Safe Mode distinctions):
#
#   -name "GOLEM SAFE MODE",process=golem-safemode
#       Sets a DISTINCT QEMU guest name. This becomes the GUI window title (if a
#       display is ever attached) and the process name, so the Safe Mode
#       instance is visually/identifiably separate from the main OS instance.
#   -drive if=pflash,format=raw,readonly=on,file="$OVMF"
#       Maps the OVMF firmware as a read-only pflash device so the VM boots via
#       UEFI. readonly=on prevents the firmware blob from being modified.
#   -drive format=raw,file="$IMG"
#       Attaches golem-safemode.img as the primary raw disk (the thing to boot).
#   -m 256M
#       Gives the guest 256 MiB of RAM.
#   -serial stdio
#       Connects the guest's first serial port to this terminal's stdin/stdout,
#       so the kernel's serial console output is visible and the operator can
#       type Safe Mode configuration commands here.
#   -display none
#       Disables the graphical window — this is a headless, serial-only run.
#   -no-reboot
#       Makes QEMU exit instead of rebooting on a guest reset/triple-fault,
#       so a boot loop doesn't trap us and panics are observable.
#   -d int,cpu_reset
#       Enables QEMU's debug logging for interrupts/exceptions and CPU resets —
#       invaluable for diagnosing early-boot faults and triple faults.
#   2>&1 | tee qemu-safemode.log
#       Merges stderr into stdout and tees the whole session to qemu-safemode.log
#       (distinct from the main OS's qemu.log) while showing it live.
qemu-system-x86_64 \
  -name "GOLEM SAFE MODE",process=golem-safemode \
  -drive if=pflash,format=raw,readonly=on,file="$OVMF" \
  -drive format=raw,file="$IMG" \
  -m 256M \
  -serial stdio \
  -display none \
  -no-reboot \
  -d int,cpu_reset \
  2>&1 | tee qemu-safemode.log
