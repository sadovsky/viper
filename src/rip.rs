//! Reading music back out of a register log.
//!
//! [`viper_apu::trace`] says what the chip was doing each frame. This module
//! decides what a tracker would have written to make it do that: where the
//! rows are, which key-ons are notes, what pitch each one was, and which of
//! them are the same phrase played again.
//!
//! Everything here is inference, and the [`Report`] is as much the product as
//! the song is. An NSF does not record its tempo, its phrase boundaries or
//! its instruments; those are recovered by argument, and a reader deserves to
//! know which numbers were read and which were guessed.

use anyhow::{bail, Result};

use crate::{Cell, Instrument, Song, CHANNELS, STEPS_PER_PHRASE};
use viper_apu::trace::{FrameTrace, NOI, PU1, PU2, TRI};

/// NTSC CPU clock, the divisor behind every period on the chip.
const CPU_HZ: f64 = 1_789_773.0;

/// How far off a row an onset may land before the grid is in doubt, in rows.
/// A quarter row is generous enough to absorb a driver that keys a note one
/// frame late and tight enough that a grid twice too fine cannot hide.
const TOL_ROWS: f64 = 0.25;

#[derive(Clone, Debug, Default)]
pub struct RipOptions {
    /// Tempo, if the caller knows it. Skips detection entirely.
    pub bpm: Option<u16>,
}

/// How well the chosen grid explains the onsets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fit {
    /// Every onset lands exactly on a row start. Only an integer tempo
    /// simulated through the driver's own row clock can achieve this, so it
    /// is a real claim rather than a rounding artefact.
    ///
    /// `ambiguous` names the range of tempos that fit *equally* well, when
    /// more than one does. An exact fit says the grid explains every onset,
    /// not that the tempo is pinned: a short song at a whole number of frames
    /// per row leaves several tempos indistinguishable, and reporting the
    /// chosen one alone would be a confident answer to a question the
    /// evidence did not settle.
    Exact { ambiguous: Option<(u16, u16)> },
    /// Close, with the worst offset in rows and the fraction inside `TOL_ROWS`.
    Fitted { worst_rows: f64, within: f64 },
    /// Not really explained. `collisions` counts onsets that had to share a
    /// row with an earlier one on the same channel, which is the signature of
    /// a grid that is too coarse.
    Guessed { worst_rows: f64, collisions: usize },
    /// The caller supplied the tempo, so nothing was inferred.
    Told,
}

#[derive(Clone, Debug)]
pub struct Grid {
    pub bpm: u16,
    pub frames_per_row: f64,
    /// Frame at which row 0 begins. Drivers call PLAY before their first
    /// row, so this is rarely zero.
    #[allow(dead_code)] // reported by callers that print the grid
    pub phase: u32,
    /// Frame each row starts on.
    pub starts: Vec<u32>,
    pub fit: Fit,
}

/// The frames at which rows begin, under the driver's 8.8 fixed-point row
/// clock, starting from a given accumulator residual.
///
/// Reproducing that accumulator matters. At 220 BPM the clock runs 4, 4, 4
/// and then a 5, so a straight line through the onsets drifts off by a whole
/// row every few hundred. Reproducing it is not enough on its own either:
/// where the driver sits *within* that 4-4-4-5 cycle when the first note
/// lands is not visible from outside, so `cnt0` is a parameter to be
/// searched rather than assumed zero. Getting it wrong costs one frame every
/// twelve rows, which is exactly the kind of error that looks like sloppy
/// timing instead of a wrong model.
fn row_starts(frames_per_row: f64, phase: u32, until: u32, cnt0: i64) -> Vec<u32> {
    let speed = ((frames_per_row * 256.0).round() as i64).max(256);
    let mut cnt = cnt0;
    let mut f = phase;
    let mut out = Vec::new();
    while f <= until {
        if cnt >> 8 == 0 {
            out.push(f);
            cnt += speed;
        }
        cnt -= 256;
        f += 1;
    }
    out
}

/// The distinct accumulator residuals this row clock passes through.
///
/// The fractional part cycles with a short period — twelve rows at 220 BPM —
/// so the whole space of "where the driver was" is small enough to try
/// exhaustively rather than solve.
fn residuals(frames_per_row: f64) -> Vec<i64> {
    let speed = ((frames_per_row * 256.0).round() as i64).max(256);
    let mut cnt: i64 = 0;
    let mut seen = Vec::new();
    for _ in 0..512 {
        if cnt >> 8 == 0 {
            if seen.contains(&cnt) {
                break;
            }
            seen.push(cnt);
            cnt += speed;
        }
        cnt -= 256;
    }
    if seen.is_empty() {
        seen.push(0);
    }
    seen
}

/// Frames on which some channel started an audible note.
///
/// Audible is the operative word. A key-on whose channel has no volume makes
/// no sound, and viper's own driver emits three of them on an empty row; a
/// grid fitted to those is fitted partly to silence.
fn onsets(t: &[FrameTrace]) -> Vec<u32> {
    t.iter()
        .filter(|f| f.ch.iter().any(|c| c.keyed && c.level > 0))
        .map(|f| f.frame)
        .collect()
}

/// The volume column that reproduces an observed level.
///
/// A cell's volume is not the level the chip ends up playing. `Event::Vol`
/// scales the instrument's envelope, and the multiply is fixed-point: with a
/// full-scale envelope the driver plays `(vol * 15) >> 4`, one step short of
/// the volume asked for. Writing the observed level straight into the cell
/// therefore loses a step on every round trip — measurably, a song ripped and
/// recompiled five times fades away — so the mapping has to be inverted, not
/// copied.
///
/// Measured against the reference driver in `tests/fixtures`, where the map
/// is exactly `(vol * 15) >> 4` clamped to a floor of 1. Level 15 is the one
/// value out of reach: it needs an envelope peak of 16, which does not fit in
/// four bits, so it comes back as 14.
///
/// Zero is never produced. It means "channel default" everywhere else in
/// viper, so an audible note must not map to it.
fn cell_volume(level: u8) -> u8 {
    ((16 * level as u16 + 14) / 15).clamp(1, 15) as u8
}

/// Whether a note on this channel is still sounding when the *next* row
/// begins.
///
/// This is what separates a note being held from a note that has ended, and
/// it is the tracker's own question rather than a guess about envelope
/// shapes. Hearing sound at a row start proves nothing on its own: an
/// instrument's release is audible for several frames after the row that
/// ended it, so "still making noise" would turn every note-off into a
/// sustain and fill the transcription with holds nobody wrote.
///
/// Asking whether it survives to the next row is exactly the distinction the
/// format draws. viper's `===` emits nothing and lets a note ring on, while
/// an empty cell emits a note-off — and a note whose sound does not reach the
/// following row is one that ended inside this one.
fn spans_row(t: &[FrameTrace], c: usize, starts: &[u32], r: usize) -> bool {
    // Whole rows, not the single frame each one starts on. A reconstructed
    // grid can sit a frame away from the driver's real one, and sampling one
    // frame at the edge of a gate turns that frame into a coin toss — the
    // same bar then transcribes two ways on two passes through a loop.
    let audible = |from: u32, to: u32| t.iter().any(|f| f.frame >= from && f.frame < to && f.ch[c].level > 0);
    let Some(&next) = starts.get(r + 1) else {
        // The last row has no next row to reach, so nothing is held across it.
        return false;
    };
    let after = starts.get(r + 2).copied().unwrap_or(next + 1);
    audible(starts[r], next) && audible(next, after)
}

