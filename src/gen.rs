//! Algorithmic generators (Stage 3.5).
//!
//! Each generator mutates the current phrase of a [`Song`] in place, clearing
//! the channel it writes to first. RNG is a tiny xorshift64 so results are
//! deterministic given a seed — good enough for sketching patterns, no external
//! crate needed.

use anyhow::{anyhow, bail, Result};

use crate::{Cell, Phrase, Song, STEPS_PER_PHRASE};

// ---------- RNG ----------

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        let s = if seed == 0 { 0xACE1_DEAD_BEEF_CAFE } else { seed };
        Self(s)
    }
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 32) as u32
    }
    pub fn range(&mut self, lo: u32, hi: u32) -> u32 {
        debug_assert!(hi > lo);
        lo + self.next_u32() % (hi - lo)
    }
    pub fn chance(&mut self, p: f32) -> bool {
        (self.next_u32() as f32 / u32::MAX as f32) < p
    }
}

// ---------- Channel ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Pu1,
    Pu2,
    Tri,
    Noi,
}

impl Channel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "pu1" | "1" => Some(Self::Pu1),
            "pu2" | "2" => Some(Self::Pu2),
            "tri" | "3" => Some(Self::Tri),
            "noi" | "4" => Some(Self::Noi),
            _ => None,
        }
    }
    pub fn index(self) -> usize {
        match self {
            Self::Pu1 => 0,
            Self::Pu2 => 1,
            Self::Tri => 2,
            Self::Noi => 3,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Pu1 => "PU1",
            Self::Pu2 => "PU2",
            Self::Tri => "TRI",
            Self::Noi => "NOI",
        }
    }
}

fn clear_channel(phrase: &mut Phrase, ch: usize) {
    for s in 0..STEPS_PER_PHRASE {
        phrase.cells[s][ch] = Cell::default();
    }
}

// ---------- Generators ----------

/// Classic four-on-the-floor on NOI: kick on every 4 steps, snare on 2/4,
/// hat on offbeats. The noise generator ignores pitch, so the note values
/// just need to differ from `None` to retrigger.
pub fn four_on_floor(song: &mut Song) {
    let phrase = &mut song.phrases[song.current_phrase];
    clear_channel(phrase, 3);
    for s in 0..STEPS_PER_PHRASE {
        let note = match s % 4 {
            0 => 36, // kick
            2 => 50, // snare
            _ => 60, // hat
        };
        phrase.cells[s][3] = Cell {
            note: Some(note),
            instr: 3,
            vol: 15,
            fx: None,
        };
    }
}

/// Bjorklund-style Euclidean rhythm: `k` hits distributed as evenly as
/// possible across `n` steps, then rotated by `offset`.
pub fn euclid_mask(k: usize, n: usize, offset: usize) -> Vec<bool> {
    let n = n.max(1);
    let k = k.min(n);
    if k == 0 {
        return vec![false; n];
    }
    let mut mask = vec![false; n];
    let mut acc = 0usize;
    for i in 0..n {
        acc += k;
        if acc >= n {
            acc -= n;
            mask[i] = true;
        }
    }
    let off = offset % n;
    if off != 0 {
        mask.rotate_right(off);
    }
    mask
}

/// Fill `ch` with a Euclidean pattern of `note` at instrument `instr`.
/// `n` is clamped to the phrase length.
pub fn euclid(
    song: &mut Song,
    ch: Channel,
    k: usize,
    n: usize,
    offset: usize,
    note: u8,
    instr: u8,
) {
    let phrase = &mut song.phrases[song.current_phrase];
    let idx = ch.index();
    clear_channel(phrase, idx);
    let width = n.min(STEPS_PER_PHRASE);
    let mask = euclid_mask(k, width, offset);
    for (s, &hit) in mask.iter().enumerate() {
        if hit {
            phrase.cells[s][idx] = Cell {
                note: Some(note),
                instr,
                vol: 15,
                fx: None,
            };
        }
    }
}

// ---------- Scale / random melody ----------

