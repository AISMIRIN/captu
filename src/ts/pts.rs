// 33-bit MPEG-2 PTS/PCR normalisation.
//
// MPEG-2 (ISO 13818-1) presentation timestamps are a 33-bit counter driven by a
// 90 kHz clock.  It wraps every 2^33 / 90000 s (about 26 h 30 min), so a
// recording that crosses the wrap point sees the raw value fall from ~2^33 back
// to ~0.  Subtracting a fixed epoch with u64 arithmetic wraps at 2^64 instead of
// 2^33 and yields ~2.05e17 ms.
//
// This module is the ONE place where 33-bit unwrapping happens.  It is pure and
// fully unit-testable, so the arithmetic can never again hide inside a function
// that also performs file I/O.

/// 33-bit PTS/PCR modulus, in 90 kHz ticks.
const PTS_WRAP: u64 = 1 << 33;

/// Mask for a 33-bit PTS/PCR value.
const PTS_MASK: u64 = PTS_WRAP - 1;

/// Half the modulus: the ambiguity limit of the nearest-difference rule.
/// 2^32 ticks is about 13 h 15 min, far longer than any single recording, so the
/// rule always picks the physically correct direction across a wrap.
const PTS_HALF_WRAP: i64 = (PTS_WRAP / 2) as i64;

/// Backward jitter absorbed without declaring a discontinuity (1 s).
/// The ARIB caption ES carries no B-frames, so its PTS is monotonic by
/// construction; sub-second regressions come only from multiplexer jitter.
const PTS_BACKWARD_TOLERANCE: i64 = 90_000;

/// Forward gap still accepted as real elapsed time (3 h).
///
/// A dropped-packet burst does not move the encoder clock, so recovery after a
/// drop appears as a *genuine* gap and must be preserved.  Only a spliced or
/// restarted stream produces an arbitrary jump.
///
/// The threshold is deliberately generous.  A false positive (a real
/// caption-free stretch misread as a splice) collapses the timeline and
/// misaligns every later caption, while a false negative merely leaves an offset
/// that is still bounded and monotonic.  The nearest-difference rule already
/// bounds any delta to +/-13 h 15 min, so a 3 h gate keeps every plausible
/// recording intact while still catching most real splices.
const PTS_FORWARD_LIMIT: i64 = 90_000 * 3600 * 3;

/// Nominal advance applied across a detected discontinuity (1 s).
/// Zero would make two consecutive captions share a timestamp and break the
/// `pts_end = next.pts_start` derivation in `ts::subtitle`.
const PTS_DISCONTINUITY_ADVANCE: i64 = 90_000;

/// Upper plausibility bound for any timestamp leaving `src/ts/` (24 h in ms).
/// Deliberately below the 26 h 30 min wrap period, so a single missed wrap can
/// never masquerade as a plausible value.
pub const MAX_PLAUSIBLE_PTS_MS: i64 = 24 * 60 * 60 * 1000;

/// One normalised timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtsSample {
    /// Milliseconds since the epoch.
    pub ms: i64,
    /// True when this sample followed an implausible jump and the timeline had
    /// to be re-anchored.  Timestamps after a discontinuity stay monotonic and
    /// in range, but their absolute offset into the file is not trustworthy.
    pub discontinuity: bool,
}

/// Stateful 33-bit unwrapper.  Feed raw PTS values in stream order.
#[derive(Debug, Default)]
pub struct PtsNormalizer {
    /// Previous raw 33-bit value (already masked).  None until the first push.
    prev_raw: Option<u64>,
    /// Unwrapped 90 kHz tick count relative to the epoch.
    ticks: i64,
    /// Number of discontinuities absorbed so far.
    discontinuities: u32,
}

