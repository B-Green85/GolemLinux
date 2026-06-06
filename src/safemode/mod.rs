//! Safe Mode body — serial UART, the Sentinel configuration model, the
//! migration handoff block, and the interactive configuration terminal.
//!
//! This module is the substance of the `gkern-safemode` binary (the crate root
//! in `main.rs` is just entry/panic/halt plumbing). Everything here is
//! `no_std`-clean: it links `core` and the kernel `alloc` allocator, never
//! `std`, and uses only fixed-size stack buffers for its own work — no heap
//! allocation is required to drive the terminal.
//!
//! ## What this implements (Phase 5, Agent 1 deliverables 2, 5, 6)
//!
//! * A real full-duplex 16550 UART driver on COM1 (`serial`): the main kernel's
//!   `serial_write` is transmit-only; a configuration terminal needs to *read*
//!   operator keystrokes too, so we bring the receiver up here.
//! * [`SentinelConfig`] — the operator-tunable Sentinel parameters. Its fields
//!   mirror, one-for-one, the `Thresholds` the running kernel's Sentinel monitor
//!   consumes (`src/sentinel/monitor.rs`): four degradation-signal thresholds,
//!   three response-tier thresholds, and the sliding-window width. Defaults are
//!   the architecture-document defaults.
//! * [`MigrationBlock`] + [`SENTINEL_MIGRATION_BLOCK`] — the "known memory
//!   location" the operator's committed config is written to, so the standard
//!   OS can detect-and-migrate Sentinel state on its next boot (the login-time
//!   "did anything change?" query in `GOLEM_RUNTIME_ARCHITECTURE.md`
//!   § "Configuration Migration").
//! * [`run`] — the interactive terminal REPL.
//!
//! ## What this deliberately does NOT implement
//!
//! No agent or LLM registration of any kind. There is no IPC channel, no
//! handshake, no process table. The terminal's command set is closed and
//! recognises no registration verb; the few that an agent might probe for are
//! answered with an explicit refusal. Safe Mode being agent-free is structural.

use core::ptr;
use core::sync::atomic::{compiler_fence, Ordering};

// ===========================================================================
// Serial — 16550 UART on COM1, full duplex (transmit *and* receive)
// ===========================================================================

/// COM1 register file, polled (no interrupts — Safe Mode installs no IDT).
mod serial {
    /// COM1 I/O base. Legacy-fixed; QEMU forwards it to stdio.
    const COM1: u16 = 0x3F8;

    // Register offsets from the base (DLAB=0 unless noted).
    const RBR: u16 = 0; // Receive Buffer Register   (read,  DLAB=0)
    const THR: u16 = 0; // Transmit Holding Register (write, DLAB=0)
    const DLL: u16 = 0; // Divisor Latch Low         (DLAB=1)
    const IER: u16 = 1; // Interrupt Enable Register (DLAB=0)
    const DLM: u16 = 1; // Divisor Latch High        (DLAB=1)
    const FCR: u16 = 2; // FIFO Control Register      (write)
    const LCR: u16 = 3; // Line Control Register
    const MCR: u16 = 4; // Modem Control Register
    const LSR: u16 = 5; // Line Status Register

    const LSR_DATA_READY: u8 = 1 << 0; // a byte is waiting in the RBR
    const LSR_THR_EMPTY: u8 = 1 << 5; // the THR can accept a byte
    const LCR_DLAB: u8 = 1 << 7; // divisor-latch access bit
    const LCR_8N1: u8 = 0x03; // 8 data bits, no parity, 1 stop bit

