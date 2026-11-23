//! Host-side video flow control + adaptive bitrate (ABR).
//!
//! The viewer acknowledges every frame once it has been *decoded and converted*
//! (`ControlMsg::FrameAck`, cumulative, sent on the reverse direction of the video channel).
//! From those acks the host learns how long a frame takes from hand-off to "ready on the viewer"
//! and how fast bytes are really being delivered. Two mechanisms use that:
//!
//! 1. **Latency gate** - at most `window` frames may be unacknowledged. The window follows the
//!    measured queueing delay with AIMD (shrink by 15 % per round trip while frames queue for more
//!    than `QUEUE_BUDGET`, grow by ~1 frame per round trip while the window is actually limiting
//!    and queueing is low), so it adapts to whichever is the bottleneck: the relay link or the
//!    viewer's decoder. When the window is full the host *skips capturing* instead of piling
//!    frames into socket / relay buffers (which is how a 5 Mbps relay used to accumulate seconds
//!    of lag). A skipped tick never breaks the reference chain, so no keyframe is needed
//!    afterwards. As a backstop the oldest frame may never age beyond `base + 2 * QUEUE_BUDGET`.
//! 2. **ABR** - delay-based AIMD on the encoder bitrate, bounded by the user's maximum. Network
//!    queueing (ack delay minus the viewer's own decode time, minus the path's base delay) above
//!    `QUEUE_HIGH_MS` cuts the target to <= 85 % of the current target and <= 90 % of the measured
//!    delivery rate; low queueing while the encoder actually uses its budget grows it again.
//!
//! Peers that never ack (older viewers) keep the legacy behaviour: gate open, fixed bitrate.

use protocol::session::HostStreamStats;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Queueing delay (send -> decoded ack, above the measured base delay) the in-flight window is
/// steered to stay under. Bounds the lag queueing can add anywhere on the path.
pub const QUEUE_BUDGET: Duration = Duration::from_millis(100);
const QUEUE_BUDGET_MS: f64 = 100.0;
/// In-flight frame window (AIMD). 10 frames sustain 60 fps up to ~160 ms of base delay.
const INITIAL_WINDOW: f64 = 10.0;
const MIN_WINDOW: f64 = 1.0;
const MAX_WINDOW: f64 = 48.0;
const WINDOW_DECREASE: f64 = 0.85;
/// The window only grows if it was the limiting factor this recently (cwnd validation): an idle
/// desktop must not inflate it, or the next burst would overshoot by the inflated amount.
const WINDOW_LIMITED_RECENTLY: Duration = Duration::from_millis(500);
/// Base-delay probe ("ProbeRTT"): under *persistent* congestion every delay sample contains
/// queueing, the min-filter baseline drifts up and the queue becomes invisible. So while the
/// window is limiting, every `PROBE_INTERVAL` the pipe is drained (no new frames until all are
/// acknowledged) and the next frame, sent into an empty pipe, refreshes the baseline. Costs one
/// round trip of skipped ticks, and only when already congested.
const FIRST_PROBE_AFTER: Duration = Duration::from_secs(2);
const PROBE_INTERVAL: Duration = Duration::from_secs(8);
/// Smoothed network queueing delay above which the bitrate is cut.
const QUEUE_HIGH_MS: f64 = 45.0;
/// Smoothed network queueing delay below which the bitrate may grow.
const QUEUE_LOW_MS: f64 = 15.0;
const DECREASE_FACTOR: f64 = 0.85;
/// When congested, the new target stays this fraction below the measured delivery rate.
const RATE_HEADROOM: f64 = 0.90;
/// Encoder output / target above which the stream counts as link-limited (not content-limited).
const LINK_LIMITED_UTIL: f64 = 0.8;
const INCREASE_INTERVAL: Duration = Duration::from_millis(250);
const INCREASE_STEP_FRAC: f64 = 0.05;
const INCREASE_STEP_MIN_KBPS: f64 = 40.0;
/// Hold-off after a decrease before growing again.
const INCREASE_HOLDOFF: Duration = Duration::from_secs(1);
/// First ABR target once acks arrive (never above the user's maximum).
const START_KBPS: u32 = 6000;
pub const MIN_KBPS: u32 = 300;
/// Window for the base (uncongested) delay estimate.
const BASE_WINDOW: Duration = Duration::from_secs(10);
/// Window for the delivery-rate estimate.
const RATE_WINDOW: Duration = Duration::from_millis(500);
/// Window for the "is the encoder using its budget" check.
const UTIL_WINDOW: Duration = Duration::from_secs(1);
/// If the gate stays closed this long without any ack, assume acks are lost and probe.
const STALL_RESET: Duration = Duration::from_secs(2);
const MAX_TRACKED: usize = 512;

