//! Precise short waits for frame pacing.
//!
//! `std::thread::sleep` can overshoot by 10+ ms where the OS timer is coarse: measured on a
//! Windows 11 VM, a 1 ms sleep took ~12 ms and a 3 ms sleep ~15 ms, even after
//! `timeBeginPeriod(1)`. At 60 fps the whole frame budget is 16.7 ms, so a pacing sleep of a few
//! ms that turns into 12 ms silently caps the stream around 40 fps. We therefore learn how much
//! this machine's sleeps overshoot, sleep only the part of a wait that is safe, and yield-spin
//! the remainder (on normal hardware with 1 ms timers that remainder is ~1 ms).

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Learned sleep overshoot in microseconds (starts pessimistic, adapts within a few calls).
static OVERSHOOT_US: AtomicU32 = AtomicU32::new(2_000);
const SAFETY: Duration = Duration::from_micros(300);

/// Waits until `deadline` with ~0.1-0.5 ms accuracy regardless of the OS timer granularity.
pub fn sleep_until(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remaining = deadline - now;
        let overshoot = Duration::from_micros(OVERSHOOT_US.load(Ordering::Relaxed) as u64);
        if remaining > overshoot + SAFETY {
            let request = remaining - overshoot - SAFETY;
            let t0 = Instant::now();
            std::thread::sleep(request);
            let over = t0.elapsed().saturating_sub(request).as_micros() as u32;
            // Adapt quickly upwards (missed deadlines hurt), slowly downwards.
            let prev = OVERSHOOT_US.load(Ordering::Relaxed);
            let next = if over > prev {
                (prev + over) / 2 + 1
            } else {
                (prev * 7 + over) / 8
            };
            OVERSHOOT_US.store(next.clamp(200, 20_000), Ordering::Relaxed);
        } else {
            std::thread::yield_now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_until_hits_short_deadlines() {
        // Warm up with waits long enough to actually call the OS sleep, so the overshoot
        // estimate adapts to this machine (on coarse-timer VMs a 1 ms sleep takes ~10 ms).
        for _ in 0..6 {
            sleep_until(Instant::now() + Duration::from_millis(25));
        }
        let mut misses = Vec::new();
        for ms in [1u64, 3, 5, 8, 12, 16] {
            let deadline = Instant::now() + Duration::from_millis(ms);
            sleep_until(deadline);
            misses.push(Instant::now() - deadline);
        }
        misses.sort();
        // Median: robust to one preemption on a loaded test machine. The point is "not a
        // 10-15 ms OS tick".
        let median = misses[misses.len() / 2];
        assert!(median < Duration::from_millis(3), "overshoots {misses:?}");
    }
}
