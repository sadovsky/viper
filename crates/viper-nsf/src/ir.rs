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
    pub fn total_frames(&self) -> u32 {
        (self.total_rows() as f64 * self.frames_per_row).round() as u32
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub songs: Vec<Song>,
    pub artist: String,
    pub copyright: String,
    pub expansion: Expansion,
}