/// Sliding-window minimum (monotonic deque).
#[derive(Debug)]
struct WindowedMin {
    window: Duration,
    q: VecDeque<(Instant, f64)>,
}

impl WindowedMin {
    fn new(window: Duration) -> Self {
        Self {
            window,
            q: VecDeque::new(),
        }
    }

    fn push(&mut self, now: Instant, v: f64) {
        while matches!(self.q.back(), Some(&(_, back)) if back >= v) {
            self.q.pop_back();
        }
        self.q.push_back((now, v));
        while self.q.len() > 1
            && matches!(self.q.front(), Some(&(t, _)) if now.duration_since(t) > self.window)
        {
            self.q.pop_front();
        }
    }

    fn get(&self) -> Option<f64> {
        self.q.front().map(|&(_, v)| v)
    }
}

#[derive(Debug, Clone, Copy)]
struct InFlight {
    id: u64,
    bytes: usize,
    sent_at: Instant,
    keyframe: bool,
}

#[derive(Debug)]
pub struct FlowController {
    max_kbps: u32,
    target_kbps: f64,
    acks_seen: bool,
    inflight: VecDeque<InFlight>,
    window: f64,
    window_limited_at: Option<Instant>,
    last_window_cut: Option<Instant>,
    /// Draining for a base-delay probe.
    probing: bool,
    /// The next sent frame goes into an empty pipe (its ack is a clean base sample).
    probe_send_pending: bool,
    probe_frame: Option<u64>,
    next_probe_at: Instant,
    base_total: WindowedMin,
    base_net: WindowedMin,
    queue_ewma: Option<f64>,
    /// Delay samples of frames up to this id are ignored for ABR (they queued behind a keyframe).
    suppress_until_id: u64,
    acked_total: u64,
    acked: VecDeque<(Instant, u64)>,
    sent: VecDeque<(Instant, usize)>,
    last_ack_at: Option<Instant>,
    last_decrease: Option<Instant>,
    last_increase: Instant,
    gate_closed_since: Option<Instant>,
    stat_since: Instant,
    stat_frames: u32,
    stat_bytes: u64,
    stat_delay_sum: f64,
    stat_delay_n: u32,
    stat_delay_max: f64,
    stat_paused: u32,
}

impl FlowController {
    pub fn new(max_kbps: u32, now: Instant) -> Self {
        let max_kbps = max_kbps.max(MIN_KBPS);
        Self {
            max_kbps,
            target_kbps: max_kbps.min(START_KBPS) as f64,
            acks_seen: false,
            inflight: VecDeque::new(),
            window: INITIAL_WINDOW,
            window_limited_at: None,
            last_window_cut: None,
            probing: false,
            probe_send_pending: false,
            probe_frame: None,
            next_probe_at: now + FIRST_PROBE_AFTER,
            base_total: WindowedMin::new(BASE_WINDOW),
            base_net: WindowedMin::new(BASE_WINDOW),
            queue_ewma: None,
            suppress_until_id: 0,
            acked_total: 0,
            acked: VecDeque::new(),
            sent: VecDeque::new(),
            last_ack_at: None,
            last_decrease: None,
            last_increase: now,
            gate_closed_since: None,
            stat_since: now,
            stat_frames: 0,
            stat_bytes: 0,
            stat_delay_sum: 0.0,
            stat_delay_n: 0,
            stat_delay_max: 0.0,
            stat_paused: 0,
        }
    }

    /// The user's maximum bitrate (toolbar slider). The ABR never exceeds it.
    pub fn set_max_kbps(&mut self, max_kbps: u32) {
        self.max_kbps = max_kbps.max(MIN_KBPS);
        self.target_kbps = self.target_kbps.min(self.max_kbps as f64);
    }

    /// Bitrate the encoder should use right now.
    pub fn target_kbps(&self) -> u32 {
        if self.acks_seen {
            self.target_kbps.round() as u32
        } else {
            self.max_kbps
        }
    }

