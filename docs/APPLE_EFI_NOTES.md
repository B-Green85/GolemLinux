# Apple EFI Compatibility Notes — 2017 MacBook Pro (pre-T2 Intel)

**Subsystem:** Boot / firmware compatibility (Phase 6, Agent 2)
**Scope of this document:** reference material for booting the Golem UEFI
bootloader and `gkern` on Apple's EFI implementation. It covers the firmware,
NVRAM, disk-layout, and UEFI-protocol behaviour that differs from a
PC-class UEFI 2.x firmware, and records the one decision this agent owns about
`src/boot/boot.asm`.

*Copyright © 2026 Brandon Green. Licensed under the Apache 2.0 License.*

---

## 0. Target hardware

| Property              | Value                                                              |
| --------------------- | ------------------------------------------------------------------ |
| Marketing name        | MacBook Pro (2017)                                                 |
| Board IDs             | `MacBookPro14,1` (13″, no Touch Bar), `MacBookPro14,2` (13″ TB), `MacBookPro14,3` (15″ TB) |
| CPU                   | Intel Kaby Lake, x86_64                                            |
| Coprocessor           | Apple **T1** on Touch Bar models (13″ TB / 15″); none on `14,1`    |
| Firmware              | Apple EFI (Apple's own EDK-derived implementation), **64-bit**     |
| Spec baseline         | UEFI 2.x-class handoff; Apple boot manager is non-standard above it |
| Secure Boot           | **Not enforced for external/third-party media** (pre-T2)           |

**Why "pre-T2" matters.** The 2017 MacBook Pro predates the Apple **T2** chip
(introduced on the 2018 models). On T2 Macs, `bridgeOS` running on the T2 owns
the boot process and *UEFI Secure Boot is enforced* — an unsigned third-party
OS must have Secure Boot manually lowered to "No Security" in the Startup
Security Utility before it can boot from external media. **The 2017 model has no
such gate.** The T1 chip on Touch Bar models drives the Touch Bar and the Secure
Enclave (Touch ID / Apple Pay) only; it does **not** mediate the EFI boot path.
Consequently, on this hardware Golem can boot an unsigned `gkern` from USB with
no firmware-security reconfiguration. This is the single most important reason
the 2017 MBP was chosen as the Phase 6 reference target.

---

## 1. Apple EFI quirks on pre-T2 Intel MacBooks

Apple's firmware is *not* a stock PC UEFI. It is UEFI-2.x-class at the protocol
ABI level (so a well-behaved UEFI application loads and runs), but the layer that
*selects* what to boot, and several runtime protocols, deviate from the spec.

### 1.1 Non-standard boot manager

Apple firmware does **not** drive boot from the standard `BootOrder` /
`Boot####` global NVRAM variables the way a PC UEFI does. It loads its own
proprietary boot manager, which selects a target from:

1. The Apple-specific `efi-boot-device` NVRAM variable (set by `bless`), and
2. The "blessed" boot-file location recorded in the HFS+/APFS volume header.

Standard `Boot####` entries created with tools such as `efibootmgr` are
unreliable on Macs: the Apple boot manager may ignore them, and firmware
updates / macOS updates routinely rewrite NVRAM and drop them. **Do not rely on
`efibootmgr`-style persistence for Golem.** See §4 for the supported ways to
prioritise USB.

### 1.2 The `bless` mechanism

`bless` (the macOS `/usr/sbin/bless` tool, a relic of Classic Mac OS "System
Folder blessing") is Apple's supported way to mark a bootloader bootable. It can
target a file, a folder containing `boot.efi`, or a whole partition, and it
writes **two** things:

- the location into the `efi-boot-device` NVRAM variable, **and**
- the boot-file location into the **HFS+/APFS volume header**, so the choice
  survives the disk being moved to another Mac or the NVRAM being reset.

`bless` is only needed for a *persistent default* on an **internal** disk.
Booting Golem from USB does **not** require `bless` (see §4.1). Two caveats:

- **SIP blocks `bless`.** On macOS 10.11 (El Capitan) and later, `bless
  --setBoot` fails while System Integrity Protection is active. SIP must be
  disabled from Recovery (`csrutil disable`) to bless a third-party loader on
  the internal disk. This does **not** affect USB boot via Startup Manager.
- `bless` runs under macOS; it is a host-side provisioning step, never part of
  Golem's own boot path.

### 1.3 Dual filesystem support in firmware

Apple EFI understands **HFS+ natively** in addition to the spec-required
**FAT** (FAT12/16/32). This is unusual — stock PC UEFI reads only FAT. Two
practical consequences:

- A bootloader placed on a dedicated **HFS+** partition is visible to the
  Option-key Startup Manager (see §1.7); a bootloader on the **FAT32 EFI System
  Partition (ESP)** generally is **not** (the ESP is the firmware's blind spot
  on Macs).
- For USB media, the **FAT32 ESP + fallback path** route (§4.1) is simplest and
  needs no HFS+ tooling. Golem uses this route.

Note: Apple firmware does **not** ship an APFS driver in ROM on pre-T2 Macs; the
APFS driver is loaded from the container's Preboot volume. This is irrelevant to
Golem (we boot from FAT32), but it is why third-party loaders that chain to
macOS must load `apfs.efi` themselves.

### 1.4 64-bit vs 32-bit EFI

Early Intel Macs (2006–2007, e.g. `MacPro1,1`) shipped a **32-bit EFI** that
cannot load a 64-bit `BOOTX64.EFI`. **This does not apply to the 2017 MBP**,
which is fully 64-bit EFI. Golem's PE32+ bootloader (`x86_64-unknown-uefi`,
producing `BOOTX64.EFI`) is the correct and only artefact needed here. The
32-bit caveat is recorded only so the boot-media tooling never mistakenly
targets `BOOTIA32.EFI` for this machine.

### 1.5 Graphics Output Protocol (GOP) / console quirk

Apple firmware does not always publish a usable `EFI_GRAPHICS_OUTPUT_PROTOCOL`
on the console handle, and its text-console behaviour differs from PC UEFI.
Mature Mac bootloaders (OpenCore's `ProvideConsoleGop`, plus a `ConsoleControl`
shim) **re-install GOP on the console handle** before drawing. **This is a
runtime-protocol concern, handled after `efi_main` returns into Rust — it is the
peer Rust-bootloader subsystem's responsibility, not `boot.asm`'s** (see §3.2).
Recorded here so that whoever owns early framebuffer output expects a possibly
absent or non-standard GOP on this firmware and probes/installs accordingly.

### 1.6 Memory-map size sensitivity

Apple's own `boot.efi` / XNU is sensitive to a memory map larger than a single
4 KiB page (~≤128 descriptors), and OpenCore ships `ShrinkMemoryMap` to coalesce
contiguous same-type descriptors for that reason. Golem's kernel is **not** XNU
and does not inherit XNU's 4 KiB limit, so this specific crash does not apply to
`gkern`. The relevant Apple-firmware fact for Golem is the *companion* quirk:
on some Apple firmware revisions, `ExitBootServices()` can fail with
`EFI_INVALID_PARAMETER` if the memory-map key is stale because the map changed
between `GetMemoryMap()` and the exit call. **The Rust bootloader must call
`GetMemoryMap()` immediately before `ExitBootServices()` and retry the exit with
a fresh key on failure** (OpenCore's `ForceExitBootServices` does exactly this).
Cross-subsystem advisory — see §3.2.

### 1.7 Startup Manager (Option key) is blind to the ESP

Holding **Option (⌥)** at power-on opens Apple's Startup Manager (the graphical
boot picker). It lists:

- macOS / Recovery volumes,
- bootable **removable** media that have `\EFI\BOOT\BOOTX64.EFI`, shown as a
  generic **"EFI Boot"** entry, and
- bootloaders installed on a dedicated **HFS+** partition.

It does **not** list a bootloader installed only to the **FAT32 ESP** of an
internal disk. For USB media this blindness does not bite — removable media with
the fallback path *are* enumerated. This asymmetry (ESP-blind for internal,
fallback-path-aware for removable) is the key behavioural fact behind the §4
boot-order guidance.

### 1.8 No legacy BIOS/CSM path for Golem

Apple firmware can enter a BIOS-compatibility (CSM) mode, but **only** when a
hybrid MBR is present or a BIOS-bootable optical disc is inserted (this is the
Boot Camp mechanism). Golem is **UEFI-only** (`README.md` §"No multiboot2
fallback"). The USB must therefore be **pure GPT with no hybrid MBR** — a hybrid
MBR risks nudging the firmware toward the CSM path, which Golem does not support.

### 1.9 NVRAM hygiene

Some Windows/third-party loaders write UEFI certificates into Apple NVRAM, which
has bricked boot on certain Macs (RefindPlus actively filters this when it
detects Apple firmware). Golem writes **nothing** to NVRAM from its boot path,
so it cannot trigger this class of fault. Recorded as a hazard to preserve: no
future Golem boot-path change should write to Apple NVRAM.

---

## 2. Required EFI variables / boot flags for third-party OS boot

Short version: **for USB boot on the 2017 MBP, Golem needs no NVRAM variables
and no boot flags at all** — only the correct on-disk file layout. Persistent
internal-disk default is the only case that touches NVRAM (via `bless`).

### 2.1 What is actually required (USB)

| Requirement                          | Value                                                        |
| ------------------------------------ | ------------------------------------------------------------ |
| Partition scheme                     | **GPT**, no hybrid MBR                                        |
| EFI System Partition type GUID       | `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`                       |
| ESP filesystem                       | **FAT32** (FAT16/12 also accepted)                           |
| Bootloader path (the magic one)      | `\EFI\BOOT\BOOTX64.EFI`                                       |
| Architecture                         | x86_64 PE32+ (`x86_64-unknown-uefi`)                         |
| Secure Boot                          | n/a on this model — not enforced for external media          |

The fallback/removable boot path `\EFI\BOOT\BOOTX64.EFI` is the entire
mechanism: UEFI (Apple included) boots it from removable media with **no
`Boot####` entry required**. Golem's bootloader binary is simply named
`BOOTX64.EFI` and placed there.

### 2.2 NVRAM variables (reference)

Only relevant for a *persistent internal-disk default*; never required for USB.

| Variable / store          | Owner   | Role for Golem                                              |
| ------------------------- | ------- | ---------------------------------------------------------- |
| `efi-boot-device`         | Apple   | Set by `bless`; the Apple boot manager's persistent target. The supported way to make an internal Golem the default. |
| HFS+/APFS volume header   | Apple   | Secondary blessed-location record; survives NVRAM reset.    |
| `BootOrder` / `Boot####`  | UEFI    | **Unreliable on Macs** — Apple boot manager may ignore; updates wipe. Do not depend on. |

There are **no kernel boot flags** required at the firmware level. Any Golem
boot parameters belong in Golem's own bootloader config (peer subsystem), not in
Apple NVRAM.

---

## 3. Does `boot.asm` need Apple-specific adjustments?

**No. `src/boot/boot.asm` is left unchanged.** This section is the rigorous
justification, in the same spirit as the Phase 2 review recorded in
`src/boot/README.md`.

### 3.1 Why no change is required — and why a change would be wrong

`boot.asm` is the **UEFI application entry stub** (`efi_main`). Its entire job is
to receive the firmware handoff and forward it to the Rust `bootloader_main`
with registers intact, touching nothing else. The Apple-specific behaviour
catalogued in §1 lives in three places, **none of which is the entry stub**:

1. **Firmware boot-selection** (bless, `efi-boot-device`, Startup Manager) —
   happens *before* `BOOTX64.EFI` is even loaded. Out of any binary's control.
2. **On-disk layout** (GPT, FAT32 ESP, fallback path) — a media-provisioning
   concern (§4.1), not code.
3. **Runtime UEFI protocols** (GOP, memory map, `ExitBootServices`) — exercised
   by the Rust half *after* `efi_main` returns (§3.2).

At the *handoff boundary* `boot.asm` owns, the 2017 MBP firmware is
UEFI-2.x-compliant Microsoft x64:

- `RCX` = `ImageHandle`, `RDX` = `SystemTable` are delivered per spec — exactly
  the contract in `src/boot/README.md` §"Calling convention". `efi_main`
  forwards them untouched; nothing Apple-specific changes that.
- Stack is 16-byte aligned (`RSP + 8 ≡ 0 (mod 16)` on entry), shadow space is
  caller-provided, `RFLAGS.DF` is clear, and the CPU is in 64-bit long mode —
  all per UEFI §2.3.4, all honoured by Apple's firmware on this model.

Adding Apple-specific instructions to `efi_main` would be actively harmful:

- It would contradict the **minimum-touch invariant** documented in
  `src/boot/README.md` §"What `boot.asm` deliberately does not do" — every byte
  of state mutated here must be undone before any UEFI Boot Service call in Rust
  succeeds.
- It would change the bootloader image's bytes and therefore the **per-boot
  SHA256 measurement** the Sentinel records (`src/boot/README.md` §"Sentinel
  note"). A gratuitous edit would perturb a load-bearing audit value for zero
  compatibility benefit.

This mirrors the Phase 2 conclusion: the requirement is real, but it does not
belong to this file.

### 3.2 Where the Apple work actually belongs (cross-subsystem advisory)

The genuine Apple-firmware accommodations are runtime-protocol work for the
**peer Rust bootloader (`bootloader_main`, kernel-loader subsystem)**, not for
`boot.asm`. Flagged here so they are not lost:

| Apple quirk (§ ref)              | Required accommodation                                         | Owner                        |
| -------------------------------- | ------------------------------------------------------------- | ---------------------------- |
| Possibly-absent GOP (§1.5)       | Probe console handle; install GOP if missing before drawing.   | Rust bootloader / framebuffer |
| Stale memory-map key (§1.6)      | `GetMemoryMap()` immediately before `ExitBootServices()`; retry exit with fresh key on `EFI_INVALID_PARAMETER`. | Rust bootloader |
| NVRAM hygiene (§1.9)             | Never write certificates/vars to Apple NVRAM from the boot path. | Rust bootloader |

None of these alters the `efi_main` ↔ `bootloader_main` register contract, so
`boot.asm` stays byte-for-byte as is.

---

## 4. Setting the boot order to prioritise USB

There are three approaches, in increasing order of persistence. For Phase 6
bring-up, **§4.2 (Option-key Startup Manager) is the recommended path** — it is
transient, needs no macOS changes, no SIP changes, and no NVRAM writes.

### 4.1 Prepare the USB so the firmware will offer it

Required regardless of which selection method follows:

1. Partition the USB **GPT**, no hybrid MBR (§1.8).
2. Create a **FAT32** EFI System Partition (type GUID
   `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`).
3. Place Golem's bootloader at **`\EFI\BOOT\BOOTX64.EFI`**.

With this, the Startup Manager enumerates the stick as a generic **"EFI Boot"**
entry (§1.7) — no `Boot####` variable, no `bless`, no NVRAM write needed.

### 4.2 Transient, per-boot (recommended): Startup Manager

1. Insert the USB.
2. Power on / restart and **hold Option (⌥)** until the picker appears.
3. Select the **"EFI Boot"** entry → Golem boots.

This selects USB for **this boot only**; the Mac reverts to its normal default
next time. Ideal for development/bring-up. (If a USB keyboard is used instead of
the built-in one, hold Option on *that* keyboard; Apple firmware may need a
moment to initialise external USB HID before it registers the key.)

### 4.3 Session default (no NVRAM/SIP changes): Startup Disk

From macOS: **System Settings → General → Startup Disk** (or
`bless --mount … --setBoot` equivalents) can select a bootable volume as the
default. Apple periodically changes what Startup Disk considers selectable for
third-party loaders, so treat this as best-effort, not guaranteed for a raw
USB.

### 4.4 Persistent internal default: `bless` / `efi-boot-device`

To make an **internally-installed** Golem the standing default, `bless` it
(writes `efi-boot-device` + the volume-header location, §1.2). Requires SIP
disabled (§1.2). **Not needed for USB boot** and out of scope for Phase 6
bring-up.

### 4.5 Why not `efibootmgr` / `BootOrder`?

Because Apple's boot manager does not reliably honour standard `BootOrder` /
`Boot####` entries, and firmware/macOS updates wipe them (§1.1). Setting USB
priority via `efibootmgr` from a running Linux is the standard PC technique and
is **explicitly not recommended on this hardware** — use §4.2 instead.

---

## 5. Known issues and workarounds

| # | Symptom                                                            | Cause                                                                 | Workaround                                                                                  |
| - | ----------------------------------------------------------------- | --------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| 1 | USB never appears in Startup Manager                              | Missing fallback path, MBR/hybrid layout, or non-FAT ESP (§1.7/§4.1)  | GPT + FAT32 ESP + `\EFI\BOOT\BOOTX64.EFI`; remove any hybrid MBR.                            |
| 2 | Golem on internal ESP not offered by Startup Manager             | Apple Startup Manager is blind to the internal FAT32 ESP (§1.7)       | Use removable media (fallback path is enumerated), or install to a dedicated HFS+ partition + `bless`. |
| 3 | `Boot####` entry vanishes after a macOS/firmware update          | Apple rewrites NVRAM; standard UEFI entries not preserved (§1.1)      | Don't depend on `BootOrder`; re-select with Option (§4.2) or re-`bless` (§4.4).             |
| 4 | `bless` fails with a permissions/SIP error                        | SIP active on 10.11+ (§1.2)                                            | `csrutil disable` from Recovery, `bless`, optionally re-enable. (Not needed for USB.)       |
| 5 | Black screen / no early console output after handoff             | Apple firmware did not publish usable GOP on the console handle (§1.5)| Rust bootloader probes and installs GOP before drawing (§3.2) — not a `boot.asm` issue.     |
| 6 | `ExitBootServices()` returns `EFI_INVALID_PARAMETER`             | Memory-map key went stale between `GetMemoryMap` and exit (§1.6)      | Re-`GetMemoryMap()` immediately before exit; retry with the fresh key (§3.2).               |
| 7 | Firmware drops into BIOS/CSM behaviour unexpectedly              | Hybrid MBR present on the USB (§1.8)                                  | Use pure GPT, no hybrid MBR. Golem is UEFI-only.                                            |
| 8 | Mac won't boot at all after a third-party loader write           | Cert/var written into Apple NVRAM bricked boot (§1.9)                | Reset NVRAM (hold ⌥⌘PR at power-on). Golem writes no NVRAM, so it cannot cause this.        |
| 9 | External USB keyboard's Option key ignored at power-on           | Firmware initialises external USB HID late (§4.2)                     | Hold Option on the built-in keyboard, or wait for HID init before pressing.                 |

---

## 6. Summary

- The 2017 MacBook Pro is **pre-T2**: no UEFI Secure Boot enforcement on
  external media, so an unsigned Golem boots from USB with no firmware-security
  changes. This is why it is the Phase 6 reference target.
- **Golem boots on this Mac with the standard removable-media recipe** — GPT +
  FAT32 ESP + `\EFI\BOOT\BOOTX64.EFI`, selected per-boot by holding **Option**.
  No NVRAM variables, no `bless`, no SIP changes for the USB path.
- **`src/boot/boot.asm` is correct as-is and is not modified.** The Apple
  quirks live in firmware boot-selection, on-disk layout, and runtime UEFI
  protocols — none at the `efi_main` handoff boundary, where Apple's firmware is
  UEFI-2.x-compliant Microsoft x64. The real accommodations (GOP injection,
  memory-map/`ExitBootServices` retry) belong to the peer Rust bootloader and
  are recorded in §3.2.

---

## 7. Sources

- [Booting Macs — docosx](https://matthew-brett.github.io/docosx/booting_macs.html) — bless, `efi-boot-device`, HFS+/FAT firmware support, non-standard boot manager, CSM/`--legacy`.
- [The rEFInd Boot Manager: Keeping rEFInd Booting](https://www.rodsbooks.com/refind/bootcoup.html) — `bless` commands, SIP limitation, Startup Manager ESP-blindness, update-driven entry loss.
- [The rEFInd Boot Manager](https://www.rodsbooks.com/refind/) — "EFI Boot" enumeration for volumes with `\EFI\boot\bootx64.efi`.
- [RefindPlus](https://github.com/RefindPlusRepo/RefindPlus) — Apple-firmware NVRAM certificate-write hazard and detection.
- [Boot process for an Intel-based Mac — Apple Support](https://support.apple.com/guide/security/boot-process-sec5d0fab7c6/web) — T2 vs pre-T2 boot security model.
- [OpenCore — Differences / ProvideConsoleGop, ShrinkMemoryMap, ForceExitBootServices](https://dortania.github.io/docs/release/Differences.html) — GOP injection, memory-map sizing, ExitBootServices retry quirks.
- [EFI system partition — Wikipedia](https://en.wikipedia.org/wiki/EFI_system_partition) — ESP type GUID `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`, fallback `\EFI\BOOT\BOOTX64.EFI`, FAT requirement.
- [USB Booting Linux on a Mac with 32-bit EFI — ldx.ca](https://www.ldx.ca/notes/intel-mac-efi32-linux.html) — 32-bit-EFI caveat (early Macs; not the 2017 MBP).