impl PtsNormalizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the epoch with a value that is not itself emitted, so the first
    /// pushed sample is already offset from it.
    ///
    /// Used to anchor caption PTS to the first PCR of the file, which places
    /// t = 0 at the start of the recording rather than at the first caption.
    pub fn with_epoch(raw_epoch_90k: u64) -> Self {
        Self {
            prev_raw: Some(raw_epoch_90k & PTS_MASK),
            ticks: 0,
            discontinuities: 0,
        }
    }

    /// Feed the next raw 33-bit PTS, in stream order.
    pub fn push(&mut self, raw_pts_90k: u64) -> PtsSample {
        let raw = raw_pts_90k & PTS_MASK;

        let Some(prev) = self.prev_raw else {
            // The first sample defines the epoch.
            self.prev_raw = Some(raw);
            self.ticks = 0;
            return PtsSample {
                ms: 0,
                discontinuity: false,
            };
        };

        // Nearest modular difference, mapped into (-2^32, 2^32].  This is what
        // turns a wrap (prev near 2^33, raw near 0) into a small positive delta
        // instead of a ~-2^33 one.
        let mut delta = (raw.wrapping_sub(prev) & PTS_MASK) as i64;
        if delta > PTS_HALF_WRAP {
            delta -= PTS_WRAP as i64;
        }

        let plausible_step = -PTS_BACKWARD_TOLERANCE..=PTS_FORWARD_LIMIT;
        let discontinuity = !plausible_step.contains(&delta);
        let advance = if discontinuity {
            PTS_DISCONTINUITY_ADVANCE
        } else {
            delta
        };

        self.prev_raw = Some(raw);
        // saturating_add is defence only: ticks cannot realistically overflow.
        self.ticks = self.ticks.saturating_add(advance);
        if discontinuity {
            self.discontinuities += 1;
        }

        PtsSample {
            ms: self.ticks / 90,
            discontinuity,
        }
    }

    /// Number of discontinuities absorbed so far.
    pub fn discontinuities(&self) -> u32 {
        self.discontinuities
    }
}