    /// Read a byte from an I/O port.
    ///
    /// SAFETY: `in al, dx` is privileged; the caller must be at CPL 0 (always
    /// true in the kernel). Reading a UART register has no memory effects.
    #[inline]
    unsafe fn inb(port: u16) -> u8 {
        let value: u8;
        // SAFETY: privileged port read, legal at CPL 0; touches no memory.
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags),
        );
        value
    }

    /// Write a byte to an I/O port.
    ///
    /// SAFETY: `out dx, al` is privileged; the caller must be at CPL 0 (always
    /// true in the kernel). Writing a UART register has no memory effects.
    #[inline]
    unsafe fn outb(port: u16, value: u8) {
        // SAFETY: privileged port write, legal at CPL 0; touches no memory.
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }

    /// Program COM1 for 115200 8N1, FIFOs on, receiver enabled.
    ///
    /// SAFETY: CPL 0 required (privileged `out`). Must be called once before any
    /// `read`/`write`; it reprograms the divisor latch and line control.
    pub unsafe fn init() {
        // SAFETY of each step: see `inb`/`outb`; all are CPL-0 port I/O.
        unsafe {
            outb(COM1 + IER, 0x00); // disable all UART interrupts (we poll)
            outb(COM1 + LCR, LCR_DLAB); // unlock the divisor latch
            outb(COM1 + DLL, 0x01); // divisor 1  -> 115200 baud
            outb(COM1 + DLM, 0x00);
            outb(COM1 + LCR, LCR_8N1); // 8N1, latch relocked (DLAB cleared)
            outb(COM1 + FCR, 0xC7); // enable + clear FIFOs, 14-byte trigger
            outb(COM1 + MCR, 0x0B); // DTR | RTS | OUT2 (OUT2 gates the line)
        }
    }

    /// Transmit one byte, blocking until the THR is free.
    ///
    /// SAFETY: CPL 0 required. `init` must have run.
    pub unsafe fn write_byte(byte: u8) {
        // SAFETY: poll LSR (port read) then write THR (port write); CPL 0.
        unsafe {
            while inb(COM1 + LSR) & LSR_THR_EMPTY == 0 {
                core::hint::spin_loop();
            }
            outb(COM1 + THR, byte);
        }
    }

    /// Receive one byte, blocking until one arrives.
    ///
    /// SAFETY: CPL 0 required. `init` must have run.
    pub unsafe fn read_byte() -> u8 {
        // SAFETY: poll LSR (port read) then read RBR (port read); CPL 0.
        unsafe {
            while inb(COM1 + LSR) & LSR_DATA_READY == 0 {
                core::hint::spin_loop();
            }
            inb(COM1 + RBR)
        }
    }
}

/// Transmit-only early banner helper, usable before [`serial::init`] runs.
///
/// The main kernel's banner relies on the firmware/QEMU leaving COM1 in a
/// transmit-capable state at boot, and so do we for the pre-init messages in
/// `kernel_main`. It writes COM1's THR directly, one byte per call.
///
/// SAFETY: callers must be at CPL 0 (privileged `out`). It reads no memory the
/// caller does not own and writes only to the I/O port.
pub unsafe fn early_serial_write(s: &str) {
    for byte in s.bytes() {
        // SAFETY: `out dx, al` to COM1's THR (0x3F8). Privileged but legal at
        // CPL 0; touches no memory, preserves flags.
        core::arch::asm!(
            "out dx, al",
            in("dx") 0x3F8u16,
            in("al") byte,
            options(nomem, nostack, preserves_flags),
        );
    }
}

// ---------------------------------------------------------------------------
// Console helpers (built on the full-duplex driver, used after `serial::init`)
// ---------------------------------------------------------------------------

/// Write a string to COM1.
///
/// SAFETY: CPL 0; [`serial::init`] must have run.
unsafe fn print(s: &str) {
    for byte in s.bytes() {
        // CRLF normalisation: terminals expect a carriage return before the
        // line feed, but kernel strings use bare '\n'.
        if byte == b'\n' {
            // SAFETY: CPL 0, UART initialised.
            unsafe { serial::write_byte(b'\r') };
        }
        // SAFETY: CPL 0, UART initialised.
        unsafe { serial::write_byte(byte) };
    }
}

/// Write a single byte to COM1.
///
/// SAFETY: CPL 0; [`serial::init`] must have run.
unsafe fn putc(byte: u8) {
    // SAFETY: CPL 0, UART initialised.
    unsafe { serial::write_byte(byte) };
}