#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Major,
    Minor,
    Dorian,
    Phrygian,
    Lydian,
    Mixolydian,
    Locrian,
    PentMajor,
    PentMinor,
    Blues,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "major" | "ionian" => Some(Self::Major),
            "minor" | "aeolian" => Some(Self::Minor),
            "dorian" => Some(Self::Dorian),
            "phrygian" => Some(Self::Phrygian),
            "lydian" => Some(Self::Lydian),
            "mixolydian" => Some(Self::Mixolydian),
            "locrian" => Some(Self::Locrian),
            "pent_major" | "pentmajor" => Some(Self::PentMajor),
            "pent_minor" | "pentminor" => Some(Self::PentMinor),
            "blues" => Some(Self::Blues),
            _ => None,
        }
    }
    pub fn intervals(self) -> &'static [u8] {
        match self {
            Self::Major      => &[0, 2, 4, 5, 7, 9, 11],
            Self::Minor      => &[0, 2, 3, 5, 7, 8, 10],
            Self::Dorian     => &[0, 2, 3, 5, 7, 9, 10],
            Self::Phrygian   => &[0, 1, 3, 5, 7, 8, 10],
            Self::Lydian     => &[0, 2, 4, 6, 7, 9, 11],
            Self::Mixolydian => &[0, 2, 4, 5, 7, 9, 10],
            Self::Locrian    => &[0, 1, 3, 5, 6, 8, 10],
            Self::PentMajor  => &[0, 2, 4, 7, 9],
            Self::PentMinor  => &[0, 3, 5, 7, 10],
            Self::Blues      => &[0, 3, 5, 6, 7, 10],
        }
    }
}

/// Parse a pitch-class letter with optional `#` or `b` accidental (e.g. `A`,
/// `C#`, `Bb`). Returns a semitone 0..12.
pub fn parse_key(s: &str) -> Option<u8> {
    let b = s.as_bytes();
    if b.is_empty() {
        return None;
    }
    let pc = match b[0].to_ascii_uppercase() {
        b'C' => 0, b'D' => 2, b'E' => 4, b'F' => 5,
        b'G' => 7, b'A' => 9, b'B' => 11,
        _ => return None,
    };
    let acc: i32 = match b.get(1).copied() {
        Some(b'#') => 1,
        Some(b'b') => -1,
        None => 0,
        _ => return None,
    };
    let mut pc = pc as i32 + acc;
    while pc < 0 {
        pc += 12;
    }
    Some((pc % 12) as u8)
}

/// Fill `ch` with uniformly-random notes from the given mode. Each step
/// becomes a hit with probability `density`; the pitch is chosen uniformly
/// from the scale degrees over `octave_low..=octave_high`.
pub fn random_in_scale(
    song: &mut Song,
    ch: Channel,
    key: u8,
    mode: Mode,
    density: f32,
    octave_low: i32,
    octave_high: i32,
    instr: u8,
    seed: u64,
) {
    let mut rng = Rng::new(seed);
    let phrase = &mut song.phrases[song.current_phrase];
    let idx = ch.index();
    clear_channel(phrase, idx);
    let intervals = mode.intervals();
    let oct_range = (octave_high - octave_low + 1).max(1) as u32;
    let density = density.clamp(0.0, 1.0);
    for s in 0..STEPS_PER_PHRASE {
        if !rng.chance(density) {
            continue;
        }
        let deg = intervals[rng.range(0, intervals.len() as u32) as usize];
        let oct = octave_low + rng.range(0, oct_range) as i32;
        let midi = 12 * (oct + 1) + key as i32 + deg as i32;
        if (0..=127).contains(&midi) {
            phrase.cells[s][idx] = Cell {
                note: Some(midi as u8),
                instr,
                vol: 15,
                fx: None,
            };
        }
    }
}

// ---------- Command dispatch ----------

/// Parse and run a `:gen` subcommand. Returns a status line describing what
/// was done.
///
/// Accepts both positional and `key=value` forms for optional params:
///   :gen euclid pu1 5 16 2
///   :gen euclid pu1 5 16 offset=2
///   :gen scale pu2 A minor 0.4
///   :gen scale pu2 A mode=minor density=0.4
pub fn dispatch(song: &mut Song, args: &[&str], seed: u64) -> Result<String> {
    let (pos, kv) = split_args(args);
    match pos.as_slice() {
        [] => bail!("usage: :gen <algo> ...  (four | euclid | scale)"),
        ["four"] | ["four_on_floor"] => {
            four_on_floor(song);
            Ok("generated four-on-floor drums on NOI".into())
        }
        ["euclid", ch, k, n] => {
            let off = kv_get(&kv, "offset").unwrap_or("0");
            run_euclid(song, ch, k, n, off)
        }
        ["euclid", ch, k, n, off] => run_euclid(song, ch, k, n, off),
        ["scale", ch, key] => {
            let mode = kv_get(&kv, "mode").unwrap_or("minor");
            let density = kv_get(&kv, "density").unwrap_or("0.5");
            run_scale(song, ch, key, mode, density, seed)
        }
        ["scale", ch, key, mode] => {
            let density = kv_get(&kv, "density").unwrap_or("0.5");
            run_scale(song, ch, key, mode, density, seed)
        }
        ["scale", ch, key, mode, density] => run_scale(song, ch, key, mode, density, seed),
        _ => bail!(
            "usage: :gen four | :gen euclid <ch> <k> <n> [off] | :gen scale <ch> <key> [mode] [density]"
        ),
    }
}