    /// Whether a new frame may be captured/encoded now. Counts refused ticks for the stats.
    pub fn can_send(&mut self, now: Instant) -> bool {
        if !self.acks_seen {
            return true;
        }
        let Some(oldest) = self.inflight.front() else {
            if self.probing {
                self.probing = false;
                self.probe_send_pending = true;
            }
            self.gate_closed_since = None;
            return true;
        };
        let age = now.duration_since(oldest.sent_at);
        let window_full = self.inflight.len() as f64 >= self.window.floor().max(MIN_WINDOW);
        if window_full {
            self.window_limited_at = Some(now);
        }
        if !self.probing
            && now >= self.next_probe_at
            && self
                .window_limited_at
                .is_some_and(|t| now.duration_since(t) <= WINDOW_LIMITED_RECENTLY)
        {
            self.probing = true;
            self.next_probe_at = now + PROBE_INTERVAL;
        }
        if !self.probing && !window_full && age <= self.age_limit() {
            self.gate_closed_since = None;
            return true;
        }
        let closed_since = *self.gate_closed_since.get_or_insert(now);
        let no_ack_for = self.last_ack_at.map_or(age, |t| now.duration_since(t));
        if no_ack_for >= STALL_RESET && now.duration_since(closed_since) >= STALL_RESET {
            // Acks stopped (lost behind a stalled control path, viewer frozen, ...). Never wedge
            // the stream: forget the window, back off, and let one probe frame through.
            tracing::warn!(
                "flow control: no frame ack for {} ms; resetting in-flight window",
                no_ack_for.as_millis()
            );
            self.inflight.clear();
            self.target_kbps = (self.target_kbps * 0.5).max(MIN_KBPS as f64);
            self.window = (self.window * 0.5).max(MIN_WINDOW);
            self.gate_closed_since = None;
            self.last_ack_at = Some(now);
            return true;
        }
        self.stat_paused += 1;
        false
    }

    /// Backstop for single huge frames: the oldest frame may not age beyond base + 2x budget.
    fn age_limit(&self) -> Duration {
        let base_ms = self.base_total.get().unwrap_or(0.0);
        Duration::from_secs_f64(base_ms / 1000.0) + QUEUE_BUDGET * 2
    }

    /// AIMD on the in-flight window from one exact delay sample (send -> decoded ack).
    fn update_window(&mut self, total_ms: f64, now: Instant) {
        let base_ms = self.base_total.get().unwrap_or(total_ms);
        let queue_ms = total_ms - base_ms;
        if queue_ms > QUEUE_BUDGET_MS {
            // At most one cut per round trip: the acks of one congestion episode arrive in a burst.
            let rtt = Duration::from_secs_f64(base_ms.max(30.0) / 1000.0);
            if self.last_window_cut.is_none_or(|t| now.duration_since(t) >= rtt) {
                self.window = (self.window * WINDOW_DECREASE).max(MIN_WINDOW);
                self.last_window_cut = Some(now);
            }
        } else if queue_ms < QUEUE_BUDGET_MS * 0.5
            && self
                .window_limited_at
                .is_some_and(|t| now.duration_since(t) <= WINDOW_LIMITED_RECENTLY)
        {
            // ~ +1 frame per window's worth of acks, i.e. per round trip when window-limited.
            self.window = (self.window + 1.0 / self.window.max(1.0)).min(MAX_WINDOW);
        }
    }

    /// Records a frame handed to the network writer.
    pub fn on_frame_sent(&mut self, frame_id: u64, bytes: usize, keyframe: bool, now: Instant) {
        self.inflight.push_back(InFlight {
            id: frame_id,
            bytes,
            sent_at: now,
            keyframe,
        });
        if self.inflight.len() > MAX_TRACKED {
            self.inflight.pop_front();
        }
        if self.inflight.len() as f64 >= self.window.floor() {
            self.window_limited_at = Some(now);
        }
        if std::mem::take(&mut self.probe_send_pending) {
            self.probe_frame = Some(frame_id);
        }
        self.sent.push_back((now, bytes));
        while matches!(self.sent.front(), Some(&(t, _)) if now.duration_since(t) > UTIL_WINDOW) {
            self.sent.pop_front();
        }
        self.stat_frames += 1;
        self.stat_bytes += bytes as u64;
    }

