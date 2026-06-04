#!/usr/bin/env bash
#
# debug_qemu.sh — Launch the Golem Linux kernel under QEMU with a GDB stub.
#
# This is the QEMU side of the debug session. It boots the kernel image and
# halts the virtual CPU at the reset vector, then waits for a debugger to
# attach on TCP port 1234. See the "GDB SIDE" reference block at the bottom of
# this file for how to drive the debugger once QEMU is up.
#
# Usage:
#   ./scripts/debug_qemu.sh [path-to-bootable-image]
#
# If no image path is given, the default kernel binary location is used.
#
set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

# GDB remote stub port. Must match the ":1234" used by the GDB side / .gdbinit.
GDB_PORT="${GDB_PORT:-1234}"

# Kernel symbol file — the ELF with debug info that GDB loads via `symbol-file`.
KERNEL_SYMBOLS="target/x86_64-unknown-none/debug/gkern"

# Bootable image to run in QEMU. Defaults to the debug kernel binary; override
# by passing a path (e.g. a bootimage / disk image) as the first argument.
KERNEL_IMAGE="${1:-${KERNEL_SYMBOLS}}"

# QEMU binary for the x86_64 target.
QEMU="${QEMU:-qemu-system-x86_64}"

# ---------------------------------------------------------------------------
# Sanity checks
# ---------------------------------------------------------------------------

if ! command -v "${QEMU}" >/dev/null 2>&1; then
    echo "error: '${QEMU}' not found on PATH. Install QEMU (qemu-system-x86_64)." >&2
    exit 1
fi

if [[ ! -f "${KERNEL_IMAGE}" ]]; then
    echo "error: kernel image not found: ${KERNEL_IMAGE}" >&2
    echo "       Build the kernel first (e.g. 'cargo build'), or pass an image path." >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Launch QEMU
# ---------------------------------------------------------------------------
#
# Key flags:
#   -s   Shorthand for '-gdb tcp::1234' — opens a GDB stub on port 1234.
#   -S   Freeze the CPU at startup; do not begin execution until the debugger
#        issues 'continue'. This guarantees you can set breakpoints (e.g. at
#        kernel_main) before any kernel code runs.
#
# When GDB_PORT is customised we expand '-s' to the explicit '-gdb' form so the
# override actually takes effect.

echo "Booting kernel under QEMU with GDB stub on port ${GDB_PORT}."
echo "QEMU is halted (-S); attach GDB and 'continue' to start execution."
echo "Image: ${KERNEL_IMAGE}"
echo

if [[ "${GDB_PORT}" == "1234" ]]; then
    GDB_FLAGS=(-s -S)
else
    GDB_FLAGS=(-gdb "tcp::${GDB_PORT}" -S)
fi

exec "${QEMU}" \
    -drive format=raw,file="${KERNEL_IMAGE}" \
    "${GDB_FLAGS[@]}" \
    -serial stdio \
    -no-reboot \
    -no-shutdown

# ===========================================================================
# GDB SIDE — run this in a SEPARATE terminal once QEMU is waiting
# ===========================================================================
#
# QEMU launched above blocks while halted, so open a second terminal at the
# repo root and start GDB there. Two equivalent options:
#
#   Option A — use the checked-in .gdbinit (recommended):
#
#       gdb
#
#     GDB auto-sources ./.gdbinit from the repo root, which runs:
#         set architecture i386:x86-64
#         target remote :1234
#         symbol-file target/x86_64-unknown-none/debug/gkern
#         break kernel_main
#         continue
#     You land stopped at kernel_main, ready to debug.
#
#   Option B — drive GDB manually (the same commands, step by step):
#
#       gdb
#       (gdb) set architecture i386:x86-64
#       (gdb) target remote :1234                              # attach to QEMU stub
#       (gdb) symbol-file target/x86_64-unknown-none/debug/gkern   # load kernel symbols
#       (gdb) break kernel_main                                # breakpoint at entry
#       (gdb) continue                                         # release the halted CPU
#
# ---------------------------------------------------------------------------
# Handy GDB commands once stopped at kernel_main (debugging reference):
# ---------------------------------------------------------------------------
#   continue        / c          Resume execution until the next breakpoint.
#   next            / n          Step over one source line.
#   step            / s          Step into one source line.
#   stepi           / si         Step a single machine instruction.
#   finish                       Run until the current function returns.
#   backtrace       / bt         Print the call stack.
#   info registers  / i r        Dump CPU registers.
#   info breakpoints             List all breakpoints.
#   break <fn|file:line>         Set another breakpoint.
#   x/16xb <addr>                Examine 16 bytes of memory in hex.
#   print <expr>    / p <expr>   Evaluate / print a variable or expression.
#   layout asm                   TUI view of disassembly (layout src for source).
#   detach                       Detach from QEMU, leaving it running.
#   quit            / q          Exit GDB (QEMU keeps running unless killed).
# ===========================================================================
