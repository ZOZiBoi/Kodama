//! The gain ramp behind the two-sink crossfade, apart from the audio thread so it can be tested.
//!
//! The outgoing side goes from `out_from` down to silence while the incoming side comes up from
//! silence to full, both linear over the same stretch. `out_from` is 1.0 for a fade out of a
//! track playing normally. It is less when a skip arrives in the middle of a fade: whichever of
//! the two tracks is louder at that moment stays on as the one going out, from the gain it had
//! reached (see `Ramp::handover`), so no sink ever jumps in loudness.

use std::time::Instant;

/// Shortest fade worth running; anything less is a cut with extra steps.
pub const MIN_FADE_SECS: f64 = 0.1;

/// How long a fade may last, given what was asked for and how much is left of the outgoing track.
///
/// A fade cannot outlast the song it fades out of — the outgoing sink would run dry halfway and
/// the incoming one jump to full volume. `out_duration` 0 means the length is not known (yet),
/// and then the request stands.
pub fn fade_len(requested: f64, out_duration: f64, out_pos: f64) -> f64 {
    let mut len = if requested.is_finite() { requested } else { 0.0 };
    if out_duration > 0.0 {
        len = len.min(out_duration - out_pos);
    }
    len.max(MIN_FADE_SECS)
}

pub struct Ramp {
    len: f64,
    /// Time spent ramping before the current run, so a pause holds the fade where it is.
    banked: f64,
    /// When the current run began; None while paused.
    since: Option<Instant>,
    out_from: f32,
}

impl Ramp {
    pub fn start(len: f64, out_from: f32, now: Instant, paused: bool) -> Self {
        Ramp {
            len: len.max(MIN_FADE_SECS),
            banked: 0.0,
            since: if paused { None } else { Some(now) },
            out_from: out_from.clamp(0.0, 1.0),
        }
    }

    /// 0.0 at the start, 1.0 once the fade is complete.
    pub fn progress(&self, now: Instant) -> f32 {
        let run = self.since.map(|s| now.saturating_duration_since(s).as_secs_f64()).unwrap_or(0.0);
        ((self.banked + run) / self.len).clamp(0.0, 1.0) as f32
    }

    pub fn pause(&mut self, now: Instant) {
        if let Some(s) = self.since.take() {
            self.banked += now.saturating_duration_since(s).as_secs_f64();
        }
    }

    pub fn resume(&mut self, now: Instant) {
        if self.since.is_none() {
            self.since = Some(now);
        }
    }

    /// (outgoing, incoming) as fractions of the player volume.
    pub fn gains(&self, now: Instant) -> (f32, f32) {
        let p = self.progress(now);
        (self.out_from * (1.0 - p), p)
    }

    pub fn done(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }

    /// A new track is ready while this fade is still running, and one of the three sinks that
    /// would make has to go.
    ///
    /// While the outgoing track is still the louder, the fade carries on as it was and the new
    /// track simply takes the incoming side's place, at the incoming gain. Once the incoming track
    /// has become the louder, it turns into the outgoing side of a fresh fade, from the gain it
    /// reached, and the old outgoing track (by then under half volume) leaves.
    ///
    /// Restarting the fade every time would let a burst of skips sink into near-silence: each new
    /// fade would begin out of a track that had barely come up. This way the louder of the two
    /// sides never starts a fade below half volume.
    pub fn handover(&self, now: Instant) -> Handover {
        let (out, inc) = self.gains(now);
        if out >= inc { Handover::SwapIncoming(inc) } else { Handover::Restart(inc) }
    }
}

/// What a fade interrupted by a new track does (see `Ramp::handover`). Both carry the gain of
/// the incoming side at that moment.
#[derive(Debug, PartialEq)]
pub enum Handover {
    /// The running fade goes on; the new track replaces the incoming one at this gain.
    SwapIncoming(f32),
    /// The incoming track becomes the outgoing side of a new fade, from this gain.
    Restart(f32),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(t0: Instant, secs: f64) -> Instant {
        t0 + Duration::from_secs_f64(secs)
    }

    #[test]
    fn a_full_fade_goes_from_one_side_to_the_other() {
        let t0 = Instant::now();
        let r = Ramp::start(4.0, 1.0, t0, false);
        assert_eq!(r.gains(t0), (1.0, 0.0));
        let (o, i) = r.gains(at(t0, 2.0));
        assert!((o - 0.5).abs() < 1e-4 && (i - 0.5).abs() < 1e-4);
        assert_eq!(r.gains(at(t0, 4.0)), (0.0, 1.0));
        assert_eq!(r.gains(at(t0, 9.0)), (0.0, 1.0), "stays at the end, never overshoots");
        assert!(!r.done(at(t0, 3.99)) && r.done(at(t0, 4.0)));
    }

    #[test]
    fn a_pause_holds_the_fade_where_it_is() {
        let t0 = Instant::now();
        let mut r = Ramp::start(4.0, 1.0, t0, false);
        r.pause(at(t0, 1.0));
        assert_eq!(r.progress(at(t0, 1.0)), r.progress(at(t0, 30.0)));
        r.resume(at(t0, 30.0));
        assert!((r.progress(at(t0, 31.0)) - 0.5).abs() < 1e-4, "1s before + 1s after the pause");
        // Pausing or resuming twice changes nothing.
        r.resume(at(t0, 31.5));
        r.pause(at(t0, 32.0));
        r.pause(at(t0, 40.0));
        assert!((r.progress(at(t0, 50.0)) - 0.75).abs() < 1e-4);
    }

