#!/usr/bin/env bash
#
# build_safemode_image.sh — Golem Linux Phase 5, Agent 4 (Safe Mode Image Builder)
#
# Builds a bootable UEFI disk image (golem-safemode.img) holding the compiled
# SAFE MODE kernel at the firmware fallback boot path  \EFI\BOOT\BOOTX64.EFI.
#
# This is the Safe Mode counterpart to build_image.sh. It is modelled on that
# script line-for-line; the ONLY substantive difference is that it installs the
# gkern-safemode binary (agent processes disabled) instead of gkern, and writes
# to golem-safemode.img instead of golem.img. The two images are kept strictly
# distinct so the Safe Mode instance can never be confused with the main OS.
#
#   1. Create a 64 MiB raw disk image.
#   2. Lay down a GPT with a single FAT32 EFI System Partition.
#   3. Create EFI/BOOT/ and copy the Safe Mode kernel to BOOTX64.EFI.
#   4. Unmount and detach cleanly.
#
# macOS only — uses hdiutil/diskutil/newfs_msdos for every disk-image operation
# and requires NO root privileges.
#
# Idempotent in two senses:
#   * Operationally — any attachment left over from a previous (even interrupted)
#     run is detached first and the image is rebuilt from scratch, so re-running
#     always lands in the same defined end state and never appends or corrupts.
#   * Bit-for-bit — a final normalization pass rewrites the few OS-injected,
#     boot-irrelevant fields that macOS randomizes (GPT disk/partition GUIDs and
#     their CRCs, the FAT32 volume serial, FAT directory-entry timestamps) to
#     fixed values and zeroes freed-cluster slack, so two runs produce a
#     byte-identical golem-safemode.img with the same SHA256. (Needs python3; if
#     it is unavailable the image is still valid and bootable, just not
#     bit-reproducible.)
#
set -euo pipefail

# --- configuration ----------------------------------------------------------
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KERNEL="$REPO_ROOT/target/x86_64-unknown-none/debug/gkern-safemode"
IMG="$REPO_ROOT/golem-safemode.img"
VOL_NAME="GOLEMSAFE"
IMG_SIZE_MB=64
# Fixed timestamp for the copied kernel (improves FAT directory-entry stability).
# Format: [[CC]YY]MMDDhhmm[.SS]
FIXED_MTIME="202601010000.00"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
err() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; }