/// True when a caption timestamp pair is usable for seeking and display.
pub fn is_plausible_pts(start_ms: i64, end_ms: i64) -> bool {
    start_ms >= 0 && end_ms > start_ms && end_ms <= MAX_PLAUSIBLE_PTS_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 90_000;

    #[test]
    fn normalizer_first_sample_is_zero() {
        let mut n = PtsNormalizer::new();
        let s = n.push(123_456);
        assert_eq!(s.ms, 0);
        assert!(!s.discontinuity);
    }

    #[test]
    fn normalizer_monotonic_stream() {
        let mut n = PtsNormalizer::new();
        let out: Vec<i64> = [0, SEC, 2 * SEC].iter().map(|&p| n.push(p).ms).collect();
        assert_eq!(out, vec![0, 1000, 2000]);
        assert_eq!(n.discontinuities(), 0);
    }

    #[test]
    fn normalizer_unwraps_rollover() {
        // Last value before the wrap, then the counter restarts from 0.
        let mut n = PtsNormalizer::new();
        let out: Vec<i64> = [PTS_WRAP - SEC, 0, SEC]
            .iter()
            .map(|&p| n.push(p).ms)
            .collect();
        assert_eq!(out, vec![0, 1000, 2000]);
        assert_eq!(n.discontinuities(), 0);
    }

    #[test]
    fn normalizer_max_33bit_to_zero() {
        // 2^33 - 1 followed by 0 is a single tick forward, not a 26-hour jump.
        let mut n = PtsNormalizer::new();
        assert_eq!(n.push(PTS_MASK).ms, 0);
        let s = n.push(0);
        assert_eq!(s.ms, 0); // 1 tick / 90 truncates to 0 ms
        assert!(!s.discontinuity);
        assert_eq!(n.discontinuities(), 0);
    }

    #[test]
    fn normalizer_reproduces_reported_bug() {
        // Regression: a recording whose epoch sits 51_162_976 ticks (568.5 s)
        // before the 33-bit rollover used to render as "56934395262:52:22"
        // because `pts.wrapping_sub(epoch)` wrapped at 2^64 instead of 2^33.
        let epoch = PTS_WRAP - 51_162_976; // 8_538_771_616
        let mut n = PtsNormalizer::new();
        assert_eq!(n.push(epoch).ms, 0);
        let s = n.push(0);
        assert_eq!(s.ms, 568_477);
        assert!(!s.discontinuity);
        assert!(s.ms < MAX_PLAUSIBLE_PTS_MS);
    }

    #[test]
    fn normalizer_tolerates_backward_jitter() {
        let mut n = PtsNormalizer::new();
        assert_eq!(n.push(10 * SEC).ms, 0);
        // 0.5 s backwards: absorbed as a negative delta, not a discontinuity.
        let s = n.push(10 * SEC - SEC / 2);
        assert_eq!(s.ms, -500);
        assert!(!s.discontinuity);
        assert_eq!(n.discontinuities(), 0);
    }

    #[test]
    fn normalizer_flags_large_backward_jump() {
        let mut n = PtsNormalizer::new();
        assert_eq!(n.push(4000 * SEC).ms, 0);
        let s = n.push(4000 * SEC - 3600 * SEC); // 1 hour backwards
        assert!(s.discontinuity);
        assert_eq!(s.ms, 1000); // nominal 1 s advance
        assert_eq!(n.discontinuities(), 1);
    }

    #[test]
    fn normalizer_flags_large_forward_jump() {
        let mut n = PtsNormalizer::new();
        assert_eq!(n.push(SEC).ms, 0);
        let s = n.push(SEC + 5 * 3600 * SEC); // 5 hours forward
        assert!(s.discontinuity);
        assert_eq!(s.ms, 1000);
        assert_eq!(n.discontinuities(), 1);
    }

    #[test]
    fn normalizer_long_caption_free_gap_is_real() {
        // A 2 h stretch without captions is genuine elapsed time, not a splice.
        let mut n = PtsNormalizer::new();
        assert_eq!(n.push(SEC).ms, 0);
        let s = n.push(SEC + 2 * 3600 * SEC);
        assert!(!s.discontinuity);
        assert_eq!(s.ms, 2 * 3600 * 1000);
    }

    #[test]
    fn normalizer_masks_high_bits() {
        let mut a = PtsNormalizer::new();
        let mut b = PtsNormalizer::new();
        for &p in &[7 * SEC, 8 * SEC, 9 * SEC] {
            assert_eq!(a.push(p).ms, b.push(p | (1 << 40)).ms);
        }
    }

    #[test]
    fn normalizer_counts_discontinuities() {
        let mut n = PtsNormalizer::new();
        n.push(0);
        for i in 1..=3 {
            // Each jump is far beyond the forward limit and far from a wrap.
            n.push(i * 5 * 3600 * SEC);
        }
        assert_eq!(n.discontinuities(), 3);
    }

    #[test]
    fn normalizer_step_is_always_bounded() {
        // The property this module exists to guarantee: no single input, however
        // garbled, can move the timeline by more than the forward limit.  That is
        // what makes the reported failure impossible — a 33-bit wrap used to jump
        // straight to ~2.05e17 ms in one step.
        //
        // Note this is a *per-step* guarantee, not an absolute one: a long enough
        // run of large-but-legal gaps still accumulates past MAX_PLAUSIBLE_PTS_MS.
        // The absolute bound is a separate concern, enforced downstream by
        // `is_plausible_pts`.
        //
        // Fixed LCG keeps this deterministic without a `rand` dependency.
        const MAX_STEP_MS: i64 = PTS_FORWARD_LIMIT / 90;
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut n = PtsNormalizer::new();
        let mut prev = 0i64;
        for _ in 0..10_000 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let s = n.push(state >> 20);
            // +1 absorbs the truncation of the running tick count by /90.
            assert!(
                (s.ms - prev).abs() <= MAX_STEP_MS + 1,
                "step too large: {} -> {}",
                prev,
                s.ms
            );
            prev = s.ms;
        }
    }

    #[test]
    fn is_plausible_pts_boundaries() {
        assert!(is_plausible_pts(0, 1));
        assert!(is_plausible_pts(0, MAX_PLAUSIBLE_PTS_MS));
        assert!(!is_plausible_pts(-1, 100));
        assert!(!is_plausible_pts(0, 0));
        assert!(!is_plausible_pts(100, 100));
        assert!(!is_plausible_pts(0, MAX_PLAUSIBLE_PTS_MS + 1));
        assert!(!is_plausible_pts(i64::MIN, 0));
        assert!(!is_plausible_pts(0, i64::MAX));
    }
}