/// The row `t` belongs to: the nearest row start, not the enclosing one.
fn nearest_row(starts: &[u32], t: u32) -> usize {
    match starts.binary_search(&t) {
        Ok(i) => i,
        Err(i) => {
            if i == 0 {
                0
            } else if i >= starts.len() {
                starts.len() - 1
            } else if t - starts[i - 1] <= starts[i] - t {
                i - 1
            } else {
                i
            }
        }
    }
}

/// Distance from `t` to the nearest row start, in frames.
fn nearest(starts: &[u32], t: u32) -> u32 {
    match starts.binary_search(&t) {
        Ok(_) => 0,
        Err(i) => {
            let before = if i > 0 { t - starts[i - 1] } else { u32::MAX };
            let after = starts.get(i).map(|&s| s - t).unwrap_or(u32::MAX);
            before.min(after)
        }
    }
}

/// Find the row grid the driver was using.
///
/// Three steps, and the middle one is the whole problem.
///
/// A **coarse sweep** scores candidate row lengths by how nearly the onsets
/// fall on multiples of them. That alone finds the wrong answer every time:
/// any divisor of the true row length fits at least as well, and one frame
/// per row fits perfectly, so the best-scoring candidate is always the
/// uselessly fine one. The ambiguity only ever runs that way — a grid
/// *coarser* than the truth genuinely fails — so the fix is to take the
/// largest candidate that still fits rather than the best-fitting one.
///
/// Then a **snap to an integer tempo**, simulating the driver's real row
/// clock for each nearby BPM and keeping whichever puts the most onsets
/// exactly on a row. A regression through the onsets gets close, but the
/// row clock is a fixed-point accumulator, not a line.
pub fn fit_grid(t: &[FrameTrace], bpm: Option<u16>) -> Grid {
    let ons = onsets(t);
    let last = t.last().map(|f| f.frame).unwrap_or(0);
    let phase = ons.first().copied().unwrap_or(0);

    if let Some(b) = bpm {
        let fpr = 900.0 / (b.max(1) as f64);
        let (starts, _) = best_alignment(fpr, phase, last, &ons);
        return Grid { bpm: b, frames_per_row: fpr, phase, starts, fit: Fit::Told };
    }
    if ons.len() < 2 {
        // Nothing to fit. 150 BPM is viper's own default; say it was a guess.
        let fpr = 900.0 / 150.0;
        return Grid {
            bpm: 150,
            frames_per_row: fpr,
            phase,
            starts: row_starts(fpr, phase, last, 0),
            fit: Fit::Guessed { worst_rows: 0.0, collisions: 0 },
        };
    }

    // Coarse: the largest row length that puts every onset within tolerance.
    let mut best_coarse = 0.0f64;
    let mut fallback = (f64::MAX, 4.0f64);
    let mut fpr = 1.5f64;
    while fpr <= 32.0 {
        let worst = ons
            .iter()
            .map(|&x| {
                let r = (x - phase) as f64 / fpr;
                (r - r.round()).abs()
            })
            .fold(0.0f64, f64::max);
        if worst < TOL_ROWS {
            best_coarse = fpr;
        }
        if worst < fallback.0 {
            fallback = (worst, fpr);
        }
        fpr += 1.0 / 128.0;
    }
    // Nothing fit: keep the least-bad, and the confidence will say so.
    if best_coarse == 0.0 {
        best_coarse = fallback.1;
    }

    // Snap: try integer tempos either side and simulate the real row clock.
    let bpm0 = (900.0 / best_coarse).round().clamp(1.0, 900.0) as u16;
    // Score every nearby integer tempo, then take the middle of whichever
    // ones tie for best.
    //
    // Ties are not a corner case. A row length is a whole number of frames
    // most of the time, so any tempo that rounds to the same row length
    // explains the same onsets exactly — sixteen rows six frames apart are
    // equally good evidence for 149, 150 and 151 BPM. Picking the first or
    // the nearest candidate then lands one off a round tempo for no reason,
    // while the middle of the feasible range is the best estimate available
    // and happens to be the round number a composer typed.
    let lo = bpm0.saturating_sub(4).max(1);
    let hi = (bpm0 + 4).min(900);
    let scored: Vec<(u16, u64)> = (lo..=hi)
        .map(|c| (c, best_alignment(900.0 / c as f64, phase, last, &ons).1))
        .collect();
    let score = scored.iter().map(|&(_, s)| s).min().unwrap_or(u64::MAX);
    let tied: Vec<u16> = scored.iter().filter(|&&(_, s)| s == score).map(|&(c, _)| c).collect();
    let bpm = tied[tied.len() / 2];
    let fpr = 900.0 / bpm as f64;
    let (starts, _) = best_alignment(fpr, phase, last, &ons);
    let ambiguous = (tied.len() > 1).then(|| (tied[0], tied[tied.len() - 1]));

    // Confidence. "Exact" is only claimed when every onset is on a row.
    let worst_rows = ons.iter().map(|&x| nearest(&starts, x) as f64 / fpr).fold(0.0f64, f64::max);
    let within = ons.iter().filter(|&&x| (nearest(&starts, x) as f64 / fpr) <= TOL_ROWS).count() as f64 / ons.len() as f64;
    let fit = if score == 0 {
        Fit::Exact { ambiguous }
    } else if within >= 0.95 {
        Fit::Fitted { worst_rows, within }
    } else {
        Fit::Guessed { worst_rows, collisions: collisions(t, &starts) }
    };
    Grid { bpm, frames_per_row: fpr, phase, starts, fit }
}

/// Cut a repeated tail off the row list, returning the period it folded at.
///
/// Loop detection by RAM hashing finds the first frame whose *driver state*
/// repeats, which routinely sits a pass later than the musical loop point, so
/// a rip arrives holding the song roughly twice. The rows themselves say
/// where it actually repeats, and they are the better witness: this is the
/// same question phrase deduplication answers one bar at a time, asked of the
/// whole song at once.
///
/// Only whole phrases are considered, because a period that is not a multiple
/// of the phrase length would not survive being written to a `.vip` anyway.
///
/// The very last row is left out of the comparison. A render stops on a frame
/// boundary rather than a row boundary, so the final row is always cut short:
/// a note still sounding there has nowhere to be held to, and it would differ from
/// its own earlier copy for that reason alone.
fn fold_repeats(rows: &mut Vec<[Cell; CHANNELS]>) -> Option<usize> {
    let n = rows.len();
    let mut p = STEPS_PER_PHRASE;
    while p * 2 <= n {
        if (0..n.saturating_sub(p + 1)).all(|i| rows[i] == rows[i + p]) {
            rows.truncate(p);
            return Some(p);
        }
        p += STEPS_PER_PHRASE;
    }
    None
}

/// Row starts for this tempo under whichever accumulator phase explains the
/// onsets best, with that phase's total error in frames.
fn best_alignment(fpr: f64, phase: u32, last: u32, ons: &[u32]) -> (Vec<u32>, u64) {
    let mut best = (Vec::new(), u64::MAX);
    for cnt0 in residuals(fpr) {
        let starts = row_starts(fpr, phase, last + 1, cnt0);
        let score: u64 = ons.iter().map(|&x| nearest(&starts, x) as u64).sum();
        if score < best.1 {
            best = (starts, score);
        }
    }
    best
}

/// Onsets that had to share a row with an earlier one on the same channel.
/// A tracker row holds one note, so this counts music the grid cannot hold —
/// the signature of a grid that is too coarse.
fn collisions(t: &[FrameTrace], starts: &[u32]) -> usize {
    let mut n = 0;
    let mut last_row = [usize::MAX; CHANNELS];
    for f in t {
        for c in 0..CHANNELS {
            if f.ch[c].keyed && f.ch[c].level > 0 {
                let r = match starts.binary_search(&f.frame) {
                    Ok(i) => i,
                    Err(i) => i.saturating_sub(1),
                };
                if last_row[c] == r {
                    n += 1;
                }
                last_row[c] = r;
            }
        }
    }
    n
}