    /// Cumulative ack: every frame with id `<= frame_id` is decoded on the viewer. `viewer_us` is
    /// how long the acknowledged frame spent on the viewer (receipt -> decoded).
    pub fn on_ack(&mut self, frame_id: u64, viewer_us: u32, now: Instant) {
        self.acks_seen = true;
        self.last_ack_at = Some(now);

        let mut exact: Option<InFlight> = None;
        while let Some(f) = self.inflight.front().copied() {
            if f.id > frame_id {
                break;
            }
            self.inflight.pop_front();
            self.acked_total += f.bytes as u64;
            if f.keyframe {
                // Everything still in flight queued behind this (large) keyframe.
                let newest = self.inflight.back().map_or(frame_id, |b| b.id);
                self.suppress_until_id = self.suppress_until_id.max(newest.max(frame_id));
            }
            if f.id == frame_id {
                exact = Some(f);
            }
        }
        self.acked.push_back((now, self.acked_total));
        while self.acked.len() > 2
            && matches!(self.acked.front(), Some(&(t, _)) if now.duration_since(t) > RATE_WINDOW)
        {
            self.acked.pop_front();
        }

        // Only the acknowledged frame itself has an exact viewer-side time.
        let Some(f) = exact else {
            return;
        };
        let total_ms = now.duration_since(f.sent_at).as_secs_f64() * 1000.0;
        let net_ms = (total_ms - viewer_us as f64 / 1000.0).max(0.0);
        let delayed_by_keyframe = f.keyframe || f.id <= self.suppress_until_id;
        if self.probe_frame == Some(f.id) {
            // Sent into an empty pipe: authoritative baseline, replaces any drifted-up history.
            self.probe_frame = None;
            self.base_total = WindowedMin::new(BASE_WINDOW);
            self.base_net = WindowedMin::new(BASE_WINDOW);
            self.base_total.push(now, total_ms);
            self.base_net.push(now, net_ms);
        } else if !delayed_by_keyframe {
            self.base_total.push(now, total_ms);
            self.base_net.push(now, net_ms);
        }
        self.stat_delay_sum += total_ms;
        self.stat_delay_n += 1;
        self.stat_delay_max = self.stat_delay_max.max(total_ms);

        if !f.keyframe {
            self.update_window(total_ms, now);
        }
        if f.keyframe || f.id <= self.suppress_until_id {
            return;
        }
        let queue_ms = (net_ms - self.base_net.get().unwrap_or(net_ms)).max(0.0);
        self.queue_ewma = Some(match self.queue_ewma {
            Some(prev) => 0.8 * prev + 0.2 * queue_ms,
            None => queue_ms,
        });
        self.adjust(now);
    }

    fn adjust(&mut self, now: Instant) {
        let Some(queue_ms) = self.queue_ewma else {
            return;
        };
        let max = self.max_kbps as f64;
        if queue_ms > QUEUE_HIGH_MS {
            let base_net = self.base_net.get().unwrap_or(0.0);
            let gap = Duration::from_secs_f64((base_net * 2.0).max(200.0) / 1000.0);
            if self.last_decrease.is_none_or(|t| now.duration_since(t) >= gap) {
                let mut next = self.target_kbps * DECREASE_FACTOR;
                if let Some(rate) = self.delivery_rate_kbps() {
                    // The delivery rate only measures the link when the encoder is actually
                    // using its budget. With light content (0.6 Mbps of 4 Mbps) it measures the
                    // content, and a transient delay spike would otherwise slash the target to
                    // the content's rate.
                    if rate < self.target_kbps && self.utilization(now) >= LINK_LIMITED_UTIL {
                        next = next.min(rate * RATE_HEADROOM);
                    }
                    // A compliant encoder drains the queue after one cut to ~90 % of the
                    // delivery rate. If queueing persists the encoder is overshooting its target
                    // and cutting further can't help (the latency gate bounds lag regardless),
                    // so never collapse far below what the link demonstrably delivers.
                    next = next.max(rate * 0.5);
                }
                self.target_kbps = next.clamp(MIN_KBPS as f64, max);
                self.last_decrease = Some(now);
                self.last_increase = now;
            }
        } else if queue_ms < QUEUE_LOW_MS
            && self.target_kbps < max
            && self
                .last_decrease
                .is_none_or(|t| now.duration_since(t) >= INCREASE_HOLDOFF)
            && now.duration_since(self.last_increase) >= INCREASE_INTERVAL
        {
            // Only probe upwards while the encoder actually uses its budget; a static desktop
            // must not inflate the target (the next burst would then overshoot the link).
            if self.utilization(now) >= 0.5 {
                let step = (self.target_kbps * INCREASE_STEP_FRAC).max(INCREASE_STEP_MIN_KBPS);
                self.target_kbps = (self.target_kbps + step).min(max);
            }
            self.last_increase = now;
        }
    }

