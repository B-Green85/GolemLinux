#!/usr/bin/env bash
#
# flash_usb.sh — Write golem.img to a USB drive (macOS)
#
# Part of Golem Linux Phase 6.
# Target hardware: 2017 MacBook Pro 13" (Intel Kaby Lake, UEFI, no T2 chip).
#
# This script:
#   1. Lists available USB drives and asks the operator to confirm the target.
#   2. Warns that the selected drive will be ERASED.
#   3. Requires the operator to type "CONFIRM" before proceeding.
#   4. Uses dd to write golem.img to the selected drive.
#   5. Syncs and ejects cleanly.
#   6. Reports the SHA256 of the written image for verification.
#
# Usage:
#   ./flash_usb.sh [path/to/golem.img]
#
# If no image path is given, it defaults to ./golem.img (relative to the
# current working directory).
#
set -euo pipefail

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
err()  { printf '\033[1;31m%s\033[0m\n' "$*" >&2; }
warn() { printf '\033[1;33m%s\033[0m\n' "$*" >&2; }
info() { printf '\033[1;36m%s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m%s\033[0m\n' "$*"; }

die() { err "ERROR: $*"; exit 1; }

# ---------------------------------------------------------------------------
# Sanity checks
# ---------------------------------------------------------------------------
if [[ "$(uname -s)" != "Darwin" ]]; then
    die "This script is intended for macOS (Darwin). Detected: $(uname -s)."
fi

for tool in diskutil dd shasum awk; do
    command -v "$tool" >/dev/null 2>&1 || die "Required tool not found: $tool"
done

# ---------------------------------------------------------------------------
# Resolve the image to write
# ---------------------------------------------------------------------------
IMAGE="${1:-golem.img}"

[[ -f "$IMAGE" ]] || die "Image file not found: $IMAGE"
[[ -r "$IMAGE" ]] || die "Image file not readable: $IMAGE"

# Absolute path for clarity in prompts.
IMAGE_ABS="$(cd "$(dirname "$IMAGE")" && pwd)/$(basename "$IMAGE")"
IMAGE_SIZE_BYTES="$(stat -f%z "$IMAGE_ABS")"
IMAGE_SIZE_HUMAN="$(awk -v b="$IMAGE_SIZE_BYTES" 'BEGIN{
    split("B KB MB GB TB", u, " ");
    i=1; while (b>=1024 && i<5){ b/=1024; i++ }
    printf "%.2f %s", b, u[i]
}')"

info "Image to write : $IMAGE_ABS"
info "Image size     : $IMAGE_SIZE_BYTES bytes ($IMAGE_SIZE_HUMAN)"

# ---------------------------------------------------------------------------
# Compute SHA256 of the source image up front
# ---------------------------------------------------------------------------
info "Computing SHA256 of the source image (this may take a moment)..."
SRC_SHA256="$(shasum -a 256 "$IMAGE_ABS" | awk '{print $1}')"
ok   "Source SHA256  : $SRC_SHA256"
echo

# ---------------------------------------------------------------------------
# Enumerate external/physical USB drives
# ---------------------------------------------------------------------------
info "Scanning for external USB drives..."
echo

# diskutil list -plist external physical would be cleaner, but to avoid a
# plist parser dependency we collect external physical whole-disk identifiers
# and describe each one with diskutil info.
# NOTE: written for bash 3.2 (stock macOS) — no mapfile/readarray.
CANDIDATES=()
while IFS= read -r line; do
    [[ -n "$line" ]] && CANDIDATES+=("$line")
done < <(diskutil list external physical 2>/dev/null \
    | awk '/^\/dev\/disk[0-9]+/ {sub("^/dev/",""); print $1}')

if [[ "${#CANDIDATES[@]}" -eq 0 ]]; then
    die "No external physical disks found. Insert the USB drive and retry."
fi

# Build a human-readable menu.
declare -a MENU_IDS
idx=0
for disk in "${CANDIDATES[@]}"; do
    devinfo="$(diskutil info "/dev/$disk" 2>/dev/null || true)"
    name="$(printf '%s\n' "$devinfo" | awk -F': *' '/Device \/ Media Name/ {print $2; exit}')"
    size="$(printf '%s\n' "$devinfo" | awk -F': *' '/Disk Size/ {print $2; exit}')"
    removable="$(printf '%s\n' "$devinfo" | awk -F': *' '/Removable Media/ {print $2; exit}')"
    protocol="$(printf '%s\n' "$devinfo" | awk -F': *' '/Protocol/ {print $2; exit}')"
    internal="$(printf '%s\n' "$devinfo" | awk -F': *' '/Internal/ {print $2; exit}')"

    # Belt-and-suspenders: never offer an internal disk as a target.
    if [[ "$internal" == "Yes" ]]; then
        continue
    fi

    MENU_IDS+=("$disk")
    printf '  [%d] /dev/%s\n' "$idx" "$disk"
    printf '        Name      : %s\n' "${name:-unknown}"
    printf '        Size      : %s\n' "${size:-unknown}"
    printf '        Protocol  : %s\n' "${protocol:-unknown}"
    printf '        Removable : %s\n' "${removable:-unknown}"
    echo
    idx=$((idx + 1))