/// Print an unsigned integer in base 10.
///
/// SAFETY: CPL 0; [`serial::init`] must have run.
unsafe fn print_u32(mut value: u32) {
    // 10 digits is enough for any u32.
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    if value == 0 {
        // SAFETY: CPL 0, UART initialised.
        unsafe { putc(b'0') };
        return;
    }
    while value > 0 {
        i -= 1;
        buf[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    for &b in &buf[i..] {
        // SAFETY: CPL 0, UART initialised.
        unsafe { putc(b) };
    }
}

/// Print a 32-bit value as zero-padded 16-hex-digit (with `0x`) — used to tell
/// the operator the exact address the migration block was written to.
///
/// SAFETY: CPL 0; [`serial::init`] must have run.
unsafe fn print_hex64(value: u64) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    // SAFETY: CPL 0, UART initialised.
    unsafe {
        putc(b'0');
        putc(b'x');
    }
    for shift in (0..64).step_by(4).rev() {
        let nibble = ((value >> shift) & 0xF) as usize;
        // SAFETY: CPL 0, UART initialised.
        unsafe { putc(DIGITS[nibble]) };
    }
}

/// Print an `f32` to three decimal places (values here live in `0.0..=1.0`).
///
/// SAFETY: CPL 0; [`serial::init`] must have run.
unsafe fn print_f32(value: f32) {
    let mut v = value;
    if v < 0.0 {
        // SAFETY: CPL 0, UART initialised.
        unsafe { putc(b'-') };
        v = -v;
    }
    // Round to three decimals: multiply, add 0.5, truncate.
    let scaled = (v * 1000.0 + 0.5) as u32;
    let int = scaled / 1000;
    let frac = scaled % 1000;
    // SAFETY: CPL 0, UART initialised.
    unsafe {
        print_u32(int);
        putc(b'.');
        putc(b'0' + (frac / 100) as u8);
        putc(b'0' + ((frac / 10) % 10) as u8);
        putc(b'0' + (frac % 10) as u8);
    }
}

// ===========================================================================
// Sentinel configuration model
// ===========================================================================

/// The operator-tunable Sentinel parameters.
///
/// Field-for-field this is the running kernel's `sentinel::monitor::Thresholds`
/// (four degradation-signal thresholds, three response-tier thresholds, the
/// sliding-window width). We keep an independent, `#[repr(C)]`, dependency-free
/// copy here on purpose: Safe Mode must not pull in the Sentinel IPC / monitor
/// tree (it carries the agent-registration machinery, and Safe Mode registers
/// no agents). The migration consumer reconciles these fields back onto the
/// live `Thresholds` when the standard OS next boots.
///
/// All eight values use the architecture-document defaults via [`Self::DEFAULT`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SentinelConfig {
    // --- Degradation-signal thresholds (`0.0..=1.0`) -----------------------
    pub repetition: f32,
    pub self_referential: f32,
    pub token_velocity: f32,
    pub tool_retry: f32,
    // --- Response-tier thresholds (`0.0..=1.0`, soft <= medium <= hard) ----
    pub soft: f32,
    pub medium: f32,
    pub hard: f32,
    // --- Sliding output-window width, in observations ----------------------
    pub window: u32,
}

impl SentinelConfig {
    /// Defaults from `GOLEM_RUNTIME_ARCHITECTURE.md` (and
    /// `sentinel::monitor::Thresholds::default_config`).
    pub const DEFAULT: Self = Self {
        repetition: 0.6,
        self_referential: 0.5,
        token_velocity: 0.5,
        tool_retry: 0.4,
        soft: 0.4,
        medium: 0.7,
        hard: 0.9,
        window: 16,
    };

    /// A simple additive checksum over the byte representation of every field,
    /// little-endian. The migration consumer recomputes this to detect a
    /// torn or corrupted block before trusting it.
    pub fn checksum(&self) -> u32 {
        let mut sum: u32 = 0;
        let mut fold = |bytes: [u8; 4]| {
            for b in bytes {
                sum = sum.wrapping_add(b as u32);
            }
        };
        fold(self.repetition.to_le_bytes());
        fold(self.self_referential.to_le_bytes());
        fold(self.token_velocity.to_le_bytes());
        fold(self.tool_retry.to_le_bytes());
        fold(self.soft.to_le_bytes());
        fold(self.medium.to_le_bytes());
        fold(self.hard.to_le_bytes());
        fold(self.window.to_le_bytes());
        sum
    }
}

