# Bare Metal Boot Verification

This document walks you through booting Golem Linux on real hardware (a physical
Intel MacBook) from a USB stick, confirming it actually boots, and recovering
safely if it doesn't. It is written for someone doing this **for the first
time** — every step is spelled out, and the things that commonly go wrong have
their own recovery sections.

> **Why bare metal?** Everything up to this point has been verified in QEMU. QEMU
> is forgiving: it gives us a clean UART, a predictable framebuffer, and an exit
> code. Real hardware does not. Booting on a physical MacBook is the only proof
> that the kernel survives a real UEFI firmware handoff, a real framebuffer, and
> real timing. Treat this as a *verification* milestone, not a daily workflow.

---

## 0. Before you start — read this whole section

### What you need

| Item | Notes |
|------|-------|
| **Target machine** | An **Intel (x86-64) MacBook**. Apple-Silicon (M1/M2/…) Macs are ARM64 and **cannot** boot an `x86_64-unknown-none` image — do not attempt. |
| **Build machine** | Any Linux/macOS box with the Rust toolchain installed. Can be the same MacBook, but it's far safer to build on a *second* machine so the target stays untouched. |
| **USB stick** | ≥ 1 GB, **and one you are willing to erase completely**. Flashing destroys all data on it. |
| **Power** | Target MacBook on AC power, ≥ 50% battery. A mid-boot power loss during a write test can corrupt state. |
| **A phone or camera** | For recording the boot (see [§8](#8-recording-setup)). The screen output is ephemeral — if it panics and reboots, the only record may be your video. |
| **A second person (recommended)** | One to operate the keyboard, one to run the camera. Doing both alone is error-prone. |

### Safety ground rules

- **Back up the MacBook first** if it has anything important on it. We are not
  touching the internal disk, but you are about to repeatedly hard-power-cycle a
  laptop, and mistakes happen.
- **Never run `flash_usb.sh` against the internal disk.** The single most
  destructive mistake possible here is targeting the wrong device. [§3](#3-flash-the-image-to-usb)
  covers how to identify the USB device correctly.
- **Know the force-shutdown gesture before you boot** ([§7](#7-emergency-force-shutdown)).
  You will likely need it.

### Toolchain prerequisites (build machine)

```bash
# Rust nightly is required for build-std / the bare-metal target.
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
rustup target add x86_64-unknown-none
```

> The `x86_64-unknown-none` target is a *freestanding* target: no OS, no libc, no
> std. If `cargo build` complains it can't find `core`, you are missing
> `rust-src` (the component above).

---

## 1. Build the release kernel

From the repository root:

```bash
cargo build --release --target x86_64-unknown-none
```

**What this produces:** an optimized, freestanding kernel ELF at
`target/x86_64-unknown-none/release/golem` (the exact name follows the crate's
`[[bin]]`/package name).

**Why `--release`:** debug builds are large and slow, and some timing-sensitive
boot paths behave differently unoptimized. Bare-metal verification must use the
release artifact so we're testing what we'd actually ship.

**Verify the build before going further:**

```bash
ls -lh target/x86_64-unknown-none/release/golem
file target/x86_64-unknown-none/release/golem
# Expect: "ELF 64-bit LSB executable, x86-64 ... statically linked"
```

### Failure modes — build

| Symptom | Cause | Fix |
|---------|-------|-----|
| `error[E0463]: can't find crate for 'core'` | `rust-src` not installed, or stable toolchain in use | `rustup component add rust-src --toolchain nightly` and ensure nightly is selected |
| `error: target may not be installed` | Target not added | `rustup target add x86_64-unknown-none` |
| Links fine in debug, fails in release | A `#[cfg(debug_assertions)]` path or panic handler only compiled in debug | Search for `debug_assertions`; the panic handler / `eh_personality` must exist in release too |
| Build succeeds but binary is tiny (< few KB) | Wrong `[[bin]]` selected, or it built a stub | Confirm you built the kernel package, not a helper crate |

**Do not proceed to imaging until the ELF builds clean.** A bad build flashed to
USB just wastes a full boot cycle.

---

## 2. Build the boot image

```bash
./scripts/build_image.sh
```

**What this does:** wraps the kernel ELF from §1 into a bootable disk image —
typically a UEFI-bootable layout with a GPT/ESP partition containing
`EFI/BOOT/BOOTX64.EFI` (the bootloader stub) plus the kernel. The output is a
single image file (e.g. `build/golem.img`).

**Verify the image exists and is non-trivial:**

```bash
ls -lh build/golem.img        # should be MBs, not bytes
```

### Failure modes — image

| Symptom | Cause | Fix |
|---------|-------|-----|
| `build_image.sh: kernel not found` | You skipped §1 or used a different profile | Re-run the §1 build; confirm the path the script expects |
| Image is only a few KB | Kernel wasn't embedded; script copied a stub | Re-run §1, then re-run the script; check script output for the `cp`/`dd` of the kernel |
| `mtools`/`mformat`/`xorriso` not found | Imaging tooling missing on build machine | Install the tool the script names (commonly `mtools`, `dosfstools`, or `xorriso`) |
| Permission denied | Script not executable | `chmod +x scripts/build_image.sh` |

---

## 3. Flash the image to USB

> ⚠️ **This is the dangerous step.** Flashing writes raw bytes to a block device
> and **destroys everything on the target device**. Identifying the wrong device
> can wipe your internal disk or another drive. Slow down here.

### 3a. Identify the USB device

Insert the USB stick, then:

- **Linux:** `lsblk` — find the device that *appeared* when you inserted the
  stick and whose size matches the stick (e.g. `/dev/sdb`, a whole disk, **not**
  a partition like `/dev/sdb1`).
- **macOS:** `diskutil list` — find the `/dev/diskN` whose size matches and is
  marked `(external, physical)`. Then unmount it (not eject):
  `diskutil unmountDisk /dev/diskN`.

Sanity checks before you commit:
- The size matches your stick, not your system disk.
- It is marked **external/removable**.
- Removing the stick and re-running the listing makes that device disappear.

### 3b. Flash

```bash
./scripts/flash_usb.sh /dev/sdX        # Linux: whole-disk node
# or on macOS:
./scripts/flash_usb.sh /dev/rdiskN     # the "raw" node is much faster
```

> If `flash_usb.sh` takes **no argument** and auto-detects, **read its output and
> confirm the device it chose before answering any prompt.** If it offers no
> confirmation prompt, stop and pass the device explicitly instead.

The script typically runs `dd` (or equivalent) and then `sync`. Wait for it to
fully return and for `sync` to complete before pulling the stick — a premature
unplug leaves a half-written, unbootable image.

### 3c. Verify the flash

```bash
sync
# Linux: confirm a FAT/ESP filesystem and the boot file landed:
sudo fdisk -l /dev/sdX
# Optionally mount the ESP read-only and confirm EFI/BOOT/BOOTX64.EFI exists.
```

Eject cleanly:
- **Linux:** `udisksctl power-off -b /dev/sdX` (or `sync` then unplug)
- **macOS:** `diskutil eject /dev/diskN`

### Failure modes — flash

| Symptom | Cause | Fix |
|---------|-------|-----|
| `Permission denied` / `Operation not permitted` | Need elevated rights to write block device | Re-run with `sudo`; on macOS unmount the disk first (`diskutil unmountDisk`) |
| `Resource busy` (macOS) | Device still mounted | `diskutil unmountDisk /dev/diskN` (unmount, don't eject) before flashing |
| Flash "succeeds" but USB won't boot | Wrote to a partition (`/dev/sdb1`) instead of the whole disk (`/dev/sdb`) | Re-flash to the whole-disk node |
| Wrote to the wrong device | Misidentified target | **Stop. Do not reboot the affected machine.** Assess the damage on the wrongly-written device; this is exactly why §3a exists |

---

## 4. Boot the MacBook from USB

1. **Fully shut down** the target MacBook (Apple menu → Shut Down, or hold power
   until it's off). Do not just sleep it.
2. Insert the flashed USB stick.
3. Press the **power button**, then **immediately press and hold the `Option`
   (`⌥`) key**. Keep holding until the **Startup Manager** (boot picker) appears
   — a row of drive icons.
4. The USB volume usually appears as an orange/gold **"EFI Boot"** icon. Use the
   arrow keys to select it and press **Return**.

> **Apple-Silicon note (again):** On an M-series Mac, holding the power button
> (not Option) brings up boot options, and an `x86_64` image will **not** appear
> or boot. This procedure is **Intel-only**.

### Firmware obstacles you may hit

- **Secure Boot / "Startup Security Utility."** Newer Intel MacBooks with a T2
  chip ship with Secure Boot set to *Full Security*, which blocks unsigned
  external boot. If the USB icon doesn't appear or is rejected:
  1. Boot into **macOS Recovery** (hold `Cmd-R` at startup).
  2. **Utilities → Startup Security Utility.**
  3. Set Secure Boot to **No Security** and "Allow booting from external media."
  4. Reboot and retry the `Option` boot.
- **Firmware password.** If one is set, you'll be prompted; you need it to reach
  the picker.

### Failure modes — boot picker

| Symptom | Cause | Fix |
|---------|-------|-----|
| Boot picker never appears, boots straight to macOS | `Option` pressed too late or released too early | Power off fully, press power, then hold `Option` *before* the chime/Apple logic and keep holding |
| Picker appears but no USB icon | Secure Boot blocking, bad image, or stick not bootable | Run Startup Security Utility (above); re-verify §3c; try a different USB port (prefer a direct port over a hub/dongle) |
| USB icon present but selecting it returns to picker / black screen, no output | Bootloader rejected by firmware, or image not actually UEFI-bootable | Re-check that `EFI/BOOT/BOOTX64.EFI` exists on the ESP (§3c); rebuild the image (§2) |

---

## 5. What you should see — and where serial goes

This is the part first-timers most often get wrong, so read carefully.

### Where does serial output go on bare metal?

In QEMU we read the kernel's UART (16550 at I/O port `0x3F8` / COM1) because QEMU
redirects it to your terminal (`-serial stdio`). **A MacBook has no physical
RS-232 serial port.** When the kernel writes to COM1 on this hardware, those
bytes go to a UART that **nothing is listening to** — effectively into the void.
Do **not** expect serial text to appear anywhere by default.

That leaves two realities:

1. **Primary verification on bare metal is the on-screen framebuffer, not
   serial.** The kernel must draw to the UEFI **GOP framebuffer** (the laptop's
   own display). Legacy VGA text mode (`0xB8000`) does **not** work under UEFI —
   if the kernel only writes VGA text, you will see **nothing** on a MacBook even
   on a perfect boot. Confirm the kernel's early console targets the GOP
   framebuffer.
2. **If you genuinely need serial capture**, you must add hardware: a
   USB/Thunderbolt → serial adapter does **not** expose COM1 (that's a host-side
   USB device, not the platform UART). Real options are a PCIe/Thunderbolt serial
   card the kernel can address, or simply relying on the framebuffer. For this
   verification, **we rely on the framebuffer.** Note this limitation in your
   recording.

### Expected on-screen sequence (healthy boot)

1. Apple logo / black screen briefly (firmware handing off).
2. Screen clears to the kernel's framebuffer console (background color flips to
   whatever the kernel sets — often black with light text).
3. Boot banner / version line prints.
4. Init log lines stream (memory map parsed, allocator up, subsystems init…).
5. A final **steady state**: either a known prompt, a heartbeat, a blinking
   cursor, or a static "boot complete / idle" message that **stays put and does
   not reboot**.

The decisive signal is **#5 holding steady**. A kernel that prints its banner and
then triple-faults will flash text and instantly reboot — which is why you film
it ([§8](#8-recording-setup)).

---

## 6. Verify a successful boot

A boot counts as **verified** when **all** of these hold:

- [ ] The framebuffer console appeared (screen changed from firmware/Apple logo
      to the kernel's own output).
- [ ] The expected boot banner / version string printed.
- [ ] The kernel reached its steady-state line/prompt/heartbeat.
- [ ] It **stayed there for ≥ 30 seconds** without rebooting, freezing
      mid-line, or panicking.
- [ ] No panic/`PANIC`/fault dump on screen.
- [ ] (If the build has one) any liveness indicator — blinking cursor, timer
      tick, heartbeat counter — is actually advancing, proving the kernel is
      *running*, not just halted on a static frame.

Record the result: machine model, macOS/firmware version, commit hash of the
build (`git rev-parse HEAD`), pass/fail, and the video file name.

> **"It shows text but is it alive?"** A frozen panic and a healthy idle loop can
> look identical if nothing animates. This is why a heartbeat/cursor matters. If
> there's no animation, the safest extra check is any input the kernel responds
> to (a keypress that echoes, etc.) — if the build supports it.

---

## 7. Emergency force shutdown

If the machine hangs, garbles the display, won't respond, or you simply need to
stop **— do this:**

> **Press and hold the physical power button for ~10 seconds** until the screen
> goes black and the machine powers off completely.

Notes:
- This is a hard power cut. It is the correct and expected way to stop a
  bare-metal kernel that has no shutdown path — our kernel generally cannot ACPI
  power-off cleanly yet.
- On Touch-ID MacBooks the **Touch ID button is the power button** — hold it.
- After it's off, **remove the USB stick** before powering back on if you want
  to return to macOS, or leave it in to retry.
- A hard cut is safe for *our* kernel (it isn't writing to the internal disk).
  It is only risky to whatever else might be mid-write — which is why §0 said put
  the machine on AC power and don't keep important unsaved work open elsewhere.

### Recovery checklist after a force shutdown

1. Wait ~5 seconds fully off before repowering (lets the power rail drain).
2. To get back into macOS: remove the USB, power on normally.
3. If the Mac won't boot back into macOS or behaves oddly: **reset NVRAM**
   (`Option-Cmd-P-R` held at startup on Intel Macs) and, if needed, the SMC.
4. If you changed Secure Boot in §4 and want to restore it, redo Startup
   Security Utility and set it back to Full Security.

---

## 8. Recording setup

The on-screen output is the **only** evidence (no serial — see §5), and a failing
boot may show its panic for under a second before rebooting. Film everything.

### What to capture

- **The laptop screen** — the primary subject. Must be readable: get close
  enough that banner/log text is legible on playback. This is the verification
  record.
- **The keyboard / operator's hands** — proves *what was pressed and when* (the
  `Option`-hold, the boot-picker selection, the force-shutdown hold). Disputes
  about "did you hold Option in time?" are settled here.
- **The USB stick going in** and the boot-picker screen with the EFI icon
  selected — establishes provenance (the right media, the right selection).

### Suggested two-camera (or two-phone) setup

| Camera | Frames | Purpose |
|--------|--------|---------|
| **Cam A — overhead / over-shoulder** | The full laptop: screen **and** keyboard in one frame | The master shot; ties actions to on-screen results in one timeline |
| **Cam B — screen close-up** | Tight on the display only | Legible text capture for the banner, log lines, and any panic dump |

If you only have one camera, use the **Cam A** angle (screen + hands in frame)
and physically lean in for the close-up at the moment the kernel console appears.

### Recording hygiene

- **Start recording before you press power.** The most important frames (firmware
  handoff, first console output, an early panic) happen in the first seconds.
- **Narrate out loud**: "Building commit `abc123`… holding Option now… selecting
  EFI Boot… kernel console up… banner reads vX… steady for 30 seconds… verified."
  The audio track becomes your timestamped log.
- **Stabilize Cam B** (tripod, stack of books) — handheld close-ups of fast
  scrolling text are unreadable.
- **Lighting:** avoid glare on the glossy MacBook screen. Angle the cameras off-
  axis from overhead lights, or tilt the lid slightly.
- **Keep filming through the force shutdown** so the recovery is documented too.
- Save the clip named with the commit hash and pass/fail, alongside the §6
  result record.

---

## Quick reference (one-screen cheat sheet)

```text
BUILD     cargo build --release --target x86_64-unknown-none
IMAGE     ./scripts/build_image.sh            -> build/golem.img
IDENTIFY  lsblk / diskutil list               (confirm size + external!)
FLASH     ./scripts/flash_usb.sh /dev/sdX     (whole disk, then sync)
BOOT      power on + HOLD Option (⌥) -> pick orange "EFI Boot" -> Return
WATCH     framebuffer on the laptop screen    (serial = nowhere on bare metal)
VERIFY    banner + steady state + no reboot for 30s + heartbeat alive
STOP      HOLD power button ~10s = hard off
FILM      screen + hands; start recording BEFORE pressing power
```

---

### Appendix: cross-check against QEMU

Before trusting (or doubting) a bare-metal result, re-run the same release
artifact in QEMU. If it boots clean in QEMU but not on the MacBook, the
divergence is almost always one of: (a) framebuffer — GOP vs. VGA-text console,
(b) Secure Boot rejecting the image, or (c) a real firmware/memory-map difference
QEMU papers over. Those three account for the large majority of first-time
bare-metal failures.