    #[test]
    fn a_fade_started_while_paused_waits_for_resume() {
        let t0 = Instant::now();
        let mut r = Ramp::start(2.0, 1.0, t0, true);
        assert_eq!(r.progress(at(t0, 5.0)), 0.0);
        r.resume(at(t0, 5.0));
        assert!((r.progress(at(t0, 6.0)) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn the_length_is_clamped_to_what_is_left_of_the_outgoing_track() {
        assert_eq!(fade_len(8.0, 200.0, 100.0), 8.0);
        assert_eq!(fade_len(8.0, 200.0, 197.0), 3.0);
        assert_eq!(fade_len(8.0, 0.0, 50.0), 8.0, "unknown duration: the request stands");
        assert_eq!(fade_len(8.0, 200.0, 200.0), MIN_FADE_SECS);
        assert_eq!(fade_len(8.0, 200.0, 250.0), MIN_FADE_SECS, "position past the end");
        assert_eq!(fade_len(f64::NAN, 200.0, 10.0), MIN_FADE_SECS);
        assert_eq!(fade_len(0.0, 200.0, 10.0), MIN_FADE_SECS);
    }

    #[test]
    fn the_louder_side_decides() {
        let t0 = Instant::now();
        let r = Ramp::start(4.0, 1.0, t0, false);
        assert_eq!(r.handover(at(t0, 1.0)), Handover::SwapIncoming(0.25));
        assert_eq!(r.handover(at(t0, 3.0)), Handover::Restart(0.75));
        // A tie keeps the fade going: the outgoing track is the one that has been playing.
        assert_eq!(r.handover(at(t0, 2.0)), Handover::SwapIncoming(0.5));
    }

    /// Rapid skipping: every `gap` seconds a new track arrives while the last fade is still
    /// running, handed over the way the audio thread does. A sink that carries on may not change
    /// loudness by more than one tick's worth of ramp, a new track enters at the gain of the one
    /// it replaces, and the two together never drop below half volume - a burst of skips must
    /// not fade into silence. Afterwards the last track plays at full volume.
    fn rapid_skips(gap: f64, count: usize) {
        let t0 = Instant::now();
        let len = 3.0;
        let tick = 0.01;
        let max_step = (tick / len) as f32 + 1e-4;
        let debug = std::env::var_os("KODAMA_XFADE_DEBUG").is_some();

        let mut ramp = Ramp::start(len, 1.0, t0, false);
        // Gains of the two tracked sinks after the previous tick: (outgoing, incoming).
        let mut prev = ramp.gains(t0);
        let mut t = 0.0;
        let mut skips = 0;
        let end = gap * count as f64 + 2.0 * len;
        while t < end {
            t += tick;
            let now = at(t0, t);
            if skips < count && t >= gap * (skips + 1) as f64 {
                let h = ramp.handover(now);
                let (o, i) = ramp.gains(now);
                match h {
                    // The fade goes on; the new track takes the incoming side's gain.
                    Handover::SwapIncoming(g) => assert_eq!(g, i),
                    // The incoming track goes out from where it was; the new one starts silent.
                    Handover::Restart(g) => {
                        assert_eq!(g, i);
                        assert!(g >= 0.5, "a fade restarted out of a quiet track: {g}");
                        ramp = Ramp::start(len, g, now, false);
                    }
                }
                if debug {
                    eprintln!("[xfade-test] gap {gap}s, skip {skips} at {t:.2}s (out={o:.3} in={i:.3}): {h:?}");
                }
                prev = ramp.gains(now);
                skips += 1;
                continue;
            }
            let (o, i) = ramp.gains(now);
            assert!((o - prev.0).abs() <= max_step, "outgoing jumped at {t:.2}s: {} -> {o}", prev.0);
            assert!((i - prev.1).abs() <= max_step, "incoming jumped at {t:.2}s: {} -> {i}", prev.1);
            assert!(o + i >= 0.5 - 1e-4, "near-silence at {t:.2}s: out={o} in={i}");
            if debug && ((t * 100.0).round() as i64) % 50 == 0 {
                eprintln!("[xfade-test] gap {gap}s, t={t:.2}s out={o:.3} in={i:.3}");
            }
            prev = (o, i);
        }
        assert_eq!(skips, count);
        assert_eq!(ramp.gains(at(t0, t)), (0.0, 1.0), "settles at full volume, nothing stuck");
    }

    #[test]
    fn rapid_skips_hand_over_without_a_jump() {
        rapid_skips(0.05, 50); // key repeat: the first fade simply runs its course
        rapid_skips(0.15, 10); // faster than a fade
        rapid_skips(1.0, 10); // a third of the way in
        rapid_skips(1.5, 10); // exactly halfway
        rapid_skips(1.6, 30); // just past halfway, again and again: the worst case for restarts
        rapid_skips(2.0, 10); // most of a fade: the newest track keeps coming in
    }
}