// ===========================================================================
// Migration handoff block — the "known memory location"
// ===========================================================================

/// Magic identifying a populated [`MigrationBlock`]: ASCII `"GLMSAFE0"`.
pub const MIGRATION_MAGIC: u64 = u64::from_le_bytes(*b"GLMSAFE0");

/// Wire format version of the migration block. Bump on any layout change so a
/// future migration consumer can refuse a block it does not understand.
pub const MIGRATION_VERSION: u32 = 1;

/// The fixed, stable handoff record Safe Mode publishes its committed config to.
///
/// `#[repr(C)]` so the layout is a stable contract with the (separately owned)
/// migration consumer rather than something `rustc` may reorder.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MigrationBlock {
    /// Must equal [`MIGRATION_MAGIC`] for the block to be considered populated.
    pub magic: u64,
    /// Layout version — [`MIGRATION_VERSION`].
    pub version: u32,
    /// Monotonic commit counter. Starts at 0 (never committed); each `commit`
    /// bumps it. Lets the consumer tell "config was touched" from "never set".
    pub sequence: u32,
    /// The committed configuration.
    pub config: SentinelConfig,
    /// [`SentinelConfig::checksum`] of `config`, recomputed by the consumer.
    pub checksum: u32,
    /// 0 until the first successful commit, then 1.
    pub committed: u32,
}

impl MigrationBlock {
    /// The pre-commit state: zeroed magic, default config, `committed == 0`.
    pub const EMPTY: Self = Self {
        magic: 0,
        version: MIGRATION_VERSION,
        sequence: 0,
        config: SentinelConfig::DEFAULT,
        checksum: 0,
        committed: 0,
    };
}

/// THE known memory location.
///
/// Exported with a stable, unmangled symbol name so the migration subsystem can
/// resolve it directly. It lives in the kernel image's `.bss`/`.data`, which the
/// bootloader maps read/write per `src/boot/linker.ld`, so writes here are valid
/// the moment `kernel_main` is entered — no extra page mapping required.
///
/// Single-writer, single-threaded: Safe Mode runs on one CPU with no scheduler,
/// so the only accessor is the operator's `commit` on this core.
#[no_mangle]
pub static mut SENTINEL_MIGRATION_BLOCK: MigrationBlock = MigrationBlock::EMPTY;

/// Publish `config` to [`SENTINEL_MIGRATION_BLOCK`], bumping the sequence.
///
/// Writes the whole record through a raw pointer (never forming a reference to
/// the `static mut`), with a compiler fence so the magic is the last logical
/// store ordered after the payload — a consumer that checks magic first will
/// not observe a half-written block under this single-threaded model.
///
/// Returns the address written and the new sequence number, for the operator
/// log. SAFETY: CPL 0; called only from the single Safe Mode core.
unsafe fn commit(config: SentinelConfig) -> (u64, u32) {
    let block_ptr = ptr::addr_of_mut!(SENTINEL_MIGRATION_BLOCK);

    // Read the prior sequence without forming a reference to the static.
    // SAFETY: `block_ptr` is the address of a live, aligned `MigrationBlock`;
    // `MigrationBlock: Copy`, so a `read` is sound. Single-threaded => no race.
    let prev = unsafe { ptr::read(block_ptr) };
    let next_seq = if prev.magic == MIGRATION_MAGIC {
        prev.sequence.wrapping_add(1)
    } else {
        1
    };

    let block = MigrationBlock {
        magic: MIGRATION_MAGIC,
        version: MIGRATION_VERSION,
        sequence: next_seq,
        config,
        checksum: config.checksum(),
        committed: 1,
    };

    // SAFETY: same provenance/alignment as above; we own the only access.
    unsafe { ptr::write(block_ptr, block) };
    // Order the payload store before any subsequent observation of the block.
    compiler_fence(Ordering::SeqCst);

    (block_ptr as u64, next_seq)
}

// ===========================================================================
// Interactive configuration terminal
// ===========================================================================

