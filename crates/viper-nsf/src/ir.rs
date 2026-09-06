//! The intermediate representation the emitter consumes.

/// The five 2A03 channels, in driver order. VRC6 channels would extend
/// this table; the `expansion` flag on [`Module`] gates their use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    Pu1 = 0,
    Pu2 = 1,
    Tri = 2,
    Noi = 3,
    Dpcm = 4,
}

pub const CHANNELS: [Channel; 5] = [Channel::Pu1, Channel::Pu2, Channel::Tri, Channel::Noi, Channel::Dpcm];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expansion {
    None,
    Vrc6,
}

impl Expansion {
    pub fn nsf_bits(self) -> u8 {
        match self {
            Expansion::None => 0,
            Expansion::Vrc6 => 0x01,
        }
    }
}

/// One thing that happens on a channel at a row. Order within a row
/// matters: state changes should precede the `Note`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Pulse/TRI: period-table index (MIDI − 24, TRI +12). NOI: period
    /// index 0–15. DPCM: sample index.
    Note(u8),
    Off,
    Vol(u8),
    Duty(u8),
    Instr(u8),
    Retrig(u8),
    Slide(u8),
    Vibrato { depth: u8, rate: u8 },
    Arp { x: u8, y: u8 },
    EnvReset,
    /// Set the row clock from this row on: 8.8 fixed-point frames per row.
    /// Song-global; the lowering emits it on one channel only.
    Speed(u16),
}

/// A volume envelope: one value per frame, with optional loop and
/// release indices (see ABI.md).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub values: Vec<u8>,
    pub loop_point: Option<u8>,
    pub release_point: Option<u8>,
}