    /// Bytes acknowledged per second over the last `RATE_WINDOW`, in kbps.
    fn delivery_rate_kbps(&self) -> Option<f64> {
        let (&(t0, b0), &(t1, b1)) = (self.acked.front()?, self.acked.back()?);
        let secs = t1.duration_since(t0).as_secs_f64();
        if secs < 0.1 || b1 <= b0 {
            return None;
        }
        Some((b1 - b0) as f64 * 8.0 / 1000.0 / secs)
    }

    fn utilization(&self, now: Instant) -> f64 {
        let bytes: usize = self
            .sent
            .iter()
            .filter(|(t, _)| now.duration_since(*t) <= UTIL_WINDOW)
            .map(|&(_, b)| b)
            .sum();
        let kbps = bytes as f64 * 8.0 / 1000.0 / UTIL_WINDOW.as_secs_f64();
        kbps / self.target_kbps.max(1.0)
    }

    /// Returns and resets the per-window statistics (stream size fields are left at 0).
    pub fn take_stats(&mut self, now: Instant) -> HostStreamStats {
        let secs = now.duration_since(self.stat_since).as_secs_f64().max(0.001);
        let stats = HostStreamStats {
            target_kbps: self.target_kbps(),
            sent_kbps: (self.stat_bytes as f64 * 8.0 / 1000.0 / secs).round() as u32,
            sent_fps: (self.stat_frames as f64 / secs) as f32,
            avg_delay_ms: if self.stat_delay_n > 0 {
                (self.stat_delay_sum / self.stat_delay_n as f64).round() as u32
            } else {
                0
            },
            max_delay_ms: self.stat_delay_max.round() as u32,
            paused_ticks: self.stat_paused,
            flow_control: self.acks_seen,
            // Filled in by the capture loop, which owns these measurements.
            ..Default::default()
        };
        self.stat_since = now;
        self.stat_frames = 0;
        self.stat_bytes = 0;
        self.stat_delay_sum = 0.0;
        self.stat_delay_n = 0;
        self.stat_delay_max = 0.0;
        self.stat_paused = 0;
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_micros(16_667);

    /// Single FIFO bottleneck (like the relay's egress) with propagation delay each way, and the
    /// viewer decoder modelled as a serial server. Time is virtual: no sleeping.
    struct Sim {
        t0: Instant,
        tick: u64,
        flow: FlowController,
        link_bps: f64,
        one_way: Duration,
        link_free_at: Instant,
        decode_time: Duration,
        decoder_free_at: Instant,
        acks: VecDeque<(Instant, u64, u32)>,
        acks_enabled: bool,
        next_id: u64,
        /// (sent_at, decoded_at, bytes), relative to t0.
        log: Vec<(Duration, Duration, usize)>,
        gate_refusals: u32,
    }

    impl Sim {
        fn new(max_kbps: u32, link_mbps: f64, one_way_ms: u64, decode_ms: u64) -> Self {
            let t0 = Instant::now();
            Self {
                t0,
                tick: 0,
                flow: FlowController::new(max_kbps, t0),
                link_bps: link_mbps * 1_000_000.0 / 8.0,
                one_way: Duration::from_millis(one_way_ms),
                link_free_at: t0,
                decode_time: Duration::from_millis(decode_ms),
                decoder_free_at: t0,
                acks: VecDeque::new(),
                acks_enabled: true,
                next_id: 0,
                log: Vec::new(),
                gate_refusals: 0,
            }
        }

        /// Runs `seconds` of 60 Hz capture ticks. `encoder(target_kbps, id)` returns
        /// `(frame bytes, is_keyframe)`.
        fn run(&mut self, seconds: f64, encoder: &dyn Fn(u32, u64) -> (usize, bool)) {
            let ticks = (seconds / TICK.as_secs_f64()).round() as u64;
            for _ in 0..ticks {
                let now = self.t0 + TICK * self.tick as u32;
                self.tick += 1;
                while let Some(&(at, id, viewer_us)) = self.acks.front() {
                    if at > now {
                        break;
                    }
                    self.acks.pop_front();
                    self.flow.on_ack(id, viewer_us, at);
                }
                if !self.flow.can_send(now) {
                    self.gate_refusals += 1;
                    continue;
                }
                self.next_id += 1;
                let id = self.next_id;
                let (bytes, keyframe) = encoder(self.flow.target_kbps(), id);
                self.flow.on_frame_sent(id, bytes, keyframe, now);
                let start = self.link_free_at.max(now);
                self.link_free_at = start + Duration::from_secs_f64(bytes as f64 / self.link_bps);
                let arrived = self.link_free_at + self.one_way;
                let dec_start = self.decoder_free_at.max(arrived);
                self.decoder_free_at = dec_start + self.decode_time;
                let decoded = self.decoder_free_at;
                self.log.push((now - self.t0, decoded - self.t0, bytes));
                if self.acks_enabled {
                    let viewer_us = (decoded - arrived).as_micros() as u32;
                    self.acks.push_back((decoded + self.one_way, id, viewer_us));
                }
            }
        }

        /// (fps, p95 send->decoded latency in ms, Mbps) for frames sent in `[from, to)` seconds.
        fn window(&self, from: f64, to: f64) -> (f64, f64, f64) {
            let frames: Vec<_> = self
                .log
                .iter()
                .filter(|(s, _, _)| (from..to).contains(&s.as_secs_f64()))
                .collect();
            let mut lat: Vec<f64> = frames
                .iter()
                .map(|(s, d, _)| (*d - *s).as_secs_f64() * 1000.0)
                .collect();
            lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p95 = lat.get(lat.len() * 95 / 100).copied().unwrap_or(f64::MAX);
            let bytes: usize = frames.iter().map(|f| f.2).sum();
            let secs = to - from;
            let res = (frames.len() as f64 / secs, p95, bytes as f64 * 8.0 / 1e6 / secs);
            eprintln!(
                "[{from:>4.1}s..{to:>4.1}s] fps {:.1} | p95 send->decoded {:.0} ms | {:.2} Mbps | target {} kbps | window {:.1}",
                res.0, res.1, res.2, self.flow.target_kbps(), self.flow.window
            );
            res
        }
    }

    /// CBR encoder: each frame uses its share of the target; frame 1 is a 150 KB keyframe.
    fn cbr(target_kbps: u32, id: u64) -> (usize, bool) {
        if id == 1 {
            (150_000, true)
        } else {
            ((target_kbps as usize * 1000 / 8 / 60).max(200), false)
        }
    }

    #[test]
    fn five_mbps_relay_keeps_60fps_and_low_latency() {
        let mut sim = Sim::new(8000, 5.0, 20, 2);
        sim.run(20.0, &cbr);
        let (fps, p95, mbps) = sim.window(5.0, 20.0);
        assert!(fps >= 57.0, "fps {fps:.1}");
        assert!(p95 <= 120.0, "p95 latency {p95:.0} ms");
        assert!((3.0..=5.0).contains(&mbps), "throughput {mbps:.2} Mbps");
        assert!(sim.flow.target_kbps() <= 5000, "target {}", sim.flow.target_kbps());
    }

    #[test]
    fn capacity_drop_recovers_quickly() {
        let mut sim = Sim::new(20_000, 10.0, 15, 2);
        sim.run(8.0, &cbr);
        sim.link_bps = 3.0 * 1_000_000.0 / 8.0; // 10 Mbps -> 3 Mbps
        sim.run(12.0, &cbr);
        let (fps, p95, mbps) = sim.window(11.0, 20.0);
        assert!(fps >= 55.0, "fps {fps:.1}");
        assert!(p95 <= 130.0, "p95 latency {p95:.0} ms");
        assert!(mbps <= 3.0, "throughput {mbps:.2} Mbps");
    }

    #[test]
    fn latency_stays_bounded_even_when_encoder_ignores_the_target() {
        // Worst case: ~9 Mbps worth of frames regardless of the requested bitrate.
        let mut sim = Sim::new(8000, 5.0, 20, 2);
        sim.run(15.0, &|_, id| (if id == 1 { 150_000 } else { 9_000_000 / 8 / 60 }, id == 1));
        let (fps, p95, _) = sim.window(3.0, 15.0);
        assert!(p95 <= 200.0, "p95 latency {p95:.0} ms (gate must bound queueing)");
        assert!(fps >= 25.0, "fps {fps:.1}");
    }

    #[test]
    fn slow_viewer_decoder_limits_fps_without_collapsing_bitrate() {
        // Fast LAN, but the viewer can only decode one frame per 30 ms.
        let mut sim = Sim::new(20_000, 100.0, 1, 30);
        sim.run(10.0, &cbr);
        let (fps, p95, _) = sim.window(3.0, 10.0);
        assert!((25.0..=36.0).contains(&fps), "fps {fps:.1}");
        assert!(p95 <= 200.0, "p95 latency {p95:.0} ms");
        assert!(
            sim.flow.target_kbps() >= 6000,
            "network is fine, bitrate must not collapse: {}",
            sim.flow.target_kbps()
        );
    }

    #[test]
    fn legacy_viewer_without_acks_is_never_gated() {
        let mut sim = Sim::new(4000, 5.0, 20, 2);
        sim.acks_enabled = false;
        sim.run(5.0, &cbr);
        assert_eq!(sim.gate_refusals, 0);
        assert!(!sim.flow.acks_seen);
        assert_eq!(sim.flow.target_kbps(), 4000);
    }

    #[test]
    fn lost_acks_never_wedge_the_stream() {
        let mut sim = Sim::new(4000, 5.0, 20, 2);
        sim.run(5.0, &cbr);
        sim.acks_enabled = false;
        sim.acks.clear();
        sim.run(10.0, &cbr);
        let late = sim.log.iter().filter(|(s, _, _)| s.as_secs_f64() >= 6.0).count();
        assert!(late >= 3, "only {late} frames got through after acks stopped");
    }

    #[test]
    fn keyframe_burst_does_not_crash_the_bitrate() {
        let mut sim = Sim::new(4000, 5.0, 20, 2);
        sim.run(6.0, &cbr);
        let before = sim.flow.target_kbps();
        // A 250 KB keyframe mid-stream (monitor switch / viewer recovery request).
        let kf_id = sim.next_id + 1;
        sim.run(4.0, &|t, id| if id == kf_id { (250_000, true) } else { cbr(t, id) });
        let after = sim.flow.target_kbps();
        assert!(
            after as f64 >= before as f64 * 0.8,
            "keyframe caused bitrate collapse: {before} -> {after}"
        );
        let (fps, _, _) = sim.window(7.0, 10.0);
        assert!(fps >= 55.0, "fps after keyframe {fps:.1}");
    }

    #[test]
    fn transient_spike_with_light_content_does_not_collapse_bitrate() {
        // Content only needs ~0.6 Mbps of a 4 Mbps budget; then a 250 ms hiccup (CPU stall,
        // Wi-Fi retry burst). The delivery rate here measures the content, not the link.
        let light = |_: u32, id: u64| {
            if id == 1 {
                (150_000, true)
            } else {
                (600_000 / 8 / 60, false)
            }
        };
        let mut sim = Sim::new(4000, 5.0, 20, 2);
        sim.run(6.0, &light);
        let before = sim.flow.target_kbps();
        sim.link_free_at = sim.t0 + TICK * sim.tick as u32 + Duration::from_millis(250);
        sim.run(4.0, &light);
        let after = sim.flow.target_kbps();
        assert!(
            after as f64 >= before as f64 * 0.8,
            "light content + transient spike collapsed the bitrate: {before} -> {after}"
        );
    }

    #[test]
    fn windowed_min_tracks_and_expires() {
        let t0 = Instant::now();
        let mut m = WindowedMin::new(Duration::from_secs(1));
        m.push(t0, 50.0);
        m.push(t0 + Duration::from_millis(100), 70.0);
        assert_eq!(m.get(), Some(50.0));
        m.push(t0 + Duration::from_millis(1500), 90.0);
        assert_eq!(m.get(), Some(90.0));
    }
}