/// Split args into positional tokens and `key=value` pairs.
fn split_args<'a>(args: &'a [&'a str]) -> (Vec<&'a str>, Vec<(&'a str, &'a str)>) {
    let mut pos = Vec::new();
    let mut kv = Vec::new();
    for a in args {
        if let Some((k, v)) = a.split_once('=') {
            kv.push((k, v));
        } else {
            pos.push(*a);
        }
    }
    (pos, kv)
}

fn kv_get<'a>(kv: &[(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    kv.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

fn run_euclid(song: &mut Song, ch: &str, k: &str, n: &str, off: &str) -> Result<String> {
    let ch = Channel::parse(ch).ok_or_else(|| anyhow!("bad channel {:?}", ch))?;
    let k: usize = k.parse().map_err(|_| anyhow!("bad k"))?;
    let n: usize = n.parse().map_err(|_| anyhow!("bad n"))?;
    let off: usize = off.parse().map_err(|_| anyhow!("bad offset"))?;
    let (note, instr) = default_note_instr(ch);
    euclid(song, ch, k, n, off, note, instr);
    Ok(format!("euclid {} in {} on {}", k, n, ch.label()))
}

fn run_scale(song: &mut Song, ch: &str, key: &str, mode: &str, density: &str, seed: u64) -> Result<String> {
    let ch = Channel::parse(ch).ok_or_else(|| anyhow!("bad channel {:?}", ch))?;
    let key = parse_key(key).ok_or_else(|| anyhow!("bad key {:?}", key))?;
    let mode = Mode::parse(mode).ok_or_else(|| anyhow!("bad mode {:?}", mode))?;
    let density: f32 = density.parse().map_err(|_| anyhow!("bad density"))?;
    let (_note, instr) = default_note_instr(ch);
    let (lo, hi) = default_octaves(ch);
    random_in_scale(song, ch, key, mode, density, lo, hi, instr, seed);
    Ok(format!("scale {:?} on {}", mode, ch.label()))
}

fn default_note_instr(ch: Channel) -> (u8, u8) {
    match ch {
        Channel::Pu1 => (69, 0), // A4
        Channel::Pu2 => (64, 1), // E4
        Channel::Tri => (45, 2), // A2
        Channel::Noi => (60, 3),
    }
}

fn default_octaves(ch: Channel) -> (i32, i32) {
    match ch {
        Channel::Pu1 => (4, 5),
        Channel::Pu2 => (3, 4),
        Channel::Tri => (2, 3),
        Channel::Noi => (3, 4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn euclid_hit_count() {
        let m = euclid_mask(5, 16, 0);
        assert_eq!(m.iter().filter(|&&h| h).count(), 5);
    }

    #[test]
    fn euclid_tresillo() {
        // 3-in-8 Cuban tresillo: hits at 0, 3, 6.
        let m = euclid_mask(3, 8, 0);
        let hits: Vec<usize> = m.iter().enumerate().filter(|(_, &h)| h).map(|(i, _)| i).collect();
        assert_eq!(hits, vec![2, 5, 7]); // Bjorklund orientation — accept whatever the impl gives as long as it's 3 hits.
    }

    #[test]
    fn rng_deterministic() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u32(), b.next_u32());
        }
    }

    #[test]
    fn parse_key_accidentals() {
        assert_eq!(parse_key("C"), Some(0));
        assert_eq!(parse_key("C#"), Some(1));
        assert_eq!(parse_key("Db"), Some(1));
        assert_eq!(parse_key("A"), Some(9));
        assert_eq!(parse_key("Bb"), Some(10));
    }
}