/// Initialise the UART and run the Sentinel configuration terminal forever.
///
/// This is the whole of Safe Mode's runtime behaviour. It never returns: when
/// the operator is done they power off / reboot into the standard OS, whose
/// login-time migration query picks up whatever was committed here.
pub fn run() -> ! {
    // SAFETY: CPL 0 (kernel ring); brings the COM1 receiver online so the
    // terminal can read keystrokes. Called exactly once.
    unsafe { serial::init() };

    // Working copy the operator edits; only `commit` publishes it.
    let mut config = SentinelConfig::DEFAULT;

    // SAFETY (all console calls below): CPL 0 and the UART is now initialised.
    unsafe {
        print("\n");
        print("==============================================================\n");
        print(" Golem Linux — Safe Mode Sentinel Configuration\n");
        print("==============================================================\n");
        print(" Agent-free recovery environment. No scheduler, no syscalls,\n");
        print(" no filesystem, no agent or LLM registration.\n");
        print(" This is the only context in which Sentinel may be configured.\n");
        print("\n");
        print(" Type 'help' for commands.\n");
        print("\n");
    }

    let mut line = [0u8; LINE_MAX];
    loop {
        // SAFETY: CPL 0, UART initialised.
        unsafe { print("safemode> ") };
        let len = read_line(&mut line);
        let input = &line[..len];
        dispatch(input, &mut config);
    }
}

/// Maximum command-line length. Fixed buffer — no allocation on the input path.
const LINE_MAX: usize = 128;

/// Read one line of operator input into `buf`, echoing as we go. Terminates on
/// CR or LF. Handles backspace/DEL. Bytes past `LINE_MAX` are dropped (with a
/// bell) rather than overflowing. Returns the number of bytes stored.
fn read_line(buf: &mut [u8; LINE_MAX]) -> usize {
    let mut len = 0usize;
    loop {
        // SAFETY: CPL 0, UART initialised (set up in `run` before any call).
        let byte = unsafe { serial::read_byte() };
        match byte {
            b'\r' | b'\n' => {
                // SAFETY: CPL 0, UART initialised.
                unsafe { print("\n") };
                return len;
            }
            0x08 | 0x7F => {
                // Backspace / DEL: erase the last char on screen and in buffer.
                if len > 0 {
                    len -= 1;
                    // SAFETY: CPL 0, UART initialised. "\b \b" rubs out the glyph.
                    unsafe { print("\x08 \x08") };
                }
            }
            byte if byte.is_ascii_graphic() || byte == b' ' => {
                if len < LINE_MAX {
                    buf[len] = byte;
                    len += 1;
                    // SAFETY: CPL 0, UART initialised — echo the keystroke.
                    unsafe { putc(byte) };
                } else {
                    // SAFETY: CPL 0, UART initialised — ring the bell, drop it.
                    unsafe { putc(0x07) };
                }
            }
            // Ignore other control bytes (arrow-key escapes, NUL, etc.).
            _ => {}
        }
    }
}

/// Parse and execute one command line against the working `config`.
fn dispatch(input: &[u8], config: &mut SentinelConfig) {
    let (cmd, rest) = split_first_token(input);
    if cmd.is_empty() {
        return; // blank line
    }

    if eq(cmd, b"help") || eq(cmd, b"?") {
        cmd_help();
    } else if eq(cmd, b"show") || eq(cmd, b"print") {
        cmd_show(config);
    } else if eq(cmd, b"set") {
        cmd_set(rest, config);
    } else if eq(cmd, b"reset") {
        *config = SentinelConfig::DEFAULT;
        // SAFETY: CPL 0, UART initialised.
        unsafe { print("config reset to defaults (working copy; 'commit' to publish)\n") };
    } else if eq(cmd, b"commit") || eq(cmd, b"save") {
        cmd_commit(config);
    } else if eq(cmd, b"register") || eq(cmd, b"agent") || eq(cmd, b"llm") || eq(cmd, b"spawn") {
        // Deliverable #4, made explicit: there is no agent/LLM registration in
        // Safe Mode. These verbs exist only to refuse them loudly.
        // SAFETY: CPL 0, UART initialised.
        unsafe {
            print("refused: Safe Mode registers no agents or LLM processes. ");
            print("No exceptions.\n");
        }
    } else {
        // SAFETY: CPL 0, UART initialised.
        unsafe {
            print("unknown command: '");
            print_bytes(cmd);
            print("'  (type 'help')\n");
        }
    }
}

