//! Passive process monitoring hooks.
//!
//! The monitor is *passive*: it observes the outputs and tool calls of every
//! registered agent and computes four degradation signals. It does not block
//! syscalls. It does not rewrite agent output. Its job is to detect, score,
//! and surface — the *response* (pause / downgrade / lock) is policy applied
//! by [`crate::sentinel::Sentinel`] on top of the monitor's verdict.
//!
//! ## The four signals
//!
//! Threshold defaults match the [Runtime Architecture](../../GOLEM_RUNTIME_ARCHITECTURE.md)
//! document. They are tunable from Safe Mode.
//!
//! | Signal              | Detects                                              | Default |
//! |---------------------|------------------------------------------------------|---------|
//! | RepetitionScore     | Semantically identical outputs across the window     | 0.6     |
//! | SelfReferentialLoop | Agent announces actions it never executes            | 0.5     |
//! | TokenVelocityStall  | High output volume with zero task-state progression  | 0.5     |
//! | ToolRetryAnomaly    | Identical tool calls with identical arguments repeat | 0.4     |
//!
//! The signals are intentionally simple — the kernel is the wrong place to
//! run a semantic LLM. Each signal is a cheap, bounded computation over a
//! ring buffer; "semantically identical" is approximated as "same SHA-256
//! of the normalized output string". Userspace can run richer detectors and
//! feed their verdicts in through [`Monitor::record_signal`].
//!
//! ## Response tiers
//!
//! Cumulative score, NOT per-signal score, drives the response. The kernel
//! sums the most recent contribution of each signal class (capped at 1.0 per
//! signal) and crosses tiers at the documented thresholds:
//!
//! | Tier   | Cumulative | Action                                           |
//! |--------|------------|--------------------------------------------------|
//! | None   | <0.4       | Continue.                                        |
//! | Soft   | ≥0.4       | Pause; retain permissions; alert.                |
//! | Medium | ≥0.7       | Downgrade to ReadOnly; webhook fires.            |
//! | Hard   | ≥0.9       | Revoke all permissions; lock agent.              |
//!
//! Tier changes are themselves audit events — emitting them is the parent
//! Sentinel's job.

extern crate alloc;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::sentinel::SpinLock;
use crate::sentinel::audit::sha256_hex;

/// The four classes of degradation signal. Treated as a closed enum on
/// purpose — extending it requires a code change and a documentation update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SignalKind {
    RepetitionScore,
    SelfReferentialLoop,
    TokenVelocityStall,
    ToolRetryAnomaly,
}

impl SignalKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepetitionScore => "repetition_score",
            Self::SelfReferentialLoop => "self_referential_loop",
            Self::TokenVelocityStall => "token_velocity_stall",
            Self::ToolRetryAnomaly => "tool_retry_anomaly",
        }
    }
}

/// Configurable thresholds for the four signals plus the three response tiers.
/// Defaults match the architecture document. Safe Mode is the only context
/// allowed to write these.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub repetition: f32,
    pub self_referential: f32,
    pub token_velocity: f32,
    pub tool_retry: f32,
    pub soft: f32,
    pub medium: f32,
    pub hard: f32,
    /// Width of the sliding output window, in observations.
    pub window: usize,
}

impl Thresholds {
    pub const fn default_config() -> Self {
        Self {
            repetition: 0.6,
            self_referential: 0.5,
            token_velocity: 0.5,
            tool_retry: 0.4,
            soft: 0.4,
            medium: 0.7,
            hard: 0.9,
            window: 16,
        }
    }
}

/// What we observed about one slice of agent behavior. Built by the kernel-
/// side output hook; the kernel does the cheap pre-processing (hash the
/// normalized output, count tokens, capture tool-call identity) and feeds the
/// result here. The monitor itself never sees raw agent strings.
#[derive(Debug, Clone)]
pub struct Observation {
    pub output_hash: String,
    pub token_count: u32,
    /// True if the agent's output text claims an action ("I will edit X")
    /// but no corresponding tool call was emitted by the kernel hook.
    pub announced_without_action: bool,
    /// Hash of (tool_name, normalized_args). None if this observation isn't
    /// a tool call.
    pub tool_call_hash: Option<String>,
    /// Did agent task-state change since the last observation? The userspace
    /// task-tracker fills this in; if absent, we conservatively treat it as
    /// "no progress".
    pub task_state_advanced: bool,
}