# --- detach any device currently backed by $IMG -----------------------------
# Parse `hdiutil info`, find whole-disk nodes (/dev/diskN) whose backing
# image-path is our image, and force-detach them. Cleans up after an
# interrupted previous run so the rebuild starts from a known state.
detach_image() {
  local img="$1" dev
  for dev in $(hdiutil info 2>/dev/null | awk -v t="$img" '
        /^image-path[[:space:]]*:/ { blk = (index($0, t) > 0) }
        blk && $1 ~ /^\/dev\/disk[0-9]+$/ { print $1 }'); do
    log "detaching stale attachment $dev"
    diskutil unmountDisk force "$dev" >/dev/null 2>&1 || true
    hdiutil detach "$dev" -force      >/dev/null 2>&1 || true
  done
}

DEV=""
cleanup() {
  # Always release the device we attached, even on error.
  if [[ -n "$DEV" ]]; then
    diskutil unmountDisk force "$DEV" >/dev/null 2>&1 || true
    hdiutil detach "$DEV" -force      >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

# --- preflight --------------------------------------------------------------
if [[ ! -f "$KERNEL" ]]; then
  err "safe mode kernel binary not found: $KERNEL"
  err "build the safe mode kernel first (it produces gkern-safemode)"
  exit 1
fi

# --- start fresh (idempotency) ---------------------------------------------
detach_image "$IMG"
rm -f "$IMG"

# --- 1. create the 64 MiB raw image ----------------------------------------
log "creating ${IMG_SIZE_MB} MiB raw image: $IMG"
dd if=/dev/zero of="$IMG" bs=1m count="$IMG_SIZE_MB" status=none

# --- 2. attach without mounting, then partition -----------------------------
log "attaching image"
DEV="$(hdiutil attach -nomount -imagekey diskimage-class=CRawDiskImage "$IMG" \
        | head -1 | awk '{print $1}')"
[[ -n "$DEV" ]] || { err "failed to attach image"; exit 1; }
log "attached as $DEV"

log "writing GPT with a single FAT32 EFI partition"
# diskutil creates the GPT, formats partition 1 as FAT32, and auto-mounts it.
diskutil partitionDisk "$DEV" GPT FAT32 "$VOL_NAME" 100% >/dev/null

PART="${DEV}s1"
# Resolve the real mount point (handles names like "/Volumes/GOLEMSAFE 1").
MOUNT="$(diskutil info "$PART" | awk -F': *' '/Mount Point/{print $2; exit}')"
[[ -n "$MOUNT" && -d "$MOUNT" ]] || { err "partition not mounted"; exit 1; }
log "partition $PART mounted at $MOUNT"

# --- 3. create EFI/BOOT and install the kernel ------------------------------
log "creating EFI/BOOT/ and installing safe mode kernel as BOOTX64.EFI"
mkdir -p "$MOUNT/EFI/BOOT"
cp "$KERNEL" "$MOUNT/EFI/BOOT/BOOTX64.EFI"
touch -t "$FIXED_MTIME" "$MOUNT/EFI/BOOT/BOOTX64.EFI"

# Strip macOS-injected metadata so the EFI partition holds only the boot files.
rm -rf "$MOUNT"/.fseventsd "$MOUNT"/.Trashes "$MOUNT"/.Spotlight-V100 \
       "$MOUNT"/.TemporaryItems "$MOUNT"/.DS_Store 2>/dev/null || true
find "$MOUNT" -name '._*' -delete 2>/dev/null || true
sync

# --- 4. unmount cleanly -----------------------------------------------------
log "unmounting and detaching"
diskutil unmountDisk "$DEV" >/dev/null
hdiutil detach "$DEV" >/dev/null
DEV=""   # released; nothing left for the trap to do

# --- 5. normalize for bit-for-bit reproducibility ---------------------------
# Rewrites only OS-randomized, boot-irrelevant fields to fixed values so the
# image is byte-identical across runs, and promotes the partition to a true
# EFI System Partition type. Pure byte editing of the finished file.
if command -v python3 >/dev/null 2>&1; then
  log "normalizing image for reproducibility"
  python3 - "$IMG" <<'PY'
import sys, struct, zlib

FIXED_DISK_GUID = bytes.fromhex("0123456789abcdef0123456789abcdef")
FIXED_PART_GUID = bytes.fromhex("fedcba9876543210fedcba9876543210")
# EFI System Partition type GUID C12A7328-F81F-11D2-BA4B-00A0C93EC93B (on-disk order)
ESP_TYPE_GUID   = bytes.fromhex("28732ac11ff8d211ba4b00a0c93ec93b")
FIXED_SERIAL    = b"\x60\x1e\x60\x1e"               # FAT volume id
FDATE = struct.pack("<H", ((2026-1980)<<9)|(1<<5)|1)  # 2026-01-01
FTIME = struct.pack("<H", 0)                          # 00:00:00

def u16(b,o): return struct.unpack_from("<H",b,o)[0]
def u32(b,o): return struct.unpack_from("<I",b,o)[0]
def u64(b,o): return struct.unpack_from("<Q",b,o)[0]

def norm_gpt(img):
    SEC=512
    for hoff in (1*SEC, u64(img,1*SEC+32)*SEC):     # primary, then backup (AlternateLBA)
        assert img[hoff:hoff+8]==b"EFI PART", "bad GPT signature @ 0x%x"%hoff
        hsz=u32(img,hoff+12)
        pe_lba=u64(img,hoff+72); npe=u32(img,hoff+80); spe=u32(img,hoff+84)
        arr=pe_lba*SEC
        img[arr:arr+16]    = ESP_TYPE_GUID          # entry 0 partition type
        img[arr+16:arr+32] = FIXED_PART_GUID        # entry 0 unique GUID
        struct.pack_into("<I",img,hoff+88,
                         zlib.crc32(bytes(img[arr:arr+npe*spe])) & 0xffffffff)
        img[hoff+56:hoff+72] = FIXED_DISK_GUID      # disk GUID
        struct.pack_into("<I",img,hoff+16,0)        # zero header CRC field...
        struct.pack_into("<I",img,hoff+16,
                         zlib.crc32(bytes(img[hoff:hoff+hsz])) & 0xffffffff)  # ...recompute
    return u64(img,2*SEC+32)                         # entry 0 FirstLBA = partition start

def norm_fat(img, part_lba):
    SEC=512; p=part_lba*SEC
    bps=u16(img,p+0x0B); spc=img[p+0x0D]; rsvd=u16(img,p+0x0E)
    nfat=img[p+0x10]; fatsz=u32(img,p+0x24); rootclus=u32(img,p+0x2C)
    bkboot=u16(img,p+0x32)
    totsec=u16(img,p+0x13) or u32(img,p+0x20)
    img[p+0x43:p+0x47]=FIXED_SERIAL                  # volume serial (boot sector)
    if bkboot: img[p+bkboot*SEC+0x43:p+bkboot*SEC+0x47]=FIXED_SERIAL  # backup boot sector
    data=rsvd+nfat*fatsz; ndataclus=(totsec-data)//spc
    fat0=p+rsvd*bps
    def clus_off(c): return p+(data+(c-2)*spc)*bps
    def chain(c):
        out=[]
        while 2<=c<0x0FFFFFF8: out.append(c); c=u32(img,fat0+c*4)&0x0FFFFFFF
        return out
    def norm_dir(start):
        for c in chain(start):
            base=clus_off(c)
            for e in range(0,bps*spc,32):
                o=base+e; first=img[o]
                if first==0x00: return               # end of directory
                if first==0xE5:                      # deleted: wipe stale slack, keep marker
                    img[o+1:o+32]=b"\x00"*31; continue
                attr=img[o+0x0B]
                if attr==0x0F: continue              # long-name entry: no timestamps
                img[o+0x0D]=0                         # create time (tenths)
                img[o+0x0E:o+0x10]=FTIME             # create time
                img[o+0x10:o+0x12]=FDATE            # create date
                img[o+0x12:o+0x14]=FDATE            # last access date
                img[o+0x16:o+0x18]=FTIME            # write time
                img[o+0x18:o+0x1A]=FDATE            # write date
                if (attr&0x10) and img[o]!=0x2E:     # subdirectory (not '.'/'..')
                    sub=(u16(img,o+0x14)<<16)|u16(img,o+0x1A)
                    if sub>=2: norm_dir(sub)
    norm_dir(rootclus)
    zero=b"\x00"*(bps*spc)                            # zero freed-cluster slack
    for c in range(2, ndataclus+2):
        if (u32(img,fat0+c*4)&0x0FFFFFFF)==0:
            off=clus_off(c); img[off:off+bps*spc]=zero

img=bytearray(open(sys.argv[1],"rb").read())
norm_fat(img, norm_gpt(img))
open(sys.argv[1],"wb").write(img)
PY
else
  err "python3 not found — skipping reproducibility normalization"
  err "image is valid and bootable but not bit-reproducible across runs"
fi

# --- report -----------------------------------------------------------------
SIZE_BYTES="$(stat -f %z "$IMG")"
SHA256="$(shasum -a 256 "$IMG" | awk '{print $1}')"
log "done"
printf '\n'
printf 'Image:   %s\n' "$IMG"
printf 'Size:    %s bytes (%s MiB)\n' "$SIZE_BYTES" "$((SIZE_BYTES / 1024 / 1024))"
printf 'SHA256:  %s\n' "$SHA256"