/// Print the command help.
fn cmd_help() {
    // SAFETY: CPL 0, UART initialised.
    unsafe {
        print("commands:\n");
        print("  help                 show this help\n");
        print("  show                 show the current working configuration\n");
        print("  set <field> <value>  set a threshold (see fields below)\n");
        print("  reset                restore architecture-default thresholds\n");
        print("  commit               write the working config to the\n");
        print("                       migration block for the standard OS\n");
        print("\n");
        print("fields (thresholds 0.000..=1.000, window a positive integer):\n");
        print("  repetition        RepetitionScore signal threshold\n");
        print("  self_referential  SelfReferentialLoop signal threshold\n");
        print("  token_velocity    TokenVelocityStall signal threshold\n");
        print("  tool_retry        ToolRetryAnomaly signal threshold\n");
        print("  soft              Soft response-tier threshold\n");
        print("  medium            Medium response-tier threshold\n");
        print("  hard              Hard response-tier threshold\n");
        print("  window            sliding output window width (observations)\n");
    }
}

/// Print the working configuration.
fn cmd_show(config: &SentinelConfig) {
    // SAFETY (all): CPL 0, UART initialised.
    unsafe {
        print("Sentinel configuration (working copy):\n");
        print("  signals:\n");
        print("    repetition        = ");
        print_f32(config.repetition);
        print("\n    self_referential  = ");
        print_f32(config.self_referential);
        print("\n    token_velocity    = ");
        print_f32(config.token_velocity);
        print("\n    tool_retry        = ");
        print_f32(config.tool_retry);
        print("\n  tiers:\n");
        print("    soft              = ");
        print_f32(config.soft);
        print("\n    medium            = ");
        print_f32(config.medium);
        print("\n    hard              = ");
        print_f32(config.hard);
        print("\n  window              = ");
        print_u32(config.window);
        print("\n");
    }
}

/// Handle `set <field> <value>`.
fn cmd_set(rest: &[u8], config: &mut SentinelConfig) {
    let (field, value_tok) = split_first_token(rest);
    if field.is_empty() || value_tok.is_empty() {
        // SAFETY: CPL 0, UART initialised.
        unsafe { print("usage: set <field> <value>\n") };
        return;
    }

    // `window` is the one integer field; everything else is a 0..=1 threshold.
    if eq(field, b"window") {
        match parse_u32(value_tok) {
            Some(v) if v > 0 => {
                config.window = v;
                report_set(field, |c| {
                    // SAFETY: CPL 0, UART initialised.
                    unsafe { print_u32(c) }
                }, config.window);
            }
            _ => {
                // SAFETY: CPL 0, UART initialised.
                unsafe { print("error: window must be a positive integer\n") };
            }
        }
        return;
    }

    let value = match parse_f32(value_tok) {
        Some(v) => v,
        None => {
            // SAFETY: CPL 0, UART initialised.
            unsafe { print("error: value must be a decimal number, e.g. 0.65\n") };
            return;
        }
    };
    if !(0.0..=1.0).contains(&value) {
        // SAFETY: CPL 0, UART initialised.
        unsafe { print("error: threshold must be in 0.000..=1.000\n") };
        return;
    }

    let target: &mut f32 = if eq(field, b"repetition") {
        &mut config.repetition
    } else if eq(field, b"self_referential") {
        &mut config.self_referential
    } else if eq(field, b"token_velocity") {
        &mut config.token_velocity
    } else if eq(field, b"tool_retry") {
        &mut config.tool_retry
    } else if eq(field, b"soft") {
        &mut config.soft
    } else if eq(field, b"medium") {
        &mut config.medium
    } else if eq(field, b"hard") {
        &mut config.hard
    } else {
        // SAFETY: CPL 0, UART initialised.
        unsafe {
            print("error: unknown field '");
            print_bytes(field);
            print("'  (type 'help')\n");
        }
        return;
    };
    *target = value;

    // SAFETY: CPL 0, UART initialised.
    unsafe {
        print("set ");
        print_bytes(field);
        print(" = ");
        print_f32(value);
        print("\n");
    }

    // Advisory ordering check — Safe Mode lets the operator stage any values,
    // but a non-monotonic tier ladder is almost always a mistake worth flagging.
    if !(config.soft <= config.medium && config.medium <= config.hard) {
        // SAFETY: CPL 0, UART initialised.
        unsafe {
            print("  warning: tiers are not ordered soft <= medium <= hard\n");
        }
    }
}

