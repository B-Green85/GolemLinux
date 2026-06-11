# Phase 6 — Bare Metal

> The day Golem Linux stopped pretending.

For five phases, Golem lived inside a window. It booted under QEMU, it
serialized its console to a terminal tab, and every "boot" was really the
hypervisor doing the hard part — owning the page tables, owning the interrupt
controller, owning the clock. The kernel was real, but the world around it was
a courteous fiction. A VM forgives. It hands you a flat, well-behaved machine
and quietly papers over the places where the firmware lies, the timers drift,
and the hardware does not match its own datasheet.

Phase 6 ends that. Golem Linux now boots on real silicon — from the firmware
handoff, through the bootloader, into the kernel, to a login prompt — with no
hypervisor underneath. This document records how, and why it matters.

---

## 1. The Hardware Target

The target is a **2017 15-inch MacBook Pro** — internally `MacBookPro14,3`.

This was a deliberate choice, not a convenient one. The requirements were:

- **Kaby Lake (7th-gen Intel Core).** A mature, exhaustively documented
  x86_64 microarchitecture. Every quirk of its memory controller, its IGP, and
  its power management is public. Nothing about bringing up this CPU requires
  reverse engineering — the Intel Software Developer's Manual describes the
  whole thing.

- **No Apple T2.** This is the decisive constraint, and the reason a 2017
  machine was chosen over anything newer. Starting with the 2018 models, Apple
  put a T2 coprocessor in the boot path. The T2 owns the SSD controller, owns
  the camera, gates secure boot, and turns "install another OS on the bare
  drive" into a fight with a second computer you do not control. The 2017 MBP
  is the **last Intel MacBook Pro with no T2** — the storage and boot path are
  conventional. The machine boots the way a PC boots.

- **UEFI compliant.** Apple's firmware is a real, standards-compliant UEFI
  implementation. That means the firmware handoff to the bootloader follows the
  UEFI Boot Services contract — a `LoadImage`/`StartImage` path, a memory map
  we can request, a `GOP` framebuffer we can draw to. We are not bringing up a
  vendor's bespoke ROM. We are talking to a spec.

Put together: a fast, well-understood x86_64 CPU, a boot path with no
adversarial coprocessor in it, and firmware that honors an open standard. It is
arguably the friendliest piece of "real, modern, retail" hardware on which to
land a hobby OS — modern enough to matter, open enough to be possible.

---

## 2. The Cross-Compilation Story

Here is the part that should not work, and does.

**Golem Linux for x86_64 is built on an Apple Silicon Mac.** An M-series ARM64
machine — a CPU that cannot execute a single byte of the output — produces the
x86_64 kernel, bootloader, and userland that boot the 2017 MBP.

This is the everyday miracle of a properly configured cross toolchain, and it
is worth stating plainly because it is exactly the kind of thing that feels
impossible until it is mundane:

- **The compiler does not care what it runs on.** A cross-compiler targeting
  `x86_64-unknown-none` (freestanding, no host OS, no libc) emits x86_64
  machine code regardless of whether the compiler binary itself is ARM64 or
  x86_64. The *host* and the *target* are independent axes. We build on ARM,
  for x86.

- **Freestanding is the whole trick.** Because the kernel links against no host
  runtime — no libc, no dynamic loader, its own `_start`, its own linker
  script — there is nothing host-shaped to leak in. The output is a flat
  x86_64 image that answers only to the firmware and the hardware. The build
  host is invisible in the artifact.

- **The Mac never executes the result.** Not once. The M-series machine
  compiles, links, and lays the image onto a boot medium. The *first* CPU to
  ever run these instructions is the Kaby Lake chip in the MacBook Pro. We
  ship code that our build machine is physically incapable of running.

There is a pleasing symmetry to it: a 2024-era Apple ARM laptop building an OS
for a 2017-era Apple Intel laptop. One Mac builds the operating system the
other Mac was never meant to run.

---

## 3. The Boot Process, End to End

This is the full chain, from cold power to login prompt, with the hypervisor
removed from every link.

1. **Power-on → Apple UEFI firmware.** The CPU comes out of reset, the
   firmware initializes RAM and the platform, and enumerates boot candidates.
   On a no-T2 machine the internal storage is a conventional path; the firmware
   finds our EFI System Partition and the `BOOTX64.EFI` within it.

2. **Firmware → bootloader (UEFI Boot Services).** The firmware loads our
   bootloader as a UEFI application and calls into its entry point with Boot
   Services still live. While we hold that handle we do the things only the
   firmware can do for us:
   - request the **UEFI memory map** (the authoritative picture of usable RAM),
   - acquire the **GOP framebuffer** (a real linear framebuffer on real glass),
   - load the kernel image into memory.

