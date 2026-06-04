# .gdbinit — auto-sourced by GDB when started from the repo root.
#
# Drives the GDB side of a QEMU debug session started via
# scripts/debug_qemu.sh (QEMU launched with `-s -S`, halted, GDB stub on :1234).
#
# Connect, load kernel debug symbols, break at the kernel entry point, then
# release the halted CPU so it runs up to kernel_main.
set architecture i386:x86-64
target remote :1234
symbol-file target/x86_64-unknown-none/debug/gkern
break kernel_main
continue