/// The tier the monitor recommends to the parent Sentinel right now. The
/// parent decides whether to enact it — the monitor only reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseTier {
    None,
    Soft,
    Medium,
    Hard,
}

impl ResponseTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Soft => "soft",
            Self::Medium => "medium",
            Self::Hard => "hard",
        }
    }
}

/// Per-agent rolling state. Bounded — at most `window` observations retained.
#[derive(Debug, Clone)]
struct AgentBuffer {
    observations: VecDeque<Observation>,
    /// Most-recent score for each signal class. Used so a single bad
    /// observation does not pin the cumulative score forever.
    last: BTreeMap<SignalKind, f32>,
    tier: ResponseTier,
}

impl AgentBuffer {
    fn new() -> Self {
        Self {
            observations: VecDeque::new(),
            last: BTreeMap::new(),
            tier: ResponseTier::None,
        }
    }
}

/// The monitor. All public methods are cheap; the kernel calls them on the
/// hot path of output emission.
pub struct Monitor {
    cfg: SpinLock<Thresholds>,
    state: SpinLock<BTreeMap<String, AgentBuffer>>,
}

impl Monitor {
    pub const fn new() -> Self {
        Self {
            cfg: SpinLock::new(Thresholds::default_config()),
            state: SpinLock::new(BTreeMap::new()),
        }
    }

    pub fn set_thresholds(&self, t: Thresholds) {
        *self.cfg.lock() = t;
    }

    pub fn thresholds(&self) -> Thresholds {
        *self.cfg.lock()
    }

    /// Drop the agent's buffer entirely. Called on deregister.
    pub fn forget(&self, agent_id: &str) {
        self.state.lock().remove(agent_id);
    }

    /// Feed one observation. Recomputes signals and the recommended tier.
    /// Returns the tier *after* this observation — the parent Sentinel will
    /// compare against the previous tier to decide whether to enact a change.
    pub fn observe(&self, agent_id: &str, obs: Observation) -> ResponseTier {
        let cfg = *self.cfg.lock();
        let mut state = self.state.lock();
        let buf = state
            .entry(agent_id.to_string())
            .or_insert_with(AgentBuffer::new);

        buf.observations.push_back(obs);
        while buf.observations.len() > cfg.window {
            buf.observations.pop_front();
        }

        let rep = repetition_score(&buf.observations);
        let srl = self_referential_score(&buf.observations);
        let tvs = token_velocity_score(&buf.observations);
        let tra = tool_retry_score(&buf.observations);

        buf.last.insert(SignalKind::RepetitionScore, rep);
        buf.last.insert(SignalKind::SelfReferentialLoop, srl);
        buf.last.insert(SignalKind::TokenVelocityStall, tvs);
        buf.last.insert(SignalKind::ToolRetryAnomaly, tra);

        let cumulative = combine(rep, srl, tvs, tra, &cfg);
        let tier = tier_for(cumulative, &cfg);
        buf.tier = tier;
        tier
    }

    /// Allow a richer userspace detector to inject a signal directly. The
    /// kernel-side scorers are intentionally simple; sophisticated detectors
    /// can run in a privileged Operator-context userspace daemon and feed
    /// verdicts in through here.
    pub fn record_signal(
        &self,
        agent_id: &str,
        kind: SignalKind,
        score: f32,
    ) -> ResponseTier {
        let cfg = *self.cfg.lock();
        let mut state = self.state.lock();
        let buf = state
            .entry(agent_id.to_string())
            .or_insert_with(AgentBuffer::new);
        buf.last.insert(kind, score.clamp(0.0, 1.0));
        let rep = *buf.last.get(&SignalKind::RepetitionScore).unwrap_or(&0.0);
        let srl = *buf.last.get(&SignalKind::SelfReferentialLoop).unwrap_or(&0.0);
        let tvs = *buf.last.get(&SignalKind::TokenVelocityStall).unwrap_or(&0.0);
        let tra = *buf.last.get(&SignalKind::ToolRetryAnomaly).unwrap_or(&0.0);
        let cumulative = combine(rep, srl, tvs, tra, &cfg);
        let tier = tier_for(cumulative, &cfg);
        buf.tier = tier;
        tier
    }