done

if [[ "${#MENU_IDS[@]}" -eq 0 ]]; then
    die "No eligible external USB drives found after filtering internal disks."
fi

# ---------------------------------------------------------------------------
# Operator selects the target
# ---------------------------------------------------------------------------
SELECTION=""
while true; do
    printf 'Enter the number of the target drive (or "q" to quit): '
    read -r SELECTION
    [[ "$SELECTION" == "q" || "$SELECTION" == "Q" ]] && { info "Aborted by operator."; exit 0; }
    if [[ "$SELECTION" =~ ^[0-9]+$ ]] && (( SELECTION >= 0 && SELECTION < ${#MENU_IDS[@]} )); then
        break
    fi
    warn "Invalid selection. Please enter a number between 0 and $(( ${#MENU_IDS[@]} - 1 ))."
done

TARGET_DISK="${MENU_IDS[$SELECTION]}"
TARGET_DEV="/dev/$TARGET_DISK"
# Raw device is dramatically faster for dd on macOS.
TARGET_RDEV="/dev/r$TARGET_DISK"

echo
info "Selected target: $TARGET_DEV"
diskutil info "$TARGET_DEV" 2>/dev/null \
    | awk -F': *' '/Device \/ Media Name|Disk Size|Protocol|Removable Media|Internal/ {printf "    %-22s %s\n", $1":", $2}'
echo

# ---------------------------------------------------------------------------
# Final, mandatory confirmation
# ---------------------------------------------------------------------------
warn "============================================================"
warn "  WARNING: THIS WILL ERASE THE SELECTED DRIVE"
warn "  Target : $TARGET_DEV"
warn "  Image  : $IMAGE_ABS"
warn ""
warn "  ALL DATA ON $TARGET_DEV WILL BE PERMANENTLY DESTROYED."
warn "  Double-check this is the USB drive and NOT an internal disk."
warn "============================================================"
echo

printf 'Type CONFIRM (all caps) to proceed, anything else to abort: '
read -r CONFIRMATION
if [[ "$CONFIRMATION" != "CONFIRM" ]]; then
    die "Confirmation not received (you typed: '${CONFIRMATION}'). Aborting. No changes made."
fi

# ---------------------------------------------------------------------------
# Unmount, write, sync, eject
# ---------------------------------------------------------------------------
echo
info "Unmounting $TARGET_DEV ..."
diskutil unmountDisk "$TARGET_DEV" || die "Failed to unmount $TARGET_DEV."

info "Writing image to $TARGET_RDEV (sudo required for raw disk access)..."
info "Press Ctrl-T during the write to see progress."
# bs=1m is a good balance on macOS USB writes. Write to the raw device.
if ! sudo dd if="$IMAGE_ABS" of="$TARGET_RDEV" bs=1m; then
    err "dd failed. The drive may be in an inconsistent state."
    sudo diskutil unmountDisk "$TARGET_DEV" >/dev/null 2>&1 || true
    die "Write failed."
fi

info "Flushing buffers (sync)..."
sync

# ---------------------------------------------------------------------------
# Verify what was actually written to the device
# ---------------------------------------------------------------------------
echo
info "Verifying the written data by reading back $IMAGE_SIZE_BYTES bytes..."
# Read back exactly the image's worth of MB blocks, then trim to the exact byte
# count before hashing. The '|| true' guards against SIGPIPE/pipefail aborting
# the script when head closes the pipe early.
READ_MB="$(( (IMAGE_SIZE_BYTES + 1048575) / 1048576 ))"
WRITTEN_SHA256="$( { sudo dd if="$TARGET_RDEV" bs=1m count="$READ_MB" 2>/dev/null \
    | head -c "$IMAGE_SIZE_BYTES" \
    | shasum -a 256 | awk '{print $1}'; } || true )"

ok "Source  SHA256 : $SRC_SHA256"
ok "Written SHA256 : $WRITTEN_SHA256"

if [[ "$SRC_SHA256" == "$WRITTEN_SHA256" ]]; then
    ok "VERIFIED: written image matches the source image."
    VERIFY_RC=0
else
    err "MISMATCH: written image does NOT match the source image!"
    err "The drive may be faulty, undersized, or the write was truncated."
    VERIFY_RC=1
fi

# ---------------------------------------------------------------------------
# Eject cleanly
# ---------------------------------------------------------------------------
echo
info "Ejecting $TARGET_DEV ..."
if diskutil eject "$TARGET_DEV"; then
    ok "Drive ejected. It is now safe to remove."
else
    warn "Eject reported an error. Run 'diskutil eject $TARGET_DEV' manually before removing."
fi

echo
if [[ "$VERIFY_RC" -eq 0 ]]; then
    ok "DONE: golem.img flashed and verified on $TARGET_DEV."
else
    err "DONE WITH ERRORS: verification failed. Do NOT trust this drive — re-flash."
fi

exit "$VERIFY_RC"