/// A pulse period back to a MIDI note. `None` for periods the hardware mutes
/// or that fall outside the note range, rather than inventing a pitch.
fn pulse_note(p: u16) -> Option<u8> {
    if p < 8 {
        return None;
    }
    note_of(CPU_HZ / (16.0 * (p as f64 + 1.0)))
}

/// The triangle divides by 32 rather than 16, which is why it reaches an
/// octave below the pulses.
fn tri_note(p: u16) -> Option<u8> {
    if p < 2 {
        return None;
    }
    note_of(CPU_HZ / (32.0 * (p as f64 + 1.0)))
}

fn note_of(hz: f64) -> Option<u8> {
    let n = (69.0 + 12.0 * (hz / 440.0).log2()).round();
    (0.0..=127.0).contains(&n).then_some(n as u8)
}

/// The noise channel's 4-bit index back to a note.
///
/// `compile::noise_period_index` maps four semitones onto each index, so this
/// is irreducibly lossy: any of four notes could have produced a given index.
/// Picking the top of the bucket is what round-trips the values a tracker
/// actually writes, because integer division truncates downward.
fn noise_note(idx: u16) -> u8 {
    36 + 4 * (15 - idx.min(15) as u8) + 3
}

/// The envelope value that produces an observed level at a given column
/// volume, inverting the driver's `(vol * env) >> 4`.
fn env_for(level: u8, vol: u8) -> u8 {
    if level == 0 || vol == 0 {
        return 0;
    }
    ((16 * level as u16).div_ceil(vol as u16)).min(15) as u8
}

/// One note's envelope, as the chip played it: levels from the key-on until
/// the channel is keyed again or falls silent.
#[derive(Clone, Debug)]
struct NoteEnv {
    ch: usize,
    /// The `$4000`-family duty bits this note sounded with.
    duty: u8,
    levels: Vec<u8>,
}

/// An ADSR read off an observed envelope, in frames.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Adsr {
    attack: usize,
    decay: usize,
    release: usize,
    peak: u8,
    sustain: u8,
}

/// Read an ADSR out of the levels one note actually played.
///
/// This is a construction, not a fit. `Envelope::from_adsr` lays a note out
/// as a rising attack, a decay to a sustain level, a held frame that loops,
/// and a release to zero — so those four parts can be read straight back off
/// the shape, provided the shape is read in envelope units rather than
/// audible ones. The driver plays `(vol * env) >> 4`, and inverting that
/// first is what makes the reconstruction exact instead of approximate:
/// `10 9 9 9 7 4 2 0` at full column volume is the envelope
/// `11 10 10 10 8 5 3 0`, which is `a=0 d=1 s=10/11 r=4` and nothing else.
fn read_adsr(levels: &[u8], vol: u8) -> Option<Adsr> {
    let env: Vec<u8> = levels.iter().map(|&l| env_for(l, vol)).collect();
    let peak = *env.iter().max()?;
    if peak == 0 {
        return None;
    }
    let at_peak = env.iter().position(|&v| v == peak)?;
    // Everything before the peak is the attack ramp.
    let attack = at_peak;
    // The sustain is the level the envelope settles on: the last value
    // before it starts falling to silence, or the final value if it never
    // does. A note cut short by the next key-on never shows its release,
    // which is why the longest note in a group is the one worth reading.
    let tail = &env[at_peak..];
    let end = tail.iter().position(|&v| v == 0).unwrap_or(tail.len());
    let body = &tail[..end];
    let released = end < tail.len();
    // Walk back from the end of the body over the release ramp: strictly
    // falling values that lead into the silence.
    let mut r = 0usize;
    if released {
        while r + 1 < body.len() && body[body.len() - 1 - r] < body[body.len() - 2 - r] {
            r += 1;
        }
        r += 1; // the step into zero
    }
    let sustain = *body.get(body.len().saturating_sub(r + 1)).unwrap_or(&peak);
    // What is left between the peak and the sustain plateau is the decay.
    let plateau = body.len().saturating_sub(r);
    let decay = body[..plateau].iter().position(|&v| v == sustain).unwrap_or(0);
    Some(Adsr { attack, decay, release: r, peak, sustain })
}

/// What the pitch was doing inside one note.
///
/// A tracker cell holds one effect, and `compile` only emits it on a cell
/// that carries a note, so a note's whole period series has to reduce to a
/// single answer. That constraint does most of the work: the three motions
/// below look nothing like each other over a few frames.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Motion {
    Steady,
    /// `Vdr`: the period wobbles around the note by less than a semitone.
    Vibrato { depth: u8, rate: u8 },
    /// `Axy`: the period cycles through the note and two offsets from it,
    /// one frame each.
    Arpeggio { x: u8, y: u8 },
    /// `Sxx`: the period walks to a new pitch at a fixed number of units per
    /// frame. Portamento does not retrigger, so this is the one motion that
    /// arrives with no key-on to hang an effect on.
    Slide { speed: u8 },
}

/// Read the pitch motion out of one note's periods.
///
/// The numbers come back exactly, because each effect writes its parameter
/// into the register stream in a form that survives. Measured against the
/// reference driver: vibrato spans `depth - 1` period units peak to peak and
/// repeats every `32 / rate` frames; portamento moves `speed` units on its
/// first frame; and an arpeggio's offsets are just the notes it visits.
fn classify(periods: &[u16], ch: usize) -> Motion {
    if periods.len() < 3 {
        return Motion::Steady;
    }
    let (lo, hi) = (*periods.iter().min().unwrap(), *periods.iter().max().unwrap());
    if lo == hi {
        return Motion::Steady;
    }
    let note_of = |p: u16| if ch == TRI { tri_note(p) } else { pulse_note(p) };

    // Arpeggio: two or three distinct pitches, revisited every few frames.
    // The chip is stepping between them one frame at a time, which nothing
    // else on this list does.
    let all: Vec<u8> = periods.iter().filter_map(|&p| note_of(p)).collect();
    // Pitches the cycle actually visits, not the ones it passes through. A
    // period is written a byte at a time, so a frame can be sampled between
    // two settings and name a note that was never played; those appear once
    // and would otherwise make a three-note arpeggio look like a four-note
    // one, which is not an arpeggio at all.
    let visited: Vec<u8> = all.iter().copied().filter(|n| all.iter().filter(|m| *m == n).count() >= 2).collect();
    let mut distinct: Vec<u8> = visited.clone();
    distinct.sort_unstable();
    distinct.dedup();
    // An arpeggio steps every frame. Two pitches that alternate every few
    // frames are two notes, not one chord — the stress song's triangle
    // rocks between E-2 and E-3 once a row, and calling that an arpeggio
    // would rewrite its bassline as a single droning chord.
    let longest_run = {
        let mut best = 0usize;
        let mut run = 0usize;
        let mut prev: Option<u8> = None;
        for n in &all {
            run = if Some(*n) == prev { run + 1 } else { 1 };
            prev = Some(*n);
            best = best.max(run);
        }
        best
    };
    if (2..=3).contains(&distinct.len()) && visited.len() >= 4 && longest_run <= 2 {
        // The root is the lowest pitch, which is the note the tracker wrote;
        // the others are its offsets above it.
        let root = *distinct.first().unwrap();
        let off = |i: usize| distinct.get(i).map(|&n| n.saturating_sub(root).min(15)).unwrap_or(0);
        return Motion::Arpeggio { x: off(1), y: off(2) };
    }

    // Slide: the pitch walks one way and stays there. The first step is the
    // speed, since the driver moves a fixed number of units per frame until
    // it arrives.
    let first = (periods[1] as i32 - periods[0] as i32).unsigned_abs();
    let ends_elsewhere = note_of(periods[0]) != note_of(*periods.last().unwrap());
    if ends_elsewhere && first > 0 {
        let monotone = periods.windows(2).take(4).all(|w| w[1] <= w[0]) || periods.windows(2).take(4).all(|w| w[1] >= w[0]);
        if monotone {
            return Motion::Slide { speed: first.min(255) as u8 };
        }
    }

    // Vibrato: a wobble narrower than a semitone, repeating. Depth and rate
    // both fall out of the shape.
    if hi - lo < 32 {
        let span = (hi - lo) as u8;
        // A cycle is one return to the lowest period — the unmodulated pitch,
        // since this driver bends only upward in period.
        let mut cycle = 0usize;
        let mut last: Option<usize> = None;
        for (i, &p) in periods.iter().enumerate() {
            if p == lo {
                if let Some(prev) = last {
                    if i - prev > 1 {
                        cycle = i - prev;
                        break;
                    }
                }
                last = Some(i);
            }
        }
        if cycle > 0 {
            let rate = (32 / cycle).clamp(1, 15) as u8;
            return Motion::Vibrato { depth: (span + 1).min(15), rate };
        }
    }
    Motion::Steady
}