3. **`ExitBootServices` — the point of no return.** The bootloader calls
   `ExitBootServices`. The firmware steps back; from this instruction onward
   *nothing* is managing the machine but us. No VM to catch our mistakes, no
   firmware babysitter. We own the memory map, the framebuffer, and the CPU.

4. **Bootloader → kernel handoff.** Control transfers to the kernel entry with
   the memory map and framebuffer pointer passed through. The kernel takes the
   firmware's RAM picture as ground truth and begins to build its own world on
   top of it.

5. **Kernel early init.** The kernel:
   - installs its own **GDT** and **IDT** (it owns segmentation and the
     interrupt vector table now — no hypervisor injecting them),
   - builds its own **page tables** and switches to its own address space,
   - brings up the **local APIC** and the timer for a real, ticking clock,
   - initializes the console against the **real GOP framebuffer** — text
     rendered onto the laptop's actual panel, not a serial stream piped to a
     terminal tab.

6. **Userland → login.** With memory, interrupts, and the timer live, the
   kernel hands off to userland and the system comes up to a **login prompt** —
   on the screen of the machine itself.

Every step that the hypervisor used to perform on Golem's behalf — owning the
page tables, the interrupt controller, the clock — Golem now performs for
itself. That is the difference between Phase 5 and Phase 6 in one sentence.

---

## 4. The First Bare Metal Boot

The first boot does not look like the QEMU boots did.

In QEMU, the console was a clean serial stream in a terminal tab, on a window
you could resize. The first bare metal boot is the laptop's own display lighting
up — the GOP framebuffer driven directly, Golem's console rendered onto the
panel the firmware just handed us. The fans spin. The keyboard is the machine's
keyboard. There is no host OS behind the glass to fall back to. If the kernel
faults, the screen is what tells you, because the screen is all there is.

The milestone is small and total at the same time: **a login prompt, on the
display of a 2017 MacBook Pro, with nothing underneath it but Golem.** No
hypervisor. No host kernel. No emulator. The firmware handed off, called
`ExitBootServices`, and never came back — and the machine kept running, because
Golem was there to run it.

---

## 5. The Significance: This Is Not a VM

This is the line the project crosses in Phase 6, and it does not uncross.

A virtual machine is a comfortable lie. The hypervisor presents an idealized,
spec-perfect machine and silently absorbs the gap between what the datasheet
promises and what the hardware does. Under a VM you can be wrong about the
memory map, sloppy about timer setup, and naive about the framebuffer, and the
hypervisor will cover for you. **Bare metal does not cover for you.** The
firmware tells you what it tells you; the timers drift the way they actually
drift; the framebuffer is at the address the GOP says it is and not one byte
elsewhere. Everything that works, works because it is *correct*, not because
something underneath was being kind.

So the claim Phase 6 earns is simple and it is the whole point of the project:

> **Golem Linux is not a VM. It runs on real silicon.**

It boots a real, retail, modern x86_64 laptop, from firmware handoff to login,
owning the memory, the interrupts, the clock, and the display itself. The
training wheels are off and they are not going back on. This is the transition
from **prototype to operating system** — from a thing that runs in a window to
a thing that runs a computer.

---

## 6. Next Steps

Standing on bare metal changes what the roadmap is *about*. The questions stop
being "does the abstraction hold?" and become "does the hardware?"

- **Real device drivers.** The framebuffer is ours, but the rest of the machine
  is not yet: NVMe storage, USB (and therefore the built-in keyboard and
  trackpad without firmware help), and networking. Each is a real driver
  against real silicon, with no virtio shim to lean on.

- **SMP — bring up the other cores.** Phase 6 lands on the boot processor. The
  Kaby Lake part has more cores sitting idle behind their reset vectors. Sending
  the INIT/SIPI sequence and bringing the application processors online is the
  next genuine hardware milestone.

- **ACPI.** On real hardware, ACPI is how you learn the platform's actual
  topology — interrupt routing, power states, the shape of the machine. Parsing
  the tables the firmware left us replaces guesswork with the platform's own
  description of itself.

- **Power, thermal, and suspend.** A laptop is a thermally and battery
  constrained machine. Eventually Golem has to be a good citizen of the
  hardware it runs on — frequency scaling, sleep states, not melting.

- **Persistence and a real root.** With an NVMe driver, the EFI System
  Partition stops being just a launch pad and the machine gets a real,
  writable root filesystem of its own.

- **Hardening the boot path.** Now that `ExitBootServices` is a cliff with no
  net, the early path deserves the care that a no-going-back transition
  demands — robust handling of whatever memory map and framebuffer the
  firmware actually hands us, across cold boot, warm boot, and the awkward
  states in between.

---

*Phase 6 is the phase where Golem Linux became real. Everything before it was a
prototype that ran in a window. Everything after it runs on a computer.*
