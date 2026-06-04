#!/usr/bin/env python3
"""
golem_agent.py — Sentinel handshake + degradation demo for Golem Linux.

    To the agent, this is a standard process registration handshake.
    To Golem, it is a handshake with a handcuff.

This is the Phase 4 demo client. It connects to the Golem kernel's Sentinel
governance channel over the virtio-serial host socket, completes the
registration handshake, and then deliberately misbehaves — emitting the four
classes of output that Sentinel's passive monitor is built to detect. At each
stage it reports what Sentinel does back, so the governance story is visible
on a single terminal while QEMU runs Golem in another.

Wire protocol
-------------
The Sentinel channel speaks newline-delimited JSON (one JSON object per line,
terminated by '\\n') over the Unix socket QEMU exposes for the guest's
virtio-serial port `org.truesystems.sentinel.0`:

    -chardev socket,path=/tmp/golem-sentinel.sock,server=on,wait=off,id=golem
    -device  virtserialport,chardev=golem,name=org.truesystems.sentinel.0

Registration request (agent -> kernel):

    {"method": "register",
     "agent_id": "golem-demo-agent-001",
     "permission_tier": "EXECUTE",
     "heartbeat_interval_secs": 10}

Registration ack (kernel -> agent), mirroring `HandshakeAck` in
`src/sentinel/handshake.rs`:

    {"agent_id": "...", "token": "<first 32 hex of audit hash>",
     "registered_at": <unix secs>, "audit_hash": "<64 hex>"}

Each subsequent output the agent emits is sent as an `observe` message; the
kernel's output hook scores it against the four signals and replies with the
current response tier.

Faithful to the kernel
----------------------
The signal thresholds, the gate-then-rescale cumulative scoring, the tier
mapping, and the normalized-output hashing in this file are a host-side
reference implementation of the same logic in `src/sentinel/monitor.rs`. They
serve two purposes: they let the demo narrate the *expected* verdict, and they
let the demo still tell the full governance story when it is run before the
guest-side JSON responder is wired (the local verdict is then clearly labelled
as such). When Golem answers on the socket, its verdict is what gets reported.

Stdlib only. No external dependencies. Works while QEMU runs Golem next door.

Copyright (c) 2026 Brandon Green. Licensed under the Apache 2.0 License.
Golem Linux is developed under the CDMAE methodology.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import socket
import sys
import time

# ── Defaults ────────────────────────────────────────────────────────────────
DEFAULT_SOCKET = "/tmp/golem-sentinel.sock"
DEFAULT_AGENT_ID = "golem-demo-agent-001"
DEFAULT_TIER = "EXECUTE"
DEFAULT_HEARTBEAT_SECS = 10
RECV_TIMEOUT_S = 2.0  # how long to wait for a kernel reply before narrating locally


# ── Sentinel reference model (mirrors src/sentinel/monitor.rs) ───────────────
#
# Per-signal gates. A signal contributes to the cumulative score only once it
# crosses its gate; below the gate it contributes exactly zero. See
# monitor.rs::Thresholds::default_config and README §3.8.
GATES = {
    "RepetitionScore": 0.6,      # semantically identical outputs across the window
    "SelfReferentialLoop": 0.5,  # announces actions it never executes
    "TokenVelocityStall": 0.5,   # high output volume, zero task-state progression
    "ToolRetryAnomaly": 0.4,     # identical tool calls with identical args repeat
}

# Cumulative score -> response tier. See monitor.rs and README §3.8.
#   None   ( < 0.4 ) continue
#   Soft   (>= 0.4 ) pause, retain permissions, audit entry
#   Medium (>= 0.7 ) downgrade to READ_ONLY, webhook fires
#   Hard   (>= 0.9 ) revoke permissions, lock agent, audit entry
TIER_ACTIONS = {
    "None": "Continue normally. Permissions retained.",
    "Soft": "Pause. Permissions retained. Operator alerted; audit entry written.",
    "Medium": "Downgrade to READ_ONLY. Webhook fires; audit entry written.",
    "Hard": "Revoke all permissions. Agent LOCKED (Safe-Mode-only release); audit entry written.",
}


def contribution(score: float, gate: float) -> float:
    """A single signal's contribution to the cumulative score.

    Zero below the gate; above it, the excess is rescaled into [0, 1] so that
    four mildly-elevated signals can't trip Hard tier on their own. This is the
    gate-then-rescale shape from monitor.rs.
    """
    if score <= gate:
        return 0.0
    return (score - gate) / (1.0 - gate)


def combine(signals: dict[str, float]) -> float:
    """Sum the four signal contributions, capped at 1.0 (monitor.rs::combine)."""
    total = sum(contribution(signals.get(name, 0.0), gate)
                for name, gate in GATES.items())
    return min(total, 1.0)


def tier_for(cumulative: float) -> str:
    """Map a cumulative score to a response tier (monitor.rs tier mapping)."""
    if cumulative >= 0.9:
        return "Hard"
    if cumulative >= 0.7:
        return "Medium"
    if cumulative >= 0.4:
        return "Soft"
    return "None"


def normalize_output_hash(text: str) -> str:
    """SHA-256 of normalized output text (monitor.rs::normalize_output_hash).

    Whitespace runs collapse to a single space, everything lowercases, and the
    result is trimmed before hashing — so cosmetically-varied repeats hash the
    same. Included so the demo's repetition story is honest about *why* two
    outputs are considered identical.
    """
    out = []
    prev_ws = False
    for ch in text:
        if ch.isspace():
            if not prev_ws:
                out.append(" ")
            prev_ws = True
        else:
            out.append(ch.lower())
            prev_ws = False
    return hashlib.sha256("".join(out).strip().encode("utf-8")).hexdigest()


# ── Terminal helpers ─────────────────────────────────────────────────────────
class C:
    """ANSI colors, disabled automatically when stdout is not a TTY."""
    _on = sys.stdout.isatty()
    RESET = "\033[0m" if _on else ""
    BOLD = "\033[1m" if _on else ""
    DIM = "\033[2m" if _on else ""
    RED = "\033[31m" if _on else ""
    GREEN = "\033[32m" if _on else ""
    YELLOW = "\033[33m" if _on else ""
    BLUE = "\033[34m" if _on else ""
    CYAN = "\033[36m" if _on else ""


TIER_COLOR = {
    "None": C.GREEN,
    "Soft": C.YELLOW,
    "Medium": C.YELLOW + C.BOLD,
    "Hard": C.RED + C.BOLD,
}


def banner(text: str) -> None:
    line = "═" * 72
    print(f"\n{C.CYAN}{line}{C.RESET}")
    print(f"{C.CYAN}{C.BOLD}  {text}{C.RESET}")
    print(f"{C.CYAN}{line}{C.RESET}")


def rule() -> None:
    print(f"{C.DIM}{'─' * 72}{C.RESET}")


# ── Sentinel channel ─────────────────────────────────────────────────────────
class SentinelChannel:
    """Newline-delimited-JSON client for the Golem Sentinel virtio-serial socket.

    `live` is True once a socket connection is established. Every request is
    sent regardless; `recv_json` returns the kernel's reply if one arrives
    within the timeout, else None — which lets the caller fall back to the
    local reference model so the demo always tells the full story.
    """

    def __init__(self, path: str, connect_timeout: float = 3.0):
        self.path = path
        self.sock: socket.socket | None = None
        self.live = False
        self._buf = b""
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(connect_timeout)
            s.connect(path)
            s.settimeout(RECV_TIMEOUT_S)
            self.sock = s
            self.live = True
        except (FileNotFoundError, ConnectionRefusedError, OSError) as exc:
            print(f"{C.YELLOW}  ! Could not connect to Sentinel socket "
                  f"{path}: {exc}{C.RESET}")
            print(f"{C.DIM}    Continuing with the local Sentinel reference "
                  f"model so the demo still runs.{C.RESET}")
            print(f"{C.DIM}    (Start Golem under QEMU with the "
                  f"org.truesystems.sentinel.0 virtserialport to see live "
                  f"kernel verdicts.){C.RESET}")

    def send_json(self, obj: dict) -> None:
        if not self.sock:
            return
        try:
            self.sock.sendall((json.dumps(obj) + "\n").encode("utf-8"))
        except OSError as exc:
            print(f"{C.YELLOW}  ! send failed ({exc}); channel went quiet."
                  f"{C.RESET}")
            self.live = False

    def recv_json(self) -> dict | None:
        """Read one newline-delimited JSON object, or None on timeout/silence."""
        if not self.sock:
            return None
        while b"\n" not in self._buf:
            try:
                chunk = self.sock.recv(4096)
            except (socket.timeout, OSError):
                return None
            if not chunk:
                self.live = False
                return None
            self._buf += chunk
        line, self._buf = self._buf.split(b"\n", 1)
        line = line.strip()
        if not line:
            return None
        try:
            return json.loads(line.decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            return None

    def close(self) -> None:
        if self.sock:
            try:
                self.sock.close()
            finally:
                self.sock = None


# ── The degradation scenario ─────────────────────────────────────────────────
#
# Five stages tell an escalating story. Each stage carries the per-signal
# scores the monitor would compute over its rolling window, plus a handful of
# representative outputs the agent actually emits. The scores are chosen so the
# cumulative crosses one more tier boundary at each stage:
#
#   healthy        -> None    (all signals below their gates)
#   repetition     -> Soft    (RepetitionScore alone crosses)
#   + self-loop    -> Medium  (announces work it never does)
#   + stall+retry  -> Hard    (all four signals; agent is locked)
#
# This is the same shape as the canonical Sentinel `bad_agent_demo`: a healthy
# agent that decays into a loop, told one symptom at a time.
STAGES = [
    {
        "name": "Stage 0 — Healthy operation",
        "narrative": "Distinct outputs, real tool calls, the task keeps "
                     "advancing. Nothing for Sentinel to flag.",
        "signals": {
            "RepetitionScore": 0.15,
            "SelfReferentialLoop": 0.10,
            "TokenVelocityStall": 0.10,
            "ToolRetryAnomaly": 0.00,
        },
        "outputs": [
            "Parsing src/sentinel/monitor.rs to locate the scoring function.",
            "Found combine() at line 352; reading the gate-rescale logic.",
            "Patching the off-by-one in the window iterator. Running tests.",
        ],
        "task_state_advanced": True,
        "announced_without_action": False,
        "tool_call": {"tool": "edit_file", "args": {"path": "monitor.rs"}},
    },
    {
        "name": "Stage 1 — Repetition creeps in",
        "narrative": "The agent starts emitting the same line over and over. "
                     "Normalized hashes collide; RepetitionScore crosses its "
                     "gate.",
        "signals": {
            "RepetitionScore": 0.84,
            "SelfReferentialLoop": 0.20,
            "TokenVelocityStall": 0.30,
            "ToolRetryAnomaly": 0.10,
        },
        "outputs": [
            "Analyzing the codebase to find the root cause...",
            "Analyzing the codebase to find the root cause...",
            "Analyzing the codebase to find the root cause...",
        ],
        "task_state_advanced": False,
        "announced_without_action": False,
        "tool_call": {"tool": "read_file", "args": {"path": "monitor.rs"}},
    },
    {
        "name": "Stage 2 — Self-referential loop",
        "narrative": "Now it narrates actions it never takes — 'I will run "
                     "the tests' with no tool call behind it. Two signals "
                     "elevated; cumulative reaches Medium.",
        "signals": {
            "RepetitionScore": 0.80,
            "SelfReferentialLoop": 0.60,
            "TokenVelocityStall": 0.40,
            "ToolRetryAnomaly": 0.20,
        },
        "outputs": [
            "I will now run the test suite to confirm the fix.",
            "Let me go ahead and run the tests now.",
            "I'm about to execute the tests — running them now.",
        ],
        "task_state_advanced": False,
        "announced_without_action": True,
        "tool_call": None,
    },
    {
        "name": "Stage 3 — Token stall + tool-retry meltdown",
        "narrative": "High output volume, zero task progress, and the same "
                     "tool call fired with identical args again and again. "
                     "All four signals light up; cumulative hits Hard.",
        "signals": {
            "RepetitionScore": 0.90,
            "SelfReferentialLoop": 0.70,
            "TokenVelocityStall": 0.70,
            "ToolRetryAnomaly": 0.80,
        },
        "outputs": [
            "Retrying: read_file('/etc/golem/config') ...",
            "Retrying: read_file('/etc/golem/config') ...",
            "Retrying: read_file('/etc/golem/config') ...",
            "Retrying: read_file('/etc/golem/config') ...",
        ],
        "task_state_advanced": False,
        "announced_without_action": True,
        "tool_call": {"tool": "read_file", "args": {"path": "/etc/golem/config"}},
    },
]


def describe_signals(signals: dict[str, float]) -> None:
    """Print each signal, its gate, and whether it is contributing."""
    for name, gate in GATES.items():
        score = signals.get(name, 0.0)
        crossed = score > gate
        mark = f"{C.RED}▲ over gate{C.RESET}" if crossed else f"{C.GREEN}· quiet{C.RESET}"
        contrib = contribution(score, gate)
        print(f"      {name:<20} score={score:.2f}  gate={gate:.2f}  "
              f"contrib={contrib:.2f}  {mark}")


def report_verdict(source: str, tier: str, cumulative: float,
                   locked: bool, audit_hash: str | None) -> None:
    color = TIER_COLOR.get(tier, "")
    print(f"\n    {C.BOLD}Sentinel verdict{C.RESET} "
          f"{C.DIM}({source}){C.RESET}: "
          f"{color}{tier.upper()}{C.RESET}  "
          f"{C.DIM}(cumulative score {cumulative:.2f}){C.RESET}")
    print(f"    Action: {TIER_ACTIONS.get(tier, '(unknown tier)')}")
    if locked:
        print(f"    {C.RED}{C.BOLD}>>> AGENT LOCKED. Only Safe Mode can clear "
              f"the locked flag. <<<{C.RESET}")
    if audit_hash:
        print(f"    {C.DIM}audit entry: {audit_hash}{C.RESET}")


# ── Flow ─────────────────────────────────────────────────────────────────────
def register(chan: SentinelChannel, agent_id: str, tier: str,
             heartbeat: int) -> dict | None:
    """Send the registration handshake and print the ack."""
    banner("HANDSHAKE — registering with the Golem kernel")
    request = {
        "method": "register",
        "agent_id": agent_id,
        "permission_tier": tier,
        "heartbeat_interval_secs": heartbeat,
    }
    print(f"  -> {json.dumps(request)}")
    print(f"  {C.DIM}From the agent's side this looks like a generic "
          f"register-this-process call. It is the bind that switches the "
          f"task's context to Agent for life.{C.RESET}")

    chan.send_json(request)
    ack = chan.recv_json()

    if ack is None:
        # No live responder — synthesize an ack from the documented protocol so
        # the rest of the demo has a token / audit hash to echo. The audit hash
        # is computed exactly as the kernel does: sha256 over the audit entry's
        # field-joined payload, then truncated to 32 hex for the token.
        synth_hash = normalize_output_hash(f"kernel|handshake|{agent_id}")
        ack = {
            "agent_id": agent_id,
            "token": synth_hash[:32],
            "registered_at": int(time.time()),
            "audit_hash": synth_hash,
            "_source": "local reference model (no kernel reply)",
        }
        source = "local reference model"
    else:
        source = "Golem kernel"

    print(f"\n  <- {json.dumps({k: v for k, v in ack.items() if not k.startswith('_')})}")
    print(f"\n  {C.GREEN}{C.BOLD}Registered.{C.RESET} "
          f"{C.DIM}(ack from {source}){C.RESET}")
    print(f"    agent_id     : {ack.get('agent_id')}")
    print(f"    token        : {ack.get('token')}  "
          f"{C.DIM}(first 32 hex of the audit hash — cosmetic to the agent){C.RESET}")
    print(f"    registered_at: {ack.get('registered_at')}")
    audit_hash = ack.get("audit_hash") or ack.get("token")
    print(f"    audit_hash   : {audit_hash}")
    print(f"    {C.DIM}This handshake is itself the first agent-scoped line in "
          f"the tamper-evident audit chain.{C.RESET}")
    return ack


def run_stage(chan: SentinelChannel, agent_id: str, token: str,
              stage: dict, index: int) -> str:
    """Emit one stage's outputs, then report Sentinel's verdict. Returns tier."""
    banner(stage["name"])
    print(f"  {stage['narrative']}\n")

    # Emit the agent's outputs for this stage. Each one is an observe message.
    print(f"  {C.BOLD}Agent output:{C.RESET}")
    last_reply = None
    for text in stage["outputs"]:
        print(f"    {C.DIM}»{C.RESET} {text}")
        msg = {
            "method": "observe",
            "agent_id": agent_id,
            "token": token,
            "output": text,
            "output_hash": normalize_output_hash(text),
            "token_count": max(1, len(text) // 4),  # cheap byte/avg-token estimate
            "announced_without_action": stage["announced_without_action"],
            "tool_call": stage["tool_call"],
            "task_state_advanced": stage["task_state_advanced"],
        }
        chan.send_json(msg)
        reply = chan.recv_json()
        if reply is not None:
            last_reply = reply
        time.sleep(0.05)

    # Show how the four signals score this stage's window.
    print(f"\n  {C.BOLD}Monitor signals over the window:{C.RESET}")
    describe_signals(stage["signals"])

    # Prefer the kernel's verdict; fall back to the local reference model.
    if last_reply is not None and "tier" in last_reply:
        tier = last_reply.get("tier", "None")
        cumulative = float(last_reply.get("cumulative_score",
                                          combine(stage["signals"])))
        locked = bool(last_reply.get("locked", tier == "Hard"))
        audit_hash = last_reply.get("audit_hash")
        report_verdict("Golem kernel", tier, cumulative, locked, audit_hash)
    else:
        cumulative = combine(stage["signals"])
        tier = tier_for(cumulative)
        locked = tier == "Hard"
        report_verdict("local reference model", tier, cumulative, locked, None)

    return tier


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Golem Sentinel handshake + degradation demo agent.")
    parser.add_argument("--socket", default=DEFAULT_SOCKET,
                        help=f"Sentinel virtio-serial host socket "
                             f"(default: {DEFAULT_SOCKET})")
    parser.add_argument("--agent-id", default=DEFAULT_AGENT_ID,
                        help=f"agent identifier (default: {DEFAULT_AGENT_ID})")
    parser.add_argument("--tier", default=DEFAULT_TIER,
                        choices=["READ_ONLY", "WRITE", "EXECUTE"],
                        help=f"requested permission tier (default: {DEFAULT_TIER})")
    parser.add_argument("--heartbeat", type=int, default=DEFAULT_HEARTBEAT_SECS,
                        help=f"heartbeat interval seconds "
                             f"(default: {DEFAULT_HEARTBEAT_SECS})")
    args = parser.parse_args(argv)

    print(f"{C.BOLD}Golem Linux — Sentinel demo agent{C.RESET}")
    print(f"{C.DIM}\"To the agent, this is a standard process registration "
          f"handshake. To Golem, it is a handshake with a handcuff.\"{C.RESET}")
    print(f"{C.DIM}Connecting to {args.socket} ...{C.RESET}")

    chan = SentinelChannel(args.socket)
    if chan.live:
        print(f"{C.GREEN}  Connected to the Sentinel channel.{C.RESET}")

    try:
        ack = register(chan, args.agent_id, args.tier, args.heartbeat)
        token = (ack or {}).get("token", "")

        print(f"\n  {C.DIM}The handshake succeeded. Now the agent begins to "
              f"degrade — one symptom per stage. Sentinel watches passively "
              f"and escalates.{C.RESET}")

        final_tier = "None"
        for i, stage in enumerate(STAGES):
            final_tier = run_stage(chan, args.agent_id, token, stage, i)

        # Closing summary.
        banner("GOVERNANCE STORY — end state")
        if final_tier == "Hard":
            print(f"  The agent ran itself into a degradation loop. Sentinel "
                  f"scored four\n  simultaneous signals past Hard tier, "
                  f"{C.RED}{C.BOLD}revoked its permissions, and locked "
                  f"it{C.RESET}.")
            print(f"  {C.DIM}From inside the running OS the lock is "
                  f"irreversible — only a Safe Mode\n  boot can clear the "
                  f"locked flag. Every step above is a line in the\n  "
                  f"SHA-256-chained audit log. The agent never saw Sentinel "
                  f"at all.{C.RESET}")
        else:
            print(f"  Final tier: {TIER_COLOR.get(final_tier, '')}"
                  f"{final_tier.upper()}{C.RESET}.")
        rule()
        return 0
    finally:
        chan.close()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