impl Motion {
    /// The effect column this motion writes, if any.
    fn fx(self) -> Option<(u8, u8)> {
        match self {
            Motion::Steady => None,
            Motion::Vibrato { depth, rate } => Some((b'V', (depth << 4) | rate)),
            Motion::Arpeggio { x, y } => Some((b'A', (x << 4) | y)),
            Motion::Slide { speed } => Some((b'S', speed)),
        }
    }
    /// The letter whose "off" form has to be written when this stops.
    fn off(self) -> Option<(u8, u8)> {
        match self {
            Motion::Steady => None,
            Motion::Vibrato { .. } => Some((b'V', 0)),
            Motion::Arpeggio { .. } => Some((b'A', 0)),
            Motion::Slide { .. } => Some((b'S', 0)),
        }
    }
}

/// The periods sounded inside one row. Empty for a row that does not exist,
/// so callers can look at a neighbour without checking the edges first.
fn row_periods(t: &[FrameTrace], c: usize, starts: &[u32], r: usize) -> Vec<u16> {
    let Some(&from) = starts.get(r) else { return Vec::new() };
    let to = starts.get(r + 1).copied().unwrap_or(u32::MAX);
    t.iter()
        .filter(|f| f.frame >= from && f.frame < to && f.ch[c].level > 0)
        .map(|f| f.ch[c].period)
        .collect()
}

/// The periods one note played while it was still *that* note.
///
/// Bounded twice, and both bounds matter. Thirty-two frames is one cycle of
/// the slowest vibrato the format can express, so nothing is gained by
/// looking further. And the window closes as soon as the pitch has moved
/// somewhere else and stayed there for two frames: a note that is later slid
/// or arpeggiated away from is still a plain note where it starts, and
/// reading its whole life at once turns a vibrato into a slide.
///
/// Two frames rather than one, because an arpeggio visits other pitches for
/// exactly one frame at a time and must not close the window.
fn periods_played(t: &[FrameTrace], c: usize, from: u32) -> Vec<u16> {
    let note_of = |p: u16| if c == TRI { tri_note(p) } else { pulse_note(p) };
    let mut out: Vec<u16> = Vec::new();
    let mut home: Option<u8> = None;
    let mut away = 0;
    for f in t.iter().skip_while(|f| f.frame < from).take(32) {
        if f.frame > from && f.ch[c].keyed && f.ch[c].level > 0 {
            break;
        }
        if f.ch[c].level == 0 {
            break;
        }
        let n = note_of(f.ch[c].period);
        home = home.or(n);
        if n != home {
            away += 1;
            if away >= 2 {
                out.truncate(out.len().saturating_sub(1));
                break;
            }
        } else {
            away = 0;
        }
        out.push(f.ch[c].period);
    }
    out
}

/// The levels one note played: from its key-on until the channel is keyed
/// again or falls silent.
///
/// Capped, because a note held under a long rest would otherwise drag the
/// whole remaining trace into one fingerprint. Two seconds is longer than
/// any envelope the format can express.
fn played(t: &[FrameTrace], c: usize, from: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for f in t.iter().skip_while(|f| f.frame < from).take(120) {
        if f.frame > from && f.ch[c].keyed && f.ch[c].level > 0 {
            break;
        }
        out.push(f.ch[c].level);
        if f.ch[c].level == 0 {
            break;
        }
    }
    out
}

/// Group the notes by the envelope they played and turn each group into an
/// instrument, returning the instruments and which one each note uses.
///
/// Grouping is on the *shape*, normalised to its own peak, so two notes of
/// the same voice at different volumes stay together — loudness belongs in
/// the volume column, not in a second instrument. The longest note in a
/// group is the one read for the ADSR, because a note cut short by the next
/// key-on never reveals its release, and in fast music most of them are.
fn synth_instruments(placed: &[(usize, usize, NoteEnv)]) -> (Vec<Instrument>, Vec<usize>) {
    // Shape key: the channel, plus the first few frames scaled to the peak.
    // The channel is part of it because one instrument cannot serve a pulse
    // and the triangle — their duties and pitch tables differ.
    // Three frames, padded — deliberately short and a fixed length. In fast
    // music most notes are cut off by the next key-on long before their
    // release, so a key that grew with the note would file the same voice
    // under a different instrument depending on how long it happened to
    // sound. Three frames reach the peak and the start of the decay, which
    // is what distinguishes one timbre from another, and every note has
    // them.
    const SHAPE: usize = 3;
    // Eight buckets, rounded. A quiet note carries less of its own shape than
    // a loud one — four levels can only say so much about a curve — so
    // comparing at full resolution splits one voice in two on the strength of
    // rounding alone: 9 of 10 and 4 of 5 are the same envelope and do not
    // look it. Eight buckets absorb that and still keep a plucked note apart
    // from a held one.
    const BUCKETS: u16 = 7;
    let key = |n: &NoteEnv| -> (usize, Vec<u8>) {
        let peak = n.levels.iter().copied().max().unwrap_or(0).max(1) as u16;
        let mut shape: Vec<u8> = n
            .levels
            .iter()
            .take(SHAPE)
            .map(|&l| ((l as u16 * BUCKETS + peak / 2) / peak) as u8)
            .collect();
        let last = shape.last().copied().unwrap_or(0);
        shape.resize(SHAPE, last);
        (n.ch, shape)
    };
    let mut groups: Vec<((usize, Vec<u8>), Vec<usize>)> = Vec::new();
    let mut assign = vec![0usize; placed.len()];
    for (i, (_, _, n)) in placed.iter().enumerate() {
        let k = key(n);
        match groups.iter().position(|(g, _)| *g == k) {
            Some(g) => groups[g].1.push(i),
            None => groups.push((k, vec![i])),
        }
    }
    // More voices than the table holds: keep the busiest and fold the rest
    // into the nearest survivor on the same channel.
    if groups.len() > crate::INSTRUMENTS {
        groups.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        let spare: Vec<((usize, Vec<u8>), Vec<usize>)> = groups.split_off(crate::INSTRUMENTS);
        for (k, members) in spare {
            let home = groups
                .iter()
                .position(|(g, _)| g.0 == k.0)
                .unwrap_or(0);
            groups[home].1.extend(members);
        }
    }

    let mut instruments = Vec::new();
    for (gi, (_, members)) in groups.iter().enumerate() {
        // The loudest note in the group is taken to have played at full
        // column volume; every quieter one then scales down through its own
        // volume column. Some such convention is needed, because an observed
        // level is a product of two numbers and only their product is
        // visible.
        let longest = members.iter().copied().max_by_key(|&i| placed[i].2.levels.len()).unwrap_or(0);
        let loudest = members
            .iter()
            .map(|&i| placed[i].2.levels.iter().copied().max().unwrap_or(0))
            .max()
            .unwrap_or(15);
        let levels = &placed[longest].2.levels;
        let scaled: Vec<u8> = levels
            .iter()
            .map(|&l| ((l as u16 * loudest as u16) / (levels.iter().copied().max().unwrap_or(1).max(1) as u16)) as u8)
            .collect();
        let adsr = read_adsr(&scaled, 15).unwrap_or(Adsr { attack: 0, decay: 0, release: 0, peak: 15, sustain: 15 });
        instruments.push(adsr.to_instrument(duty_of(&placed[longest].2)));
        for &m in members {
            assign[m] = gi;
        }
    }
    if instruments.is_empty() {
        instruments.push(Instrument::default());
    }
    // Two groups that produced the same instrument are the same instrument.
    // The shape key is a proxy, and a coarse one; this is the check that
    // matters, and it keeps a song from spending its sixteen slots on
    // duplicates of one voice.
    let mut unique: Vec<Instrument> = Vec::new();
    let mut remap: Vec<usize> = Vec::with_capacity(instruments.len());
    for inst in &instruments {
        let same = |a: &Instrument, b: &Instrument| {
            a.attack_ms == b.attack_ms
                && a.decay_ms == b.decay_ms
                && a.release_ms == b.release_ms
                && (a.sustain - b.sustain).abs() < 1e-3
                && (a.duty - b.duty).abs() < 1e-3
                && (a.volume - b.volume).abs() < 1e-3
        };
        match unique.iter().position(|u| same(u, inst)) {
            Some(i) => remap.push(i),
            None => {
                unique.push(*inst);
                remap.push(unique.len() - 1);
            }
        }
    }
    for a in assign.iter_mut() {
        *a = remap[*a];
    }
    (unique, assign)
}