impl Envelope {
    /// Build a frame envelope from ADSR in milliseconds at 60 fps.
    /// `sustain` and `volume` are 0..=1. Segments are capped so the
    /// whole thing stays under the driver's 253-entry limit.
    pub fn from_adsr(attack_ms: u16, decay_ms: u16, sustain: f32, release_ms: u16, volume: f32) -> Self {
        let peak = (volume.clamp(0.0, 1.0) * 15.0).round() as i32;
        let sus = (sustain.clamp(0.0, 1.0) * peak as f32).round() as i32;
        let frames = |ms: u16, cap: usize| ((ms as f32 / (1000.0 / 60.0988)).round() as usize).min(cap);
        let a = frames(attack_ms, 40);
        let d = frames(decay_ms, 80);
        let r = frames(release_ms, 80);
        let mut v: Vec<u8> = Vec::new();
        // attack: ramp 1..peak over `a` frames (0 frames = instant)
        for i in 0..a {
            let lvl = (peak as f32 * (i + 1) as f32 / (a + 1) as f32).round() as i32;
            v.push(lvl.clamp(0, 15) as u8);
        }
        // decay: peak -> sustain over `d` frames
        if d == 0 {
            v.push(peak as u8);
        } else {
            for i in 0..d {
                let t = i as f32 / d as f32;
                let lvl = (peak as f32 + (sus - peak) as f32 * t).round() as i32;
                v.push(lvl.clamp(0, 15) as u8);
            }
        }
        // sustain hold (loop point)
        let loop_point = v.len() as u8;
        v.push(sus.clamp(0, 15) as u8);
        // release: sustain -> 0 over `r` frames, last value 0
        let release_point = v.len() as u8;
        if r == 0 {
            v.push(0);
        } else {
            for i in 0..r {
                let t = (i + 1) as f32 / r as f32;
                let lvl = (sus as f32 * (1.0 - t)).round() as i32;
                v.push(lvl.clamp(0, 15) as u8);
            }
            if *v.last().unwrap() != 0 {
                v.push(0);
            }
        }
        // If sustain is silent, looping is pointless: hold at 0 and let
        // release also be immediate.
        let (loop_point, release_point) = if sus == 0 { (None, None) } else { (Some(loop_point), Some(release_point)) };
        Self { values: v, loop_point, release_point }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.values.len() + 3);
        out.push(self.values.len() as u8);
        out.push(self.loop_point.unwrap_or(0xFF));
        out.push(self.release_point.unwrap_or(0xFF));
        out.extend(self.values.iter().map(|&v| v.min(15)));
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instrument {
    /// Pulse duty 0–3 / noise mode; `None` leaves the channel's duty alone.
    pub duty: Option<u8>,
    /// `None` = constant full volume.
    pub envelope: Option<Envelope>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DpcmSample {
    pub name: String,
    /// Raw 1-bit delta data; padded by the emitter to 16n+1 bytes.
    pub data: Vec<u8>,
    /// $4010 rate index 0–15.
    pub rate: u8,
    pub loop_: bool,
}

/// rows × channels → events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pattern {
    pub rows: Vec<[Vec<Event>; 5]>,
}

impl Pattern {
    pub fn new(rows: usize) -> Self {
        Self { rows: (0..rows).map(|_| Default::default()).collect() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Song {
    pub title: String,
    /// Frames per row; the driver stores it as 8.8 fixed point.
    pub frames_per_row: f64,
    pub rows_per_pattern: u8,
    pub patterns: Vec<Pattern>,
    /// Indices into `patterns`.
    pub order: Vec<usize>,
    pub loop_pos: usize,
    pub instruments: Vec<Instrument>,
    pub samples: Vec<DpcmSample>,
}

impl Song {
    /// Frames per row for a BPM at 16th-note rows: 60 s × 60 fps / (bpm × 4).
    pub fn frames_per_row_for_bpm(bpm: f64) -> f64 {
        900.0 / bpm.max(1.0)
    }
    pub fn total_rows(&self) -> usize {
        self.order.len() * self.rows_per_pattern as usize
    }
    /// Exact frame count for `rows` rows under the driver's 8.8 row clock,
    /// starting with an empty fractional accumulator.
    pub fn frames_for_rows(&self, rows: usize) -> u32 {
        // The driver requires >= 1 frame per row; mirror that floor so an
        // out-of-range tempo cannot spin here (emit() rejects it anyway).
        let speed = ((self.frames_per_row * 256.0).round() as i64).max(256);
        let mut cnt: i64 = 0;
        let mut frames: u32 = 0;
        let mut done = 0usize;
        loop {
            if cnt >> 8 == 0 {
                if done == rows {
                    return frames;
                }
                done += 1;
                cnt += speed;
            }
            cnt -= 256;
            frames += 1;
        }
    }
    /// Frames before the loop point (the intro) and frames per loop pass.
    pub fn intro_and_loop_frames(&self) -> (u32, u32) {
        let rpp = self.rows_per_pattern as usize;
        let intro = self.frames_for_rows(self.loop_pos.min(self.order.len()) * rpp);
        let looped = self.frames_for_rows((self.order.len() - self.loop_pos.min(self.order.len())) * rpp);
        (intro, looped)
    }
    pub fn total_frames(&self) -> u32 {
        let (i, l) = self.intro_and_loop_frames();
        i + l
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub songs: Vec<Song>,
    /// Album title for multi-song containers (NSF name field, NSFe auth).
    pub album: String,
    pub artist: String,
    pub copyright: String,
    pub expansion: Expansion,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frames each row begins on, for comparing against the accumulator.
    fn row_starts(bpm: f64, rows: usize) -> Vec<u32> {
        let s = Song {
            title: String::new(),
            frames_per_row: Song::frames_per_row_for_bpm(bpm),
            rows_per_pattern: 16,
            patterns: Vec::new(),
            order: Vec::new(),
            loop_pos: 0,
            instruments: Vec::new(),
            samples: Vec::new(),
        };
        (0..rows).map(|r| s.frames_for_rows(r)).collect()
    }

    #[test]
    fn the_row_clock_is_a_fixed_point_accumulator_not_a_multiplication() {
        // 220 BPM is 4.0909 frames per row, which the driver's 8.8 clock
        // renders as eleven rows of four frames and then one of five. Rounding
        // 4.09 to 4 loses a frame every twelve rows — about a row and a half
        // over a three-minute song — and `viper rip` reconstructs this exact
        // sequence to find the grid, so the two have to agree.
        assert_eq!(row_starts(220.0, 15), vec![0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 49, 53, 57]);
        // A whole number of frames per row has no such drift.
        assert_eq!(row_starts(150.0, 5), vec![0, 6, 12, 18, 24]);
    }

    #[test]
    fn frames_for_rows_does_not_add_up_and_that_is_correct() {
        // At 120 BPM one row costs 7 frames and two cost 15, not 14: the
        // accumulator carries a half-frame that a single row rounds away.
        // Across 60-300 BPM, 46% of row-count pairs disagree this way.
        //
        // It matters because `intro_and_loop_frames` measures its two halves
        // *separately* rather than splitting one total, and that is right —
        // the driver restarts its row clock at the loop point, which is
        // observable: ripping two passes of the same song needs a different
        // accumulator phase for each. If this ever becomes additive,
        // someone has changed the row clock.
        // One row of a pattern each side of the loop, so the seam is visible
        // at all — at sixteen rows the residual happens to come out even.
        let s = Song {
            title: String::new(),
            frames_per_row: Song::frames_per_row_for_bpm(120.0),
            rows_per_pattern: 1,
            patterns: Vec::new(),
            order: vec![0, 0],
            loop_pos: 1,
            instruments: Vec::new(),
            samples: Vec::new(),
        };
        assert_eq!(s.frames_for_rows(1), 7, "one row rounds a half-frame away");
        assert_eq!(s.frames_for_rows(2), 15, "two rows keep it");
        let (intro, looped) = s.intro_and_loop_frames();
        assert_eq!((intro, looped), (7, 7), "each half measured from a fresh accumulator");
        assert_eq!(s.total_frames(), 14);
        assert_ne!(s.total_frames(), s.frames_for_rows(2), "which is a frame short of one continuous run");
    }

    #[test]
    fn a_tempo_too_slow_for_the_row_clock_is_still_reported_honestly() {
        // The emitter rejects anything past 255 frames per row; this is the
        // function that decides. 900/4 = 225 fits, 900/3 = 300 does not.
        assert_eq!(Song::frames_per_row_for_bpm(220.0), 900.0 / 220.0);
        assert!(Song::frames_per_row_for_bpm(4.0) < 255.0);
        assert!(Song::frames_per_row_for_bpm(3.0) > 255.0);
        // Zero would divide by zero; it clamps instead of producing infinity.
        assert!(Song::frames_per_row_for_bpm(0.0).is_finite());
    }

    #[test]
    fn an_adsr_becomes_the_frame_envelope_viper_rip_inverts() {
        // The stress song's lead: a=0 d=20 s=0.90 r=60 vol=0.70. `viper rip`
        // reads this shape back out of a register log and reconstructs the
        // ADSR from it, so the shape here is half of a round trip.
        let e = Envelope::from_adsr(0, 20, 0.90, 60, 0.70);
        assert_eq!(e.values[0], 11, "peak is round(0.70 * 15)");
        let loop_ = e.loop_point.expect("a sustain that holds") as usize;
        assert_eq!(e.values[loop_], 10, "sustain is round(0.90 * 11)");
        let rel = e.release_point.expect("and a release") as usize;
        assert!(rel > loop_);
        assert_eq!(*e.values.last().unwrap(), 0, "which always ends silent");
        assert!(e.values[rel..].windows(2).all(|w| w[1] <= w[0]), "and only falls");
    }

    #[test]
    fn a_silent_sustain_has_nowhere_to_loop() {
        // sustain 0 means the note dies on its own, so looping at the
        // sustain point would hold silence forever and a release would have
        // nothing to release from.
        let e = Envelope::from_adsr(0, 10, 0.0, 20, 0.6);
        assert_eq!(e.loop_point, None);
        assert_eq!(e.release_point, None);
    }

    #[test]
    fn an_encoded_envelope_leads_with_its_own_shape() {
        // Length, loop, release, then the values — and $FF for "no such
        // point", which is what the driver checks against.
        let e = Envelope { values: vec![15, 8, 4], loop_point: Some(1), release_point: None };
        assert_eq!(e.encode(), vec![3, 1, 0xFF, 15, 8, 4]);
        // Values are clamped to a nibble on the way out, because that is all
        // the hardware register holds.
        let hot = Envelope { values: vec![200], loop_point: None, release_point: None };
        assert_eq!(hot.encode(), vec![1, 0xFF, 0xFF, 15]);
    }
}