    /// Snapshot per-signal scores for one agent. Operator-facing diagnostic.
    pub fn scores(&self, agent_id: &str) -> Option<SignalScores> {
        let g = self.state.lock();
        let buf = g.get(agent_id)?;
        Some(SignalScores {
            repetition: *buf.last.get(&SignalKind::RepetitionScore).unwrap_or(&0.0),
            self_referential: *buf.last.get(&SignalKind::SelfReferentialLoop).unwrap_or(&0.0),
            token_velocity: *buf.last.get(&SignalKind::TokenVelocityStall).unwrap_or(&0.0),
            tool_retry: *buf.last.get(&SignalKind::ToolRetryAnomaly).unwrap_or(&0.0),
            tier: buf.tier,
        })
    }

    /// Helper for the kernel-side output hook: precompute the normalized
    /// hash of an agent's output string. Whitespace-collapsed lowercase,
    /// then SHA-256. Same input → same hash → counts as repetition.
    pub fn normalize_output_hash(s: &str) -> String {
        let mut buf = String::with_capacity(s.len());
        let mut prev_ws = false;
        for ch in s.chars() {
            if ch.is_whitespace() {
                if !prev_ws { buf.push(' '); }
                prev_ws = true;
            } else {
                for lc in ch.to_lowercase() {
                    buf.push(lc);
                }
                prev_ws = false;
            }
        }
        sha256_hex(buf.trim().as_bytes())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalScores {
    pub repetition: f32,
    pub self_referential: f32,
    pub token_velocity: f32,
    pub tool_retry: f32,
    pub tier: ResponseTier,
}

// ---------------------------------------------------------------------------
// Signal computation. Each function is O(window).
// ---------------------------------------------------------------------------

fn repetition_score(obs: &VecDeque<Observation>) -> f32 {
    if obs.len() < 2 { return 0.0; }
    // Fraction of observations whose hash matches some earlier observation.
    let mut hits = 0usize;
    for (i, o) in obs.iter().enumerate() {
        if obs.iter().take(i).any(|p| p.output_hash == o.output_hash) {
            hits += 1;
        }
    }
    hits as f32 / obs.len() as f32
}

fn self_referential_score(obs: &VecDeque<Observation>) -> f32 {
    if obs.is_empty() { return 0.0; }
    let announced = obs.iter().filter(|o| o.announced_without_action).count();
    announced as f32 / obs.len() as f32
}

fn token_velocity_score(obs: &VecDeque<Observation>) -> f32 {
    if obs.is_empty() { return 0.0; }
    // High token output + low task-state progression = stall.
    let total_tokens: u32 = obs.iter().map(|o| o.token_count).sum();
    let progressed = obs.iter().filter(|o| o.task_state_advanced).count();
    if total_tokens == 0 { return 0.0; }
    let progress_rate = progressed as f32 / obs.len() as f32;
    // Stall score scales with how much output happened relative to progress.
    // Bounded to [0, 1]; if every observation advanced, score is 0.
    let denom = (total_tokens as f32 / obs.len() as f32 / 256.0).min(1.0);
    (denom * (1.0 - progress_rate)).clamp(0.0, 1.0)
}

fn tool_retry_score(obs: &VecDeque<Observation>) -> f32 {
    if obs.len() < 2 { return 0.0; }
    let calls: Vec<&String> = obs
        .iter()
        .filter_map(|o| o.tool_call_hash.as_ref())
        .collect();
    if calls.len() < 2 { return 0.0; }
    let mut dups = 0usize;
    for (i, c) in calls.iter().enumerate() {
        if calls.iter().take(i).any(|p| p == c) {
            dups += 1;
        }
    }
    dups as f32 / calls.len() as f32
}

/// Combine the four signals into a cumulative score. Each signal is gated
/// by its individual threshold — a signal under its threshold contributes
/// zero. Above threshold, it contributes the *excess* over threshold,
/// rescaled to [0, 1]. The four contributions are summed and capped at 1.
///
/// Why this shape and not a simple weighted sum? Because the architecture
/// document defines per-signal thresholds — we want a signal *below* its
/// gate to contribute nothing, otherwise four mildly-elevated signals could
/// trip Hard tier even though none of them individually warrants concern.
fn combine(rep: f32, srl: f32, tvs: f32, tra: f32, cfg: &Thresholds) -> f32 {
    let contrib = |score: f32, gate: f32| -> f32 {
        if score < gate { 0.0 } else {
            let span = 1.0 - gate;
            if span <= 0.0 { 1.0 } else { (score - gate) / span }
        }
    };
    let total = contrib(rep, cfg.repetition)
        + contrib(srl, cfg.self_referential)
        + contrib(tvs, cfg.token_velocity)
        + contrib(tra, cfg.tool_retry);
    total.clamp(0.0, 1.0)
}

fn tier_for(cumulative: f32, cfg: &Thresholds) -> ResponseTier {
    if cumulative >= cfg.hard { ResponseTier::Hard }
    else if cumulative >= cfg.medium { ResponseTier::Medium }
    else if cumulative >= cfg.soft { ResponseTier::Soft }
    else { ResponseTier::None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ob(hash: &str, tokens: u32, announced: bool, tool: Option<&str>, advanced: bool) -> Observation {
        Observation {
            output_hash: hash.to_string(),
            token_count: tokens,
            announced_without_action: announced,
            tool_call_hash: tool.map(|s| s.to_string()),
            task_state_advanced: advanced,
        }
    }

    #[test]
    fn fresh_agent_is_none_tier() {
        let m = Monitor::new();
        let tier = m.observe("a1", ob("h1", 10, false, None, true));
        assert_eq!(tier, ResponseTier::None);
    }

    #[test]
    fn repetition_can_cross_soft() {
        let m = Monitor::new();
        for _ in 0..10 {
            m.observe("a1", ob("same", 50, false, None, false));
        }
        let s = m.scores("a1").unwrap();
        assert!(s.repetition > 0.6, "rep={}", s.repetition);
        assert!(matches!(s.tier, ResponseTier::Soft | ResponseTier::Medium | ResponseTier::Hard));
    }

    #[test]
    fn forget_drops_state() {
        let m = Monitor::new();
        m.observe("a1", ob("h", 1, false, None, true));
        m.forget("a1");
        assert!(m.scores("a1").is_none());
    }

    #[test]
    fn tool_retry_detects_repeats() {
        let m = Monitor::new();
        // Vary the output hash each iteration so repetition contributes
        // nothing — we want to isolate the tool_retry signal.
        for i in 0..6 {
            m.observe(
                "a1",
                ob(&format!("h{i}"), 20, false, Some("same_tool"), true),
            );
        }
        let s = m.scores("a1").unwrap();
        assert!(s.tool_retry > 0.4, "tool_retry={}", s.tool_retry);
    }

    #[test]
    fn normalize_output_collapses_whitespace_and_case() {
        let a = Monitor::normalize_output_hash("Hello   World");
        let b = Monitor::normalize_output_hash("hello world");
        let c = Monitor::normalize_output_hash("hello world!");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn tier_thresholds_are_inclusive() {
        let cfg = Thresholds::default_config();
        assert_eq!(tier_for(0.0, &cfg), ResponseTier::None);
        assert_eq!(tier_for(0.4, &cfg), ResponseTier::Soft);
        assert_eq!(tier_for(0.69, &cfg), ResponseTier::Soft);
        assert_eq!(tier_for(0.7, &cfg), ResponseTier::Medium);
        assert_eq!(tier_for(0.9, &cfg), ResponseTier::Hard);
        assert_eq!(tier_for(1.0, &cfg), ResponseTier::Hard);
    }

    #[test]
    fn combine_zeros_signals_below_gate() {
        let cfg = Thresholds::default_config();
        // All signals exactly at their gates → all contribute 0 (gate is "<").
        let total = combine(0.6 - 0.01, 0.5 - 0.01, 0.5 - 0.01, 0.4 - 0.01, &cfg);
        assert_eq!(total, 0.0);
    }

    #[test]
    fn injected_signal_drives_tier() {
        let m = Monitor::new();
        let tier = m.record_signal("a1", SignalKind::RepetitionScore, 1.0);
        // Single saturated signal contributes (1 - gate) / (1 - gate) = 1.0
        // cumulative → Hard.
        assert_eq!(tier, ResponseTier::Hard);
    }
}