/// Tiny helper so the `window` confirmation reads like the float path without
/// duplicating the surrounding prose. `printer` emits the value.
fn report_set(field: &[u8], printer: impl Fn(u32), value: u32) {
    // SAFETY: CPL 0, UART initialised.
    unsafe {
        print("set ");
        print_bytes(field);
        print(" = ");
    }
    printer(value);
    // SAFETY: CPL 0, UART initialised.
    unsafe { print("\n") };
}

/// Handle `commit` — publish the working config to the migration block.
fn cmd_commit(config: &SentinelConfig) {
    // SAFETY: CPL 0; single Safe Mode core; UART initialised.
    let (addr, seq) = unsafe { commit(*config) };
    // SAFETY: CPL 0, UART initialised.
    unsafe {
        print("committed Sentinel config to migration block\n");
        print("  symbol   : SENTINEL_MIGRATION_BLOCK\n");
        print("  address  : ");
        print_hex64(addr);
        print("\n  sequence : ");
        print_u32(seq);
        print("\n  checksum : ");
        print_u32(config.checksum());
        print("\n");
        print("The standard OS will detect and migrate this on next boot.\n");
    }
}

// ---------------------------------------------------------------------------
// Byte-string utilities (no allocation, no UTF-8 assumptions)
// ---------------------------------------------------------------------------

/// Byte-slice equality.
fn eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && {
        let mut i = 0;
        while i < a.len() {
            if a[i] != b[i] {
                return false;
            }
            i += 1;
        }
        true
    }
}

/// Split off the first whitespace-delimited token, returning `(token, rest)`
/// with leading whitespace trimmed from both. `rest` is ready to feed back in.
fn split_first_token(input: &[u8]) -> (&[u8], &[u8]) {
    let mut start = 0;
    while start < input.len() && input[start] == b' ' {
        start += 1;
    }
    let mut end = start;
    while end < input.len() && input[end] != b' ' {
        end += 1;
    }
    let token = &input[start..end];
    let mut rest_start = end;
    while rest_start < input.len() && input[rest_start] == b' ' {
        rest_start += 1;
    }
    (token, &input[rest_start..])
}

/// Print a raw byte slice (already validated to be graphic ASCII by the reader).
///
/// SAFETY: CPL 0; UART initialised.
unsafe fn print_bytes(bytes: &[u8]) {
    for &b in bytes {
        // SAFETY: CPL 0, UART initialised.
        unsafe { putc(b) };
    }
}

/// Parse a non-negative base-10 integer. Rejects empty input, non-digits, and
/// values that overflow `u32`.
fn parse_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(value)
}

/// Parse a simple decimal `f32`: optional sign, integer part, optional `.` and
/// fractional part. No exponent form — Safe Mode thresholds are plain decimals
/// like `0.65`. Returns `None` on any stray character or if no digit is present.
fn parse_f32(bytes: &[u8]) -> Option<f32> {
    if bytes.is_empty() {
        return None;
    }
    let mut i = 0;
    let mut negative = false;
    match bytes[0] {
        b'-' => {
            negative = true;
            i = 1;
        }
        b'+' => i = 1,
        _ => {}
    }

    let mut int_part: f32 = 0.0;
    let mut saw_digit = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        int_part = int_part * 10.0 + (bytes[i] - b'0') as f32;
        saw_digit = true;
        i += 1;
    }

    let mut frac: f32 = 0.0;
    let mut scale: f32 = 1.0;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            frac = frac * 10.0 + (bytes[i] - b'0') as f32;
            scale *= 10.0;
            saw_digit = true;
            i += 1;
        }
    }

    if i != bytes.len() || !saw_digit {
        return None; // trailing junk, or sign/dot with no digits
    }

    let mut value = int_part + frac / scale;
    if negative {
        value = -value;
    }
    Some(value)
}