/// The duty a note sounded with, as the fraction an `@instr` line carries.
///
/// Only the pulses have one. `compile::nes_duty` quantises back to the same
/// two bits, so these four values survive the round trip exactly; the
/// triangle, noise and DPCM take the middle setting, which their channels
/// ignore.
fn duty_of(n: &NoteEnv) -> f32 {
    match n.ch {
        PU1 | PU2 => [0.125, 0.25, 0.5, 0.75][n.duty.min(3) as usize],
        _ => 0.5,
    }
}

/// Frames back to the milliseconds an `@instr` line carries.
fn ms(frames: usize) -> u16 {
    (frames as f64 * 1000.0 / 60.0988).round() as u16
}

impl Adsr {
    fn to_instrument(self, duty: f32) -> Instrument {
        Instrument {
            attack_ms: ms(self.attack),
            decay_ms: ms(self.decay),
            sustain: self.sustain as f32 / self.peak.max(1) as f32,
            release_ms: ms(self.release),
            duty,
            volume: self.peak as f32 / 15.0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub source: String,
    pub frames: usize,
    pub loop_frames: Option<(u32, u32)>,
    pub bpm: u16,
    pub frames_per_row: f64,
    pub fit: Option<Fit>,
    pub rows: usize,
    pub phrases_total: usize,
    pub phrases_unique: usize,
    /// Instruments synthesised from the envelopes the notes played.
    pub instruments: usize,
    /// Notes carrying a recovered effect column.
    pub effects: usize,
    /// Notes found only because the pitch moved without a key-on.
    pub slide_targets: usize,
    /// Audible notes written, per channel.
    pub notes: [usize; CHANNELS],
    /// Key-ons dropped because the channel had no volume at the time.
    pub silent_keyons: usize,
    /// Notes whose period was outside the range viper can name.
    pub unpitched: usize,
    pub holds: usize,
    /// Notes dropped because another already held that row on that channel.
    pub row_collisions: usize,
    /// Row count the song was folded to, when its rows turned out to repeat.
    pub folded_at: Option<usize>,
    /// Frames on which a hardware sweep was moving a pitch.
    pub sweep_frames: usize,
    /// Frames whose level came from a simulated hardware envelope.
    pub hw_env_frames: usize,
    /// `$4012` values seen, in order of first use.
    pub dpcm_samples: Vec<u8>,
}

impl Report {
    pub fn summary(&self) -> String {
        const NAMES: [&str; CHANNELS] = ["PU1", "PU2", "TRI", "NOI", "DPCM"];
        let mut s = format!(
            "source   {}, {} frames ({:.2}s)\n",
            self.source,
            self.frames,
            self.frames as f64 / 60.0988
        );
        match self.loop_frames {
            // The period is trustworthy; the start is not. RAM hashing finds
            // the first frame whose driver state repeats, which can sit a
            // whole pass after the musical loop point.
            Some((start, len)) => s.push_str(&format!(
                "loop     {} frames, repeating from frame {} (driver RAM state; the start can overshoot)\n",
                len, start
            )),
            None => s.push_str("loop     not detected\n"),
        }
        if let Some(fit) = self.fit {
            let how = match fit {
                Fit::Told => "TOLD".to_string(),
                Fit::Exact { ambiguous: None } => "INFERRED, exact: every onset lands on a row start".to_string(),
                Fit::Exact { ambiguous: Some((lo, hi)) } => format!(
                    "INFERRED, exact: every onset lands on a row start, but {}–{} BPM fit equally — \
                     this song is too short to tell them apart",
                    lo, hi
                ),
                Fit::Fitted { worst_rows, within } => format!(
                    "INFERRED, fitted: {:.0}% of onsets within {:.2} rows, worst {:.2}",
                    within * 100.0,
                    TOL_ROWS,
                    worst_rows
                ),
                Fit::Guessed { worst_rows, collisions } => format!(
                    "INFERRED, GUESSED: worst {:.2} rows, {} onsets collided on a row{}",
                    worst_rows,
                    collisions,
                    if collisions > 0 { " — the grid may be too coarse, try --bpm" } else { "" }
                ),
            };
            s.push_str(&format!("tempo    {} BPM, {:.2} frames/row — {}\n", self.bpm, self.frames_per_row, how));
        }
        s.push_str(&format!(
            "rows     {} → {} phrases ({} unique)\n",
            self.rows, self.phrases_total, self.phrases_unique
        ));
        if let Some(p) = self.folded_at {
            s.push_str(&format!("         the rows repeat every {}, so the trailing copy was folded away\n", p));
        }
        s.push_str("notes    ");
        for c in 0..CHANNELS {
            s.push_str(&format!("{} {:<5}", NAMES[c], self.notes[c]));
        }
        s.push_str(&format!("holds {}\n", self.holds));
        if self.silent_keyons > 0 {
            s.push_str(&format!(
                "         {} key-on{} suppressed (no volume at onset)\n",
                self.silent_keyons,
                if self.silent_keyons == 1 { "" } else { "s" }
            ));
        }
        if self.row_collisions > 0 {
            s.push_str(&format!(
                "         {} notes dropped: two onsets wanted the same row — the grid may be too coarse\n",
                self.row_collisions
            ));
        }
        if self.unpitched > 0 {
            s.push_str(&format!("         {} onsets had no nameable pitch and were dropped\n", self.unpitched));
        }
        s.push_str(&format!(
            "instr    {} synthesised from the envelopes the notes played\n",
            self.instruments
        ));
        if self.effects > 0 || self.slide_targets > 0 {
            s.push_str(&format!("fx       {} notes carry a recovered effect", self.effects));
            if self.slide_targets > 0 {
                s.push_str(&format!(", {} found only because the pitch moved with no key-on", self.slide_targets));
            }
            s.push('\n');
        }
        if !self.dpcm_samples.is_empty() {
            s.push_str(&format!("dpcm     {} sample(s): {:02X?}\n", self.dpcm_samples.len(), self.dpcm_samples));
        }
        // Everything viper cannot express, counted rather than dropped in
        // silence. A rip that quietly loses a feature is worse than one that
        // says it did.
        if self.sweep_frames > 0 {
            s.push_str(&format!("lost     hardware sweep active on {} frames; viper has no sweep\n", self.sweep_frames));
        }
        if self.hw_env_frames > 0 {
            s.push_str(&format!(
                "approx   hardware envelopes on {} frames; those levels were simulated, not read\n",
                self.hw_env_frames
            ));
        }
        if !self.dpcm_samples.is_empty() {
            s.push_str("         the samples themselves are not extracted, so the default bank is used\n");
        }
        s.push_str("lost     instrument identity, phrase boundaries, groove; noise pitch is 4:1\n");
        s
    }
}

/// Turn a frame table into a song.
pub fn rip(t: &[FrameTrace], opts: &RipOptions) -> Result<(Song, Report, Grid)> {
    if t.is_empty() {
        bail!("rip: nothing to transcribe");
    }
    let grid = fit_grid(t, opts.bpm);
    if grid.starts.is_empty() {
        bail!("rip: no rows — the trace has no audible note starts");
    }
    let mut report = Report {
        frames: t.len(),
        bpm: grid.bpm,
        frames_per_row: grid.frames_per_row,
        fit: Some(grid.fit),
        ..Default::default()
    };

    let n_rows = grid.starts.len();
    let mut rows = vec![[Cell::default(); CHANNELS]; n_rows];
    let mut dpcm_slots: Vec<u8> = Vec::new();
    // Where each note landed, so its envelope can be turned into an
    // instrument once every note has been seen.
    let mut placed: Vec<(usize, usize, NoteEnv)> = Vec::new();

    // Notes first, each placed on the row it is *nearest* to rather than the
    // row whose frame window it happens to land in.
    //
    // The difference is not pedantry. A driver's row clock is a fixed-point
    // accumulator whose fractional phase is invisible from outside, and
    // viper's own resets it somewhere across a loop, so a reconstructed grid
    // can sit a frame away from the real one. Under a window rule that frame
    // pushes a note into the neighbouring row and the same bar transcribes
    // two different ways on two passes. Nearest-row assignment absorbs it,
    // and does the right thing for a foreign driver whose quirks are not
    // known at all.
    for f in t {
        for c in 0..CHANNELS {
            let cf = f.ch[c];
            if !cf.keyed || cf.level == 0 {
                continue;
            }
            let r = nearest_row(&grid.starts, f.frame);
            let note = match c {
                PU1 | PU2 => pulse_note(cf.period),
                TRI => tri_note(cf.period),
                NOI => Some(noise_note(cf.period)),
                _ => {
                    // DPCM notes address a sample slot, not a pitch:
                    // `dpcm::note_to_sample` is note - 60.
                    let s = f.dpcm.unwrap_or(0);
                    let slot = dpcm_slots.iter().position(|&x| x == s).unwrap_or_else(|| {
                        dpcm_slots.push(s);
                        dpcm_slots.len() - 1
                    });
                    Some(60u8.saturating_add(slot.min(67) as u8))
                }
            };
            let Some(n) = note else {
                report.unpitched += 1;
                continue;
            };
            if rows[r][c].note.is_some() {
                // Two notes, one row: the grid is too coarse to hold this
                // music. Keep the first and say how much was lost.
                report.row_collisions += 1;
                continue;
            }
            let motion = classify(&periods_played(t, c, f.frame), c);
            rows[r][c] = Cell { note: Some(n), instr: 0, vol: cell_volume(cf.level), fx: motion.fx(), hold: false };
            report.notes[c] += 1;
            if motion != Motion::Steady {
                report.effects += 1;
            }
            placed.push((r, c, NoteEnv { ch: c, duty: cf.duty, levels: played(t, c, f.frame) }));
        }
    }

    // Slide targets. Portamento moves a channel to a new pitch without
    // retriggering it, so there is no key-on to notice and the note it slides
    // *to* would otherwise be lost entirely — the transcription would hold
    // the old pitch through a passage that audibly moves. A pitch that
    // changes with no key-on is that, and the row it arrives on is where the
    // tracker wrote the target note.
    for c in [PU1, PU2, TRI] {
        let mut sounding: Option<u8> = None;
        let mut active: Option<(u8, u8)> = None;
        for r in 0..n_rows {
            if let Some(n) = rows[r][c].note {
                sounding = Some(n);
                active = rows[r][c].fx;
                continue;
            }
            let Some(&from) = grid.starts.get(r) else { continue };
            let ps = row_periods(t, c, &grid.starts, r);
            if ps.len() < 2 {
                continue;
            }
            // Two rows for the classification, one for the answer. An
            // arpeggio needs a couple of full cycles before its three pitches
            // outnumber the frames caught between them.
            let mut wide = ps.clone();
            wide.extend(row_periods(t, c, &grid.starts, r + 1));
            let note_of = |p: u16| if c == TRI { tri_note(p) } else { pulse_note(p) };
            let level = t.iter().find(|f| f.frame == from).map(|f| f.ch[c].level).unwrap_or(0);
            if level == 0 || sounding.is_none() {
                continue;
            }
            // What this row did, judged on this row alone. A whole note's
            // worth of frames would run past the next effect.
            let (note, fx) = match classify(&wide, c) {
                // An arpeggio is written on its root, not on whichever of
                // its three pitches the row happened to end on.
                Motion::Arpeggio { x, y } => {
                    let root = ps.iter().filter_map(|&p| note_of(p)).min();
                    match root {
                        Some(n) => (n, Some((b'A', (x << 4) | y))),
                        None => continue,
                    }
                }
                _ => {
                    let Some(now) = note_of(*ps.last().unwrap()) else { continue };
                    if sounding == Some(now) {
                        continue;
                    }
                    // Portamento moves a fixed number of period units per
                    // frame, so its first step is its speed — measured from
                    // where the pitch was at the end of the row before, since
                    // by this row's first frame it has already moved once.
                    let prev = row_periods(t, c, &grid.starts, r.wrapping_sub(1));
                    let before = prev.last().copied().unwrap_or(ps[0]);
                    let speed = (ps[0] as i32 - before as i32).unsigned_abs().min(255) as u8;
                    (now, Some((b'S', speed)))
                }
            };
            // Nothing has changed: the effect is still running on the note
            // it started on, and a tracker writes that once. Repeating it
            // every row would turn one arpeggiated chord into a wall of
            // retriggers the original never played.
            if sounding == Some(note) && fx == active {
                continue;
            }
            active = fx;
            rows[r][c] = Cell { note: Some(note), instr: 0, vol: cell_volume(level), fx, hold: false };
            report.notes[c] += 1;
            report.slide_targets += 1;
            sounding = Some(note);
            placed.push((r, c, NoteEnv { ch: c, duty: 2, levels: played(t, c, from) }));
        }
    }

    // Effects persist on a channel until they are changed, so a plain note
    // after a modulated one has to say so. Without the off-form it inherits
    // the vibrato of the note before it and the transcription wobbles where
    // the original was still.
    for c in 0..CHANNELS {
        let mut active: Option<(u8, u8)> = None;
        for r in 0..n_rows {
            if rows[r][c].note.is_none() {
                continue;
            }
            match (active, rows[r][c].fx) {
                (Some(prev), None) => {
                    rows[r][c].fx = Motion::Steady.off().or(Some((prev.0, 0)));
                    active = None;
                }
                (_, Some(f)) => active = if f.1 == 0 { None } else { Some(f) },
                (None, None) => {}
            }
        }
    }

    // Then sustains. A row with no note of its own either holds the previous
    // one or ends it, and an empty cell is exactly how the format says ended.
    for c in 0..CHANNELS {
        // DPCM notes are one-shot triggers; a sample is not "held".
        if c == crate::rip_dmc() {
            continue;
        }
        let mut sounding = false;
        for r in 0..n_rows {
            if rows[r][c].note.is_some() {
                sounding = true;
            } else if sounding {
                if spans_row(t, c, &grid.starts, r) {
                    rows[r][c] = Cell::hold();
                    report.holds += 1;
                } else {
                    sounding = false;
                }
            }
        }
    }

    // Trailing empty rows are the render's tail, not music.
    while rows.len() > STEPS_PER_PHRASE && rows.last().is_some_and(|r| r.iter().all(|c| c.note.is_none() && !c.hold)) {
        rows.truncate(rows.len() - 1);
    }
    report.folded_at = fold_repeats(&mut rows);

    let counted: usize = t
        .iter()
        .map(|f| f.ch.iter().filter(|c| c.keyed && c.level == 0).count())
        .sum();
    report.silent_keyons = counted;
    report.sweep_frames = t.iter().filter(|f| f.ch.iter().any(|c| c.sweep != 0)).count();
    report.hw_env_frames = t.iter().filter(|f| f.ch.iter().take(4).any(|c| !c.constant_vol && c.level > 0)).count();
    report.dpcm_samples = dpcm_slots;
    report.rows = rows.len();

    // Instruments, from the envelopes the notes actually played. This has to
    // happen before the rows become phrases: an instrument's peak decides
    // every cell's volume column, and two rows that differ only there are
    // not the same phrase.
    let (instruments, assign) = synth_instruments(&placed);
    report.instruments = instruments.len();
    for (k, &(r, c, _)) in placed.iter().enumerate() {
        let idx = assign[k];
        let peak = (instruments[idx].volume * 15.0).round().max(1.0) as u16;
        let level = placed[k].2.levels.first().copied().unwrap_or(0) as u16;
        // Folding may have dropped this row; only touch one that still holds
        // the note this envelope came from.
        if let Some(row) = rows.get_mut(r) {
            if row[c].note.is_some() {
                row[c].instr = idx as u8;
                row[c].vol = ((16 * level).div_ceil(peak)).clamp(1, 15) as u8;
            }
        }
    }

    let (phrases, order) = crate::phrases_from_rows(&rows)?;
    report.phrases_total = order.len();
    report.phrases_unique = phrases.len();

    let mut song = Song::default();
    song.bpm = grid.bpm;
    song.phrases = phrases;
    song.order = order;
    song.loop_pos = 0;
    song.current_phrase = song.order.first().copied().unwrap_or(0);
    for (i, inst) in instruments.iter().enumerate() {
        song.instruments[i] = *inst;
    }
    Ok((song, report, grid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use viper_apu::trace::ChannelFrame;

    /// A trace of `n` frames with a key-on and level on one channel at the
    /// given frames.
    fn keyed(n: u32, c: usize, at: &[u32], period: u16) -> Vec<FrameTrace> {
        (0..n)
            .map(|frame| {
                let mut f = FrameTrace { frame, ..Default::default() };
                f.ch[c] = ChannelFrame {
                    period,
                    level: 10,
                    keyed: at.contains(&frame),
                    constant_vol: true,
                    ..Default::default()
                };
                f
            })
            .collect()
    }

    #[test]
    fn the_row_clock_is_an_accumulator_not_a_straight_line() {
        // 220 BPM is 4.09 frames per row, which the driver's 8.8 fixed point
        // renders as eleven 4-frame rows and then a 5.
        let s = row_starts(900.0 / 220.0, 1, 60, 0);
        assert_eq!(&s[..15], &[1, 5, 9, 13, 17, 21, 25, 29, 33, 37, 41, 45, 50, 54, 58]);
    }

    /// Onsets a driver at `bpm` would actually produce, one per `every`
    /// rows, over `frames`. Real length matters: a handful of onsets leaves
    /// several tempos indistinguishable, so a test built on four bars proves
    /// less than it looks like it does.
    fn played_at(bpm: f64, every: usize, frames: u32) -> Vec<u32> {
        row_starts(900.0 / bpm, 1, frames, 0).into_iter().step_by(every).collect()
    }

    #[test]
    fn the_grid_is_the_coarsest_that_fits_not_the_best_fitting() {
        // These onsets are consistent with 4.09 frames per row — and equally
        // with 2.05, and perfectly with 1. Minimising error finds one frame
        // per row every time, which is why the rule is "largest that fits".
        let ons = played_at(220.0, 1, 200);
        let t = keyed(202, PU1, &ons, 169);
        let g = fit_grid(&t, None);
        assert_eq!(g.bpm, 220);
        assert!(g.frames_per_row > 4.0, "not a degenerate one-frame grid: {}", g.frames_per_row);
        assert_eq!(g.fit, Fit::Exact { ambiguous: None }, "a full song's worth of onsets pins the tempo");
    }

    #[test]
    fn a_half_speed_song_is_not_read_as_a_double_speed_one() {
        // Every other row empty: the onsets fit 8.18 frames per row, and the
        // coarser reading is the right one.
        let ons = played_at(220.0, 2, 400);
        let t = keyed(402, PU1, &ons, 169);
        assert_eq!(fit_grid(&t, None).bpm, 110, "not 220 with every other row blank");
    }

    #[test]
    fn a_tempo_the_evidence_cannot_pin_is_reported_as_ambiguous() {
        // Sixteen rows exactly six frames apart are equally good evidence for
        // 149, 150 and 151 BPM. The middle is the best estimate, and the
        // report has to admit the other two rather than sound certain.
        let ons: Vec<u32> = (0..16).map(|i| 1 + i * 6).collect();
        let g = fit_grid(&keyed(100, PU1, &ons, 169), None);
        assert_eq!(g.bpm, 150, "the middle of the tied range, not an edge of it");
        assert_eq!(g.fit, Fit::Exact { ambiguous: Some((149, 151)) });
    }

    #[test]
    fn a_told_tempo_is_never_reported_as_inferred() {
        let t = keyed(120, PU1, &[1, 5, 9, 13], 169);
        assert_eq!(fit_grid(&t, Some(150)).fit, Fit::Told);
        assert_eq!(fit_grid(&t, Some(150)).bpm, 150);
    }

    #[test]
    fn periods_invert_to_the_notes_that_produced_them() {
        // The stress song's own first notes, from the golden log.
        assert_eq!(pulse_note(169), Some(76), "E-5");
        assert_eq!(pulse_note(213), Some(72), "C-5");
        assert_eq!(tri_note(678), Some(40), "E-2");
        // A period the hardware mutes is not given a pitch.
        assert_eq!(pulse_note(4), None);
    }

    #[test]
    fn every_pulse_note_survives_the_round_trip_through_a_period() {
        // The driver's table is note - 24, and the emitter clamps below 33
        // because an 11-bit period cannot reach lower.
        for n in 33u8..=107 {
            let hz = 440.0 * 2f64.powf((n as f64 - 69.0) / 12.0);
            let p = (CPU_HZ / (16.0 * hz) - 1.0).round() as u16;
            assert_eq!(pulse_note(p), Some(n), "period {} for note {}", p, n);
        }
    }

    #[test]
    fn noise_notes_round_trip_through_the_period_index() {
        for idx in 0u16..=15 {
            let n = noise_note(idx);
            assert_eq!(crate::compile::noise_period_index(n) as u16, idx, "index {} -> note {}", idx, n);
        }
    }

    #[test]
    fn the_volume_column_is_inverted_rather_than_copied() {
        // Measured against the reference driver: it plays (vol * 15) >> 4.
        for level in 1u8..=14 {
            let v = cell_volume(level);
            assert_eq!((v as u16 * 15) >> 4, level as u16, "level {} needs volume {}", level, v);
        }
        // Never the "channel default" sentinel, and never past the column.
        assert!((1..=15).contains(&cell_volume(0)));
        assert_eq!(cell_volume(15), 15, "the one level out of reach clamps rather than overflowing");
    }

    #[test]
    fn a_release_tail_is_not_mistaken_for_a_held_note() {
        let mut t: Vec<FrameTrace> = (0..16).map(|frame| FrameTrace { frame, ..Default::default() }).collect();
        // 10 9 9 9 | 7 4 2 0 | 0 ... — the driver's real envelope after the
        // held-note fix: a plateau, then one release, inside rows of four.
        for (i, l) in [10u8, 9, 9, 9, 7, 4, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0].iter().enumerate() {
            t[i].ch[PU1].level = *l;
        }
        let starts = [0u32, 4, 8, 12];
        assert!(!spans_row(&t, PU1, &starts, 1), "the release does not reach row 2, so row 1 ended the note");
        assert!(!spans_row(&t, PU1, &starts, 2), "and row 2 is silent throughout");
        // One frame of grid error must not change the answer.
        let shifted = [1u32, 5, 9, 13];
        assert!(!spans_row(&t, PU1, &shifted, 1));

        // A note that really is held reaches the next row at full level.
        for f in t.iter_mut() {
            f.ch[PU1].level = 9;
        }
        assert!(spans_row(&t, PU1, &starts, 1));
        assert!(!spans_row(&t, PU1, &starts, 3), "the last row has no next row to reach");
    }

    #[test]
    fn a_song_held_twice_is_folded_back_to_once() {
        let note = |n: u8| {
            let mut r = [Cell::default(); CHANNELS];
            r[0] = Cell { note: Some(n), ..Default::default() };
            r
        };
        let mut rows: Vec<[Cell; CHANNELS]> = (0..16).map(|i| note(60 + i)).collect();
        rows.extend(rows.clone());
        assert_eq!(fold_repeats(&mut rows), Some(16));
        assert_eq!(rows.len(), 16);
        // Music that merely resembles itself is left alone.
        let mut once: Vec<[Cell; CHANNELS]> = (0..32).map(|i| note(60 + i)).collect();
        assert_eq!(fold_repeats(&mut once), None);
        assert_eq!(once.len(), 32);
    }

    #[test]
    fn a_key_on_with_no_volume_never_becomes_a_note() {
        let mut t = keyed(40, PU1, &[1, 5, 9], 169);
        t[5].ch[PU1].level = 0;
        let (song, report, _) = rip(&t, &RipOptions::default()).unwrap();
        assert_eq!(report.notes[PU1], 2, "the silent key-on is not music");
        assert_eq!(report.silent_keyons, 1);
        let written: usize = song.phrases.iter().map(|p| p.cells.iter().filter(|r| r[PU1].note.is_some()).count()).sum();
        assert_eq!(written, 2, "and it reaches no cell");
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::*;

    #[test]
    fn an_observed_note_gives_back_the_envelope_that_played_it() {
        // The stress song's lead, exactly as the golden log records it. At
        // full column volume the driver plays (vol * env) >> 4, so these
        // levels are the envelope 11 10 10 10 8 5 3 0 — which is an instant
        // attack, one frame of decay, a sustain at 10 of 11, and a release
        // over four frames. Nothing here is fitted.
        let a = read_adsr(&[10, 9, 9, 9, 7, 4, 2, 0], 15).unwrap();
        assert_eq!(a, Adsr { attack: 0, decay: 1, release: 4, peak: 11, sustain: 10 });

        let i = a.to_instrument(0.25);
        assert_eq!(i.attack_ms, 0);
        assert_eq!(i.release_ms, 67, "four frames");
        assert!((i.sustain - 10.0 / 11.0).abs() < 0.01);
        assert!((i.volume - 11.0 / 15.0).abs() < 0.01, "the source asked for 0.70");
    }

    #[test]
    fn a_note_cut_short_reports_no_release_rather_than_a_wrong_one() {
        // Most notes in fast music are stopped by the next key-on long
        // before they release. Such a note knows its attack and decay and
        // nothing about its tail, and must not invent one.
        let a = read_adsr(&[10, 9, 9, 9], 15).unwrap();
        assert_eq!(a.release, 0);
        assert_eq!(a.peak, 11);
        assert_eq!(a.sustain, 10);
    }

    #[test]
    fn a_rising_attack_is_measured_not_assumed() {
        let a = read_adsr(&[3, 6, 9, 14, 14, 14], 15).unwrap();
        assert_eq!(a.attack, 3, "three frames before the peak");
        assert_eq!(a.peak, 15);
    }

    #[test]
    fn silence_is_not_an_envelope() {
        assert!(read_adsr(&[0, 0, 0], 15).is_none());
        assert!(read_adsr(&[], 15).is_none());
    }

    #[test]
    fn the_same_voice_at_two_volumes_is_one_instrument() {
        // Loudness belongs in the volume column. A quiet note and a loud one
        // with the same shape must not become two instruments, or a song
        // with any dynamics burns through the sixteen slots.
        // The same curve at two volumes, as the chip would have rounded it.
        let loud = NoteEnv { ch: PU1, duty: 1, levels: vec![10, 9, 9, 9, 7, 4, 2, 0] };
        let soft = NoteEnv { ch: PU1, duty: 1, levels: vec![5, 4, 4, 4] };
        let placed = vec![(0, PU1, loud), (4, PU1, soft)];
        let (instr, assign) = synth_instruments(&placed);
        assert_eq!(instr.len(), 1, "one voice, two volumes");
        assert_eq!(assign, vec![0, 0]);
        // And it is the loud one that sets the instrument's peak, so the
        // quiet note can scale down through its own column.
        assert!((instr[0].volume - 11.0 / 15.0).abs() < 0.01);
    }

    #[test]
    fn two_different_shapes_stay_apart() {
        let plucked = NoteEnv { ch: PU1, duty: 1, levels: vec![15, 8, 4, 1, 0] };
        let held = NoteEnv { ch: PU1, duty: 1, levels: vec![15, 15, 15, 15, 15] };
        let (instr, assign) = synth_instruments(&vec![(0, PU1, plucked), (4, PU1, held)]);
        assert_eq!(instr.len(), 2);
        assert_ne!(assign[0], assign[1]);
    }

    #[test]
    fn a_pulse_duty_survives_the_round_trip() {
        for (bits, frac) in [(0u8, 0.125f32), (1, 0.25), (2, 0.5), (3, 0.75)] {
            let n = NoteEnv { ch: PU1, duty: bits, levels: vec![15, 15] };
            let (instr, _) = synth_instruments(&vec![(0, PU1, n)]);
            assert!((instr[0].duty - frac).abs() < 1e-6);
            assert_eq!(crate::compile::nes_duty(instr[0].duty), bits, "and quantises back");
        }
    }
}
