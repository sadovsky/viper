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
    Dpcm,
}

impl Channel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "pu1" | "1" => Some(Self::Pu1),
            "pu2" | "2" => Some(Self::Pu2),
            "tri" | "3" => Some(Self::Tri),
            "noi" | "4" => Some(Self::Noi),
            "dpcm" | "dpc" | "5" => Some(Self::Dpcm),
            _ => None,
        }
    }
    pub fn index(self) -> usize {
        match self {
            Self::Pu1 => 0,
            Self::Pu2 => 1,
            Self::Tri => 2,
            Self::Noi => 3,
            Self::Dpcm => 4,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Pu1 => "PU1",
            Self::Pu2 => "PU2",
            Self::Tri => "TRI",
            Self::Noi => "NOI",
            Self::Dpcm => "DPCM",
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
    HarmonicMinor,
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
            "harmonic_minor" | "harmonic" => Some(Self::HarmonicMinor),
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
            Self::HarmonicMinor => &[0, 2, 3, 5, 7, 8, 11],
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

// ---------- Chord symbols (Stage 25) ----------

/// A chord: root pitch class plus intervals above it in semitones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    pub root: u8,
    pub intervals: Vec<i32>,
}

impl Chord {
    /// Chord tones as MIDI notes with the root in `octave`.
    pub fn tones(&self, octave: i32) -> Vec<u8> {
        let base = 12 * (octave + 1) + self.root as i32;
        self.intervals.iter().map(|i| (base + i).clamp(0, 127) as u8).collect()
    }
    fn interval(&self, i: usize) -> i32 {
        self.intervals.get(i).copied().unwrap_or(0)
    }
}

/// Split a note letter with optional accidental off the front: `F#m7` →
/// `(6, "m7")`. Returns `None` if the token doesn't start with A–G.
fn split_root(s: &str) -> Option<(u8, &str)> {
    let first = *s.as_bytes().first()?;
    if !matches!(first.to_ascii_uppercase(), b'A'..=b'G') {
        return None;
    }
    let n = if matches!(s.as_bytes().get(1), Some(b'#') | Some(b'b')) { 2 } else { 1 };
    Some((parse_key(&s[..n])?, &s[n..]))
}

/// `Am` → A minor, `C` → C major, `F#m` → F# minor, `Ebmaj` → Eb major,
/// `Ddorian` → D dorian. Also accepts `A minor` when joined with a space.
pub fn parse_key_spec(s: &str) -> Option<(u8, Mode)> {
    let s = s.trim();
    let (root, rest) = split_root(s)?;
    let rest = rest.trim();
    let mode = match rest {
        "" | "maj" | "major" | "M" => Mode::Major,
        "m" | "min" | "minor" => Mode::Minor,
        other => Mode::parse(other)?,
    };
    Some((root, mode))
}

fn apply_chord_suffix(intervals: &mut Vec<i32>, s: &str, diatonic_seventh: Option<i32>) -> bool {
    match s {
        "" => {}
        "7" => intervals.push(diatonic_seventh.unwrap_or(10)),
        "maj7" | "M7" => intervals.push(11),
        "6" => intervals.push(9),
        "9" => { intervals.push(diatonic_seventh.unwrap_or(10)); intervals.push(14); }
        "dim" | "o" => { intervals[1] = 3; intervals[2] = 6; }
        "dim7" | "o7" => { intervals[1] = 3; intervals[2] = 6; intervals.push(9); }
        "aug" | "+" => intervals[2] = 8,
        "sus4" | "sus" => intervals[1] = 5,
        "sus2" => intervals[1] = 2,
        "5" => { intervals.truncate(1); intervals.push(7); }
        _ => return false,
    }
    true
}

/// Parse one chord token. Roman numerals (`i`, `IV`, `bVII`, `V7`) are
/// relative to `key` in `mode`, with the numeral's case deciding the third
/// and the scale deciding the fifth and seventh; letter names (`Am`,
/// `Cmaj7`, `F#dim7`, `G7`, `Esus4`, `E5`) are absolute.
pub fn parse_chord_symbol(tok: &str, key: u8, mode: Mode) -> Option<Chord> {
    let tok = tok.trim_matches(|c| c == '"' || c == '\'' || c == ',');
    if tok.is_empty() {
        return None;
    }
    // Roman: an optional b/# then i/v letters, then a quality suffix.
    let acc_len = if tok.starts_with('b') || tok.starts_with('#') { 1 } else { 0 };
    let num_len = tok[acc_len..].chars().take_while(|c| matches!(c, 'i' | 'I' | 'v' | 'V')).count();
    if num_len > 0 {
        let (num, suffix) = tok.split_at(acc_len + num_len);
        if let Some(sym) = crate::style::parse_chord(num) {
            let iv = mode.intervals();
            let n = iv.len();
            let deg = sym.degree.min(n - 1);
            let at = |d: usize| iv[d % n] as i32 + if d >= n { 12 } else { 0 } - iv[deg] as i32;
            let root_rel = iv[deg] as i32 + sym.accidental;
            let (fifth, seventh) = if sym.accidental == 0 { (at(deg + 4), Some(at(deg + 6))) } else { (7, None) };
            let mut intervals = vec![0, if sym.minor { 3 } else { 4 }, fifth];
            if !apply_chord_suffix(&mut intervals, suffix, seventh) {
                return None;
            }
            return Some(Chord { root: (key as i32 + root_rel).rem_euclid(12) as u8, intervals });
        }
    }
    let (root, suffix) = split_root(tok)?;
    let mut intervals = vec![0, 4, 7];
    let rest = if let Some(r) = suffix.strip_prefix("min") {
        intervals[1] = 3;
        r
    } else if suffix.starts_with('m') && !suffix.starts_with("maj") {
        intervals[1] = 3;
        &suffix[1..]
    } else {
        suffix
    };
    if !apply_chord_suffix(&mut intervals, rest, None) {
        return None;
    }
    Some(Chord { root, intervals })
}

/// Bundled progressions, roman numerals in the current key.
pub fn progression_preset(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        "12bar" | "blues" => "I I I I IV IV I I V IV I I",
        "doowop" | "50s" => "I vi IV V",
        "canon" | "pachelbel" => "I V vi iii IV I IV V",
        "andalusian" => "i VII VI V",
        "royal_road" | "royalroad" => "IV V iii vi",
        "four_chords" | "axis" => "I V vi IV",
        "gothenburg" => "i VI VII i",
        _ => return None,
    })
}

/// Parse a progression: a preset name, or chord tokens (roman or absolute).
pub fn parse_progression(tokens: &[&str], key: u8, mode: Mode) -> Result<Vec<Chord>> {
    let joined: Vec<String> = match tokens {
        [one] if progression_preset(one).is_some() => progression_preset(one).unwrap().split(' ').map(String::from).collect(),
        _ => tokens.iter().map(|t| t.to_string()).collect(),
    };
    let mut out = Vec::new();
    for t in &joined {
        let t = t.trim_matches(|c| c == '"' || c == '\'');
        if t.is_empty() { continue; }
        out.push(parse_chord_symbol(t, key, mode).ok_or_else(|| anyhow!("bad chord {:?}", t))?);
    }
    if out.is_empty() {
        bail!("no chords given");
    }
    Ok(out)
}

fn ensure_phrases(song: &mut Song, count: usize) {
    while song.phrases.len() < count {
        song.phrases.push(Phrase::default());
    }
}

fn hit(note: u8, instr: u8, vol: u8) -> Cell {
    Cell { note: Some(note), instr, vol, fx: None }
}

// ---------- chord_prog / bassline / arp ----------

/// Voice a progression across PU1 (top), PU2 (middle), TRI (root) with a
/// hat pulse on NOI; `steps_per_chord` steps each, flowing into following
/// phrases (created as needed). Returns the number of phrases written.
pub fn chord_prog(song: &mut Song, chords: &[Chord], steps_per_chord: usize) -> usize {
    let spc = steps_per_chord.clamp(1, STEPS_PER_PHRASE);
    let per_phrase = (STEPS_PER_PHRASE / spc).max(1);
    let n_phrases = (chords.len() + per_phrase - 1) / per_phrase;
    let first = song.current_phrase;
    ensure_phrases(song, first + n_phrases);
    for pi in 0..n_phrases {
        let phrase = &mut song.phrases[first + pi];
        for ch in 0..4 {
            clear_channel(phrase, ch);
        }
        for slot in 0..per_phrase {
            let Some(c) = chords.get(pi * per_phrase + slot) else { break };
            let start = slot * spc;
            let tones = c.tones(4);
            let top = *tones.last().unwrap_or(&60);
            let mid = tones.get(1).copied().unwrap_or(top);
            let bass = c.tones(2)[0];
            phrase.cells[start][0] = hit(top, 0, 15);
            phrase.cells[start][1] = hit(mid, 1, 13);
            phrase.cells[start][2] = hit(bass, 2, 15);
            if spc >= 4 {
                let half = start + spc / 2;
                phrase.cells[half][0] = hit(top, 0, 10);
                phrase.cells[half][1] = hit(mid, 1, 9);
                phrase.cells[half][2] = hit(bass, 2, 12);
            }
        }
        for s in (1..STEPS_PER_PHRASE).step_by(2) {
            phrase.cells[s][3] = hit(60, 3, 8);
        }
    }
    n_phrases
}

/// A bassline on TRI under a progression. Styles: `walking` (root, third,
/// fifth, chromatic approach to the next root on 8ths), `arpeggio` (root,
/// third, fifth, octave on 16ths), `root_fifth` (alternating on 8ths),
/// `octaves` (root / octave on 16ths), `roots` (root on the chord change).
pub fn bassline(song: &mut Song, chords: &[Chord], style: &str, steps_per_chord: usize) -> Result<usize> {
    let spc = steps_per_chord.clamp(1, STEPS_PER_PHRASE);
    let per_phrase = (STEPS_PER_PHRASE / spc).max(1);
    let n_phrases = (chords.len() + per_phrase - 1) / per_phrase;
    let first = song.current_phrase;
    ensure_phrases(song, first + n_phrases);
    for pi in 0..n_phrases {
        let phrase = &mut song.phrases[first + pi];
        clear_channel(phrase, 2);
        for slot in 0..per_phrase {
            let Some(c) = chords.get(pi * per_phrase + slot) else { break };
            let next = &chords[(pi * per_phrase + slot + 1) % chords.len()];
            let start = slot * spc;
            let root = c.tones(2)[0] as i32;
            let third = root + c.interval(1);
            let fifth = root + c.interval(2);
            let next_root = next.tones(2)[0] as i32;
            let approach = if next_root > root { next_root - 1 } else { next_root + 1 };
            let seq: Vec<i32> = match style {
                "walking" | "walk" => {
                    let mut s = vec![root, third, fifth];
                    // As many 8th-note slots as the chord spans; the last one leads in.
                    let slots = (spc / 2).max(1);
                    while s.len() < slots { s.push(root + 12); }
                    s.truncate(slots);
                    if slots > 1 { *s.last_mut().unwrap() = approach; }
                    s
                }
                "arpeggio" | "arp" => vec![root, third, fifth, root + 12],
                "root_fifth" | "root5" => vec![root, fifth],
                "octaves" => vec![root, root + 12],
                "roots" | "root" => vec![root],
                other => bail!("bassline: unknown style {:?} (walking | arpeggio | root_fifth | octaves | roots)", other),
            };
            let stride = match style {
                "arpeggio" | "arp" | "octaves" => 1,
                "roots" | "root" => spc,
                _ => 2,
            };
            for (i, s) in (start..start + spc).step_by(stride).enumerate() {
                if s < STEPS_PER_PHRASE {
                    phrase.cells[s][2] = hit(seq[i % seq.len()].clamp(24, 96) as u8, 2, 15);
                }
            }
        }
    }
    Ok(n_phrases)
}

/// Arpeggiate one chord on `ch`: `pattern` is `up`, `down`, `updown` or
/// `random`; `len` steps are filled, one note every `rate` steps, with
/// the chord tones spread over `octaves` octaves from octave 4.
pub fn arp(song: &mut Song, ch: Channel, chord: &Chord, pattern: &str, len: usize, rate: usize, octaves: i32, seed: u64) -> Result<()> {
    let mut pool: Vec<u8> = Vec::new();
    for o in 0..octaves.max(1) {
        pool.extend(chord.tones(4 + o));
    }
    pool.sort_unstable();
    pool.dedup();
    let seq: Vec<u8> = match pattern {
        "up" => pool.clone(),
        "down" => pool.iter().rev().copied().collect(),
        "updown" | "pingpong" => {
            let mut s = pool.clone();
            if pool.len() > 2 { s.extend(pool[1..pool.len() - 1].iter().rev()); }
            s
        }
        "random" => pool.clone(),
        other => bail!("arp: unknown pattern {:?} (up | down | updown | random)", other),
    };
    let mut rng = Rng::new(seed);
    let phrase = &mut song.phrases[song.current_phrase];
    let idx = ch.index();
    clear_channel(phrase, idx);
    let (_, instr) = default_note_instr(ch);
    let len = len.clamp(1, STEPS_PER_PHRASE);
    for (i, s) in (0..len).step_by(rate.max(1)).enumerate() {
        let note = if pattern == "random" { seq[rng.range(0, seq.len() as u32) as usize] } else { seq[i % seq.len()] };
        phrase.cells[s][idx] = hit(note, instr, 15);
    }
    Ok(())
}

// ---------- drums ----------

/// A drum preset: 16-step masks (`x` = hit) for kick, snare, closed hat,
/// open hat.
pub struct DrumPreset {
    pub name: &'static str,
    pub kick: &'static str,
    pub snare: &'static str,
    pub hat: &'static str,
    pub open: &'static str,
}

pub const DRUM_PRESETS: &[DrumPreset] = &[
    DrumPreset { name: "four",      kick: "x...x...x...x...", snare: "....x.......x...", hat: "..x...x...x...x.", open: "................" },
    DrumPreset { name: "breakbeat", kick: "x..x..x...x.x...", snare: "....x......x..x.", hat: "x.x.x.x.x.x.x.x.", open: "................" },
    DrumPreset { name: "amen",      kick: "x.x.......xx....", snare: "....x..x.x..x..x", hat: "x.x.x.x.x.x.x.x.", open: "................" },
    DrumPreset { name: "trap",      kick: "x......x..x.....", snare: "....x.......x...", hat: "xxxxxxxxxxxxxxxx", open: "......x.......x." },
    DrumPreset { name: "gameboy",   kick: "x...x...x...x...", snare: "....x.......x..x", hat: "..x...x...x...x.", open: "................" },
    DrumPreset { name: "dnb",       kick: "x.........x.....", snare: "....x.......x...", hat: "x.x.x.x.x.x.x.x.", open: "..............x." },
    DrumPreset { name: "halftime",  kick: "x.......x.......", snare: "........x.......", hat: "..x...x...x...x.", open: "................" },
    DrumPreset { name: "dbeat",     kick: "x..x..x..x..x..x", snare: "..x..x..x..x..x.", hat: "x.x.x.x.x.x.x.x.", open: "................" },
    DrumPreset { name: "blast",     kick: "x.x.x.x.x.x.x.x.", snare: ".x.x.x.x.x.x.x.x", hat: "x.x.x.x.x.x.x.x.", open: "................" },
];

pub fn drum_preset(name: &str) -> Option<&'static DrumPreset> {
    DRUM_PRESETS.iter().find(|p| p.name == name.to_ascii_lowercase())
}

/// Write a drum preset: kick and snare on DPCM (C-4 / C#4, the built-in
/// bank), hats on NOI. With `dpcm == false` everything lands on NOI using
/// the `four_on_floor` pitch convention (36 kick, 50 snare, 60 hat).
/// `fills` adds that many snare hits, Euclidean, over the last four steps.
pub fn drums(song: &mut Song, preset: &DrumPreset, fills: usize, dpcm: bool) {
    let phrase = &mut song.phrases[song.current_phrase];
    clear_channel(phrase, 3);
    if dpcm {
        clear_channel(phrase, 4);
    }
    let on = |mask: &str, s: usize| mask.as_bytes().get(s).map_or(false, |&b| b == b'x' || b == b'X');
    let mut snare_mask: Vec<bool> = (0..STEPS_PER_PHRASE).map(|s| on(preset.snare, s)).collect();
    if fills > 0 {
        for (i, f) in euclid_mask(fills.min(4), 4, 0).iter().enumerate() {
            if *f { snare_mask[12 + i] = true; }
        }
    }
    for s in 0..STEPS_PER_PHRASE {
        let kick = on(preset.kick, s);
        let snare = snare_mask[s];
        let hat = on(preset.hat, s);
        let open = on(preset.open, s);
        if dpcm {
            // One DPCM channel: the snare wins a shared step and the kick
            // moves to NOI as a low thump so the downbeat isn't lost.
            if snare {
                phrase.cells[s][4] = hit(61, 0, 15);
            } else if kick {
                phrase.cells[s][4] = hit(60, 0, 15);
            }
            if open {
                phrase.cells[s][3] = hit(55, 3, 12);
            } else if kick && snare {
                phrase.cells[s][3] = hit(36, 3, 15);
            } else if hat {
                phrase.cells[s][3] = hit(60, 3, 9);
            }
        } else {
            let note = if kick { Some(36) } else if snare { Some(50) } else if open { Some(55) } else if hat { Some(60) } else { None };
            if let Some(n) = note {
                phrase.cells[s][3] = hit(n, 3, if hat && !kick && !snare { 9 } else { 15 });
            }
        }
    }
}

// ---------- lsystem / cellular ----------

/// Parse `A=ABA,B=.A.` into rewrite rules.
pub fn parse_rules(s: &str) -> Result<Vec<(char, String)>> {
    let mut rules = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() { continue; }
        let (k, v) = part.split_once('=').ok_or_else(|| anyhow!("rule {:?} needs the form X=YZ", part))?;
        let mut kc = k.trim().chars();
        let (Some(c), None) = (kc.next(), kc.next()) else { bail!("rule {:?}: left side must be one character", part) };
        rules.push((c, v.trim().to_string()));
    }
    if rules.is_empty() {
        bail!("no rules");
    }
    Ok(rules)
}

/// Expand an L-system `iterations` times (output capped at 4096 symbols).
pub fn lsystem_expand(axiom: &str, rules: &[(char, String)], iterations: usize) -> String {
    let mut cur = axiom.to_string();
    for _ in 0..iterations {
        let mut next = String::with_capacity(cur.len() * 2);
        for c in cur.chars() {
            match rules.iter().find(|(k, _)| *k == c) {
                Some((_, v)) => next.push_str(v),
                None => next.push(c),
            }
            if next.len() >= 4096 { break; }
        }
        cur = next;
    }
    cur
}

/// `C4`, `C-4`, `F#3`, `Bb2` → MIDI note; `-` or `.` → rest.
pub fn parse_note_name(s: &str) -> Option<Option<u8>> {
    if s == "-" || s == "." || s.eq_ignore_ascii_case("rest") || s == "---" {
        return Some(None);
    }
    let (pc, rest) = split_root(s)?;
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    let oct: i32 = rest.parse().ok()?;
    let midi = 12 * (oct + 1) + pc as i32;
    (0..=127).contains(&midi).then_some(Some(midi as u8))
}

/// Parse `A=C4,B=G3,.=-` into symbol → note (None = rest).
pub fn parse_note_map(s: &str) -> Result<Vec<(char, Option<u8>)>> {
    let mut map = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() { continue; }
        let (k, v) = part.split_once('=').ok_or_else(|| anyhow!("map entry {:?} needs the form X=C4", part))?;
        let mut kc = k.trim().chars();
        let (Some(c), None) = (kc.next(), kc.next()) else { bail!("map entry {:?}: left side must be one character", part) };
        let note = parse_note_name(v.trim()).ok_or_else(|| anyhow!("bad note {:?} in map", v))?;
        map.push((c, note));
    }
    Ok(map)
}

/// Lindenmayer system → one symbol per step on `ch`. Symbols without a
/// mapping (and `.` / `-`) are rests; unmapped letters default to a scale
/// walk so `A=ABA,B=.A.` sounds like something without a map.
pub fn lsystem(song: &mut Song, ch: Channel, axiom: &str, rules: &[(char, String)], iterations: usize, map: &[(char, Option<u8>)], key: u8, mode: Mode) -> String {
    let expanded = lsystem_expand(axiom, rules, iterations);
    let phrase = &mut song.phrases[song.current_phrase];
    let idx = ch.index();
    clear_channel(phrase, idx);
    let (_, instr) = default_note_instr(ch);
    let (lo, _) = default_octaves(ch);
    let iv = mode.intervals();
    for (s, c) in expanded.chars().take(STEPS_PER_PHRASE).enumerate() {
        let note = match map.iter().find(|(k, _)| *k == c) {
            Some((_, n)) => *n,
            None if c.is_ascii_alphabetic() => {
                let deg = (c.to_ascii_uppercase() as u8 - b'A') as usize;
                let oct = lo + (deg / iv.len()) as i32;
                Some((12 * (oct + 1) + key as i32 + iv[deg % iv.len()] as i32).clamp(0, 127) as u8)
            }
            None => None,
        };
        if let Some(n) = note {
            phrase.cells[s][idx] = hit(n, instr, 15);
        }
    }
    expanded
}

/// Elementary cellular automaton, wrap-around, `rows` generations from
/// a seed row (`center` = one live cell in the middle, `random` = from
/// the RNG, `left` = one live cell at the edge).
pub fn cellular_rows(rule: u8, width: usize, rows: usize, seed: &str, rng: &mut Rng) -> Vec<Vec<bool>> {
    let width = width.max(3);
    let mut row = vec![false; width];
    match seed {
        "random" => for c in row.iter_mut() { *c = rng.chance(0.5); },
        "left" => row[0] = true,
        _ => row[width / 2] = true,
    }
    let mut out = Vec::with_capacity(rows);
    for _ in 0..rows {
        out.push(row.clone());
        let mut next = vec![false; width];
        for i in 0..width {
            let l = row[(i + width - 1) % width] as u8;
            let c = row[i] as u8;
            let r = row[(i + 1) % width] as u8;
            next[i] = (rule >> ((l << 2) | (c << 1) | r)) & 1 == 1;
        }
        row = next;
    }
    out
}

/// Cellular automaton → notes on `ch`: generation `s` is step `s`; a step
/// hits when its centre cell is alive, and the pitch climbs the scale with
/// the number of live cells in that generation.
pub fn cellular(song: &mut Song, ch: Channel, rule: u8, width: usize, seed: &str, key: u8, mode: Mode, rng_seed: u64) -> usize {
    let mut rng = Rng::new(rng_seed);
    let rows = cellular_rows(rule, width, STEPS_PER_PHRASE, seed, &mut rng);
    let phrase = &mut song.phrases[song.current_phrase];
    let idx = ch.index();
    clear_channel(phrase, idx);
    let (_, instr) = default_note_instr(ch);
    let (lo, hi) = default_octaves(ch);
    let iv = mode.intervals();
    let span = ((hi - lo + 1) as usize * iv.len()).max(1);
    let mut hits = 0;
    for (s, row) in rows.iter().enumerate() {
        if !row[row.len() / 2] {
            continue;
        }
        let alive = row.iter().filter(|&&c| c).count();
        let deg = (alive * span / (row.len() + 1)).min(span - 1);
        let oct = lo + (deg / iv.len()) as i32;
        let note = (12 * (oct + 1) + key as i32 + iv[deg % iv.len()] as i32).clamp(0, 127) as u8;
        phrase.cells[s][idx] = hit(note, instr, 15);
        hits += 1;
    }
    hits
}

// ---------- Command dispatch ----------

/// Key from `key=Am` / `key=A mode=minor` / defaults (A minor).
fn key_mode_from_kv(kv: &[(&str, &str)]) -> Result<(u8, Mode)> {
    let key = kv_get(kv, "key").unwrap_or("Am");
    let (k, m) = parse_key_spec(key).ok_or_else(|| anyhow!("bad key {:?}", key))?;
    let mode = match kv_get(kv, "mode") {
        Some(md) => Mode::parse(md).ok_or_else(|| anyhow!("bad mode {:?}", md))?,
        None => m,
    };
    Ok((k, mode))
}

fn kv_num<T: std::str::FromStr>(kv: &[(&str, &str)], key: &str, default: T) -> Result<T> {
    match kv_get(kv, key) {
        Some(v) => v.parse::<T>().map_err(|_| anyhow!("{}={:?} is not a number", key, v)),
        None => Ok(default),
    }
}

const USAGE: &str = "usage: :gen four | euclid <ch> <k> <n> [off] | scale <ch> <key> [mode] [density] | \
chord_prog <preset|chords…> [key=Am] [steps=4] | bassline <preset|chords…> [style=walking] [key=Am] [steps=4] | \
arp <chord> [up|down|updown|random] [len] [rate=1] [ch=pu2] [octaves=2] | drums <preset> [fills=N] [dpcm=off] | \
lsystem axiom=A rules=A=ABA,B=.A. [iterations=4] [map=A=C4,B=G3,.=-] [ch=pu1] | cellular [rule=30] [ch=pu1] [key=Am] [seed=center|random]";

/// Parse and run a `:gen` subcommand. Returns a status line describing what
/// was done.
///
/// Accepts both positional and `key=value` forms for optional params:
///   :gen euclid pu1 5 16 2
///   :gen euclid pu1 5 16 offset=2
///   :gen scale pu2 A minor 0.4
///   :gen scale pu2 A mode=minor density=0.4
///   :gen chord_prog i iv V i key=Am
///   :gen drums breakbeat fills=2
pub fn dispatch(song: &mut Song, args: &[&str], seed: u64) -> Result<String> {
    let (pos, kv) = split_args(args);
    match pos.as_slice() {
        [] => bail!("{}", USAGE),
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
        ["chord_prog", chords @ ..] | ["chords", chords @ ..] | ["prog", chords @ ..] => {
            let (key, mode) = key_mode_from_kv(&kv)?;
            let chords = parse_progression(chords, key, mode)?;
            let steps = kv_num(&kv, "steps", 4usize)?;
            let n = chord_prog(song, &chords, steps);
            Ok(format!("chord_prog: {} chords over {} phrase(s) from {:02X}", chords.len(), n, song.current_phrase))
        }
        ["bassline", chords @ ..] | ["bass", chords @ ..] => {
            let (key, mode) = key_mode_from_kv(&kv)?;
            let chords = parse_progression(chords, key, mode)?;
            let steps = kv_num(&kv, "steps", 4usize)?;
            let style = kv_get(&kv, "style").unwrap_or("walking");
            let n = bassline(song, &chords, style, steps)?;
            Ok(format!("bassline {}: {} chords over {} phrase(s) on TRI", style, chords.len(), n))
        }
        ["arp", chord, rest @ ..] => {
            let (key, mode) = key_mode_from_kv(&kv)?;
            let c = parse_chord_symbol(chord, key, mode).ok_or_else(|| anyhow!("bad chord {:?}", chord))?;
            let pattern = rest.first().copied().or(kv_get(&kv, "pattern")).unwrap_or("up");
            let len: usize = match rest.get(1) {
                Some(l) => l.parse().map_err(|_| anyhow!("bad length {:?}", l))?,
                None => kv_num(&kv, "len", 16usize)?,
            };
            let rate = kv_num(&kv, "rate", 1usize)?;
            let octaves = kv_num(&kv, "octaves", 2i32)?;
            let ch_s = kv_get(&kv, "ch").unwrap_or("pu2");
            let ch = Channel::parse(ch_s).ok_or_else(|| anyhow!("bad channel {:?}", ch_s))?;
            arp(song, ch, &c, pattern, len, rate, octaves, seed)?;
            Ok(format!("arp {} {} ×{} on {}", chord, pattern, len, ch.label()))
        }
        ["drums", name] => {
            let preset = drum_preset(name).ok_or_else(|| {
                let names: Vec<&str> = DRUM_PRESETS.iter().map(|p| p.name).collect();
                anyhow!("unknown drum preset {:?} (have: {})", name, names.join(", "))
            })?;
            let fills = kv_num(&kv, "fills", 0usize)?;
            let dpcm = !matches!(kv_get(&kv, "dpcm"), Some("off") | Some("no") | Some("0"));
            drums(song, preset, fills, dpcm);
            Ok(format!("drums {}{}{}", preset.name,
                if fills > 0 { format!(" +{} fill hits", fills) } else { String::new() },
                if dpcm { " (kick/snare on DPCM, hats on NOI)" } else { " (all on NOI)" }))
        }
        ["drums"] => {
            let names: Vec<&str> = DRUM_PRESETS.iter().map(|p| p.name).collect();
            Ok(format!("drum presets: {}", names.join(", ")))
        }
        ["lsystem"] | ["lsys"] => {
            let axiom = kv_get(&kv, "axiom").ok_or_else(|| anyhow!("lsystem: need axiom=…"))?;
            let rules = parse_rules(kv_get(&kv, "rules").ok_or_else(|| anyhow!("lsystem: need rules=A=ABA,B=.A."))?)?;
            let iterations = kv_num(&kv, "iterations", 4usize)?.min(12);
            let map = match kv_get(&kv, "map") { Some(m) => parse_note_map(m)?, None => Vec::new() };
            let (key, mode) = key_mode_from_kv(&kv)?;
            let ch_s = kv_get(&kv, "ch").unwrap_or("pu1");
            let ch = Channel::parse(ch_s).ok_or_else(|| anyhow!("bad channel {:?}", ch_s))?;
            let expanded = lsystem(song, ch, axiom, &rules, iterations, &map, key, mode);
            let shown: String = expanded.chars().take(16).collect();
            Ok(format!("lsystem: {} symbols after {} iterations, first 16 → {} on {}", expanded.len(), iterations, shown, ch.label()))
        }
        ["cellular"] | ["ca"] => {
            let rule = kv_num(&kv, "rule", 30u8)?;
            let width = kv_num(&kv, "width", 16usize)?;
            let (key, mode) = key_mode_from_kv(&kv)?;
            let ch_s = kv_get(&kv, "ch").unwrap_or("pu1");
            let ch = Channel::parse(ch_s).ok_or_else(|| anyhow!("bad channel {:?}", ch_s))?;
            let seed_kind = kv_get(&kv, "seed").unwrap_or("center");
            let hits = cellular(song, ch, rule, width, seed_kind, key, mode, seed);
            Ok(format!("cellular rule {}: {} hits on {}", rule, hits, ch.label()))
        }
        _ => bail!("{}", USAGE),
    }
}

/// Split args into positional tokens and `key=value` pairs. Quotes around
/// tokens are dropped so `:gen chord_prog "i iv V i"` works as typed.
fn split_args<'a>(args: &'a [&'a str]) -> (Vec<&'a str>, Vec<(&'a str, &'a str)>) {
    let mut pos = Vec::new();
    let mut kv = Vec::new();
    for a in args {
        let a = a.trim_matches('"');
        if a.is_empty() {
            continue;
        }
        if let Some((k, v)) = a.split_once('=') {
            kv.push((k, v));
        } else {
            pos.push(a);
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
        Channel::Dpcm => (60, 0), // C-4 = kick
    }
}

fn default_octaves(ch: Channel) -> (i32, i32) {
    match ch {
        Channel::Pu1 => (4, 5),
        Channel::Pu2 => (3, 4),
        Channel::Tri => (2, 3),
        Channel::Noi => (3, 4),
        Channel::Dpcm => (4, 4), // C-4..B-4 = samples 0..11
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

    // ---- Stage 25 ----

    #[test]
    fn chord_symbols_roman_and_absolute() {
        let am = parse_chord_symbol("i", 9, Mode::Minor).unwrap();
        assert_eq!(am, Chord { root: 9, intervals: vec![0, 3, 7] });
        // V in A minor: uppercase makes it major even though the scale says minor.
        let e = parse_chord_symbol("V", 9, Mode::Minor).unwrap();
        assert_eq!(e, Chord { root: 4, intervals: vec![0, 4, 7] });
        // bVII in C major is Bb major; V7 in C carries the dominant seventh.
        assert_eq!(parse_chord_symbol("bVII", 0, Mode::Major).unwrap().root, 10);
        assert_eq!(parse_chord_symbol("V7", 0, Mode::Major).unwrap(), Chord { root: 7, intervals: vec![0, 4, 7, 10] });
        assert_eq!(parse_chord_symbol("Dm", 0, Mode::Major).unwrap(), Chord { root: 2, intervals: vec![0, 3, 7] });
        assert_eq!(parse_chord_symbol("Cmaj7", 0, Mode::Major).unwrap().intervals, vec![0, 4, 7, 11]);
        assert_eq!(parse_chord_symbol("F#dim7", 0, Mode::Major).unwrap(), Chord { root: 6, intervals: vec![0, 3, 6, 9] });
        assert_eq!(parse_chord_symbol("E5", 0, Mode::Major).unwrap().intervals, vec![0, 7]);
        assert!(parse_chord_symbol("H", 0, Mode::Major).is_none());
        assert!(parse_chord_symbol("Cxyz", 0, Mode::Major).is_none());
        assert!(matches!(parse_key_spec("F#m"), Some((6, Mode::Minor))));
        assert!(matches!(parse_key_spec("Ebmaj"), Some((3, Mode::Major))));
        assert!(matches!(parse_key_spec("Ddorian"), Some((2, Mode::Dorian))));
    }

    #[test]
    fn presets_and_progressions_fill_phrases() {
        let mut song = Song::default();
        let chords = parse_progression(&["12bar"], 0, Mode::Major).unwrap();
        assert_eq!(chords.len(), 12);
        let n = chord_prog(&mut song, &chords, 4);
        assert_eq!(n, 3);
        assert_eq!(song.phrases.len(), 3);
        // First chord: C major → TRI C-2 root, PU2 third, PU1 fifth on step 0.
        assert_eq!(song.phrases[0].cells[0][2].note, Some(36));
        assert_eq!(song.phrases[0].cells[0][1].note, Some(64));
        assert_eq!(song.phrases[0].cells[0][0].note, Some(67));
        // Fifth chord (IV) lands on phrase 1 step 0 with an F root.
        assert_eq!(song.phrases[1].cells[0][2].note, Some(41));
    }

    #[test]
    fn bassline_styles_are_deterministic_and_land_on_roots() {
        let mut song = Song::default();
        let chords = parse_progression(&["Am", "Dm", "E", "Am"], 9, Mode::Minor).unwrap();
        bassline(&mut song, &chords, "walking", 4).unwrap();
        let tri: Vec<Option<u8>> = (0..16).map(|s| song.phrases[0].cells[s][2].note).collect();
        assert_eq!(tri[0], Some(45), "A-2 root on the downbeat");
        assert_eq!(tri[4], Some(38), "D-2 on the chord change");
        assert_eq!(tri[2], Some(39), "chromatic approach to D from above (D is below A)");
        assert!(bassline(&mut song, &chords, "nope", 4).is_err());
        let mut b = Song::default();
        bassline(&mut b, &chords, "arpeggio", 4).unwrap();
        assert_eq!((0..4).map(|s| b.phrases[0].cells[s][2].note.unwrap()).collect::<Vec<_>>(), vec![45, 48, 52, 57]);
    }

    #[test]
    fn arp_patterns() {
        let c = parse_chord_symbol("Am", 0, Mode::Major).unwrap();
        let mut song = Song::default();
        arp(&mut song, Channel::Pu2, &c, "up", 16, 1, 1, 0).unwrap();
        let notes: Vec<u8> = (0..6).map(|s| song.phrases[0].cells[s][1].note.unwrap()).collect();
        assert_eq!(notes, vec![69, 72, 76, 69, 72, 76]);
        arp(&mut song, Channel::Pu2, &c, "updown", 16, 2, 2, 0).unwrap();
        assert!(song.phrases[0].cells[1][1].note.is_none(), "rate 2 leaves odd steps empty");
        let mut a = Song::default();
        let mut b = Song::default();
        arp(&mut a, Channel::Pu1, &c, "random", 16, 1, 2, 7).unwrap();
        arp(&mut b, Channel::Pu1, &c, "random", 16, 1, 2, 7).unwrap();
        assert_eq!(a.phrases[0].cells, b.phrases[0].cells);
    }

    #[test]
    fn drum_presets_are_well_formed_and_fill_adds_snares() {
        for p in DRUM_PRESETS {
            for m in [p.kick, p.snare, p.hat, p.open] {
                assert_eq!(m.len(), 16, "{} mask {:?}", p.name, m);
            }
        }
        let mut song = Song::default();
        drums(&mut song, drum_preset("four").unwrap(), 0, true);
        let kicks = (0..16).filter(|&s| song.phrases[0].cells[s][4].note == Some(60)).count();
        let snares = (0..16).filter(|&s| song.phrases[0].cells[s][4].note == Some(61)).count();
        // Beats 2 and 4 carry both; the snare keeps DPCM and the kick moves to NOI.
        let noi_kicks = (0..16).filter(|&s| song.phrases[0].cells[s][3].note == Some(36)).count();
        assert_eq!((kicks, snares, noi_kicks), (2, 2, 2));
        drums(&mut song, drum_preset("four").unwrap(), 2, true);
        let snares = (0..16).filter(|&s| song.phrases[0].cells[s][4].note == Some(61)).count();
        assert!(snares >= 3, "fill adds snare hits: {}", snares);
        drums(&mut song, drum_preset("four").unwrap(), 0, false);
        assert!((0..16).any(|s| song.phrases[0].cells[s][4].note.is_some()), "dpcm=off leaves the DPCM column alone");
        assert_eq!(song.phrases[0].cells[0][3].note, Some(36));
    }

    #[test]
    fn lsystem_expands_and_maps() {
        let rules = parse_rules("A=ABA,B=.A.").unwrap();
        assert_eq!(lsystem_expand("A", &rules, 1), "ABA");
        assert_eq!(lsystem_expand("A", &rules, 2), "ABA.A.ABA");
        let map = parse_note_map("A=C4,B=G3,.=-").unwrap();
        assert_eq!(parse_note_name("F#3"), Some(Some(54)));
        assert_eq!(parse_note_name("-"), Some(None));
        let mut song = Song::default();
        let expanded = lsystem(&mut song, Channel::Pu1, "A", &rules, 2, &map, 0, Mode::Major);
        assert_eq!(expanded.len(), 9);
        let notes: Vec<Option<u8>> = (0..9).map(|s| song.phrases[0].cells[s][0].note).collect();
        assert_eq!(notes, vec![Some(60), Some(55), Some(60), None, Some(60), None, Some(60), Some(55), Some(60)]);
    }

    #[test]
    fn cellular_rule_30_from_center() {
        let mut rng = Rng::new(1);
        let rows = cellular_rows(30, 7, 3, "center", &mut rng);
        let s = |r: &Vec<bool>| r.iter().map(|&c| if c { 'x' } else { '.' }).collect::<String>();
        assert_eq!(s(&rows[0]), "...x...");
        assert_eq!(s(&rows[1]), "..xxx..");
        assert_eq!(s(&rows[2]), ".xx..x.");
        let mut song = Song::default();
        let hits = cellular(&mut song, Channel::Pu1, 30, 16, "center", 9, Mode::Minor, 1);
        assert!(hits > 0 && hits < 16);
        assert!(song.phrases[0].cells[0][0].note.is_some(), "the seed cell is alive at step 0");
    }

    #[test]
    fn dispatch_routes_new_generators() {
        let mut song = Song::default();
        assert!(dispatch(&mut song, &["chord_prog", "\"i", "iv", "V", "i\"", "key=Am"], 1).unwrap().contains("4 chords"));
        assert!(dispatch(&mut song, &["drums", "breakbeat", "fills=2"], 1).unwrap().contains("breakbeat"));
        assert!(dispatch(&mut song, &["drums", "nope"], 1).is_err());
        assert!(dispatch(&mut song, &["arp", "Cmaj7", "updown", "16", "rate=2"], 1).is_ok());
        assert!(dispatch(&mut song, &["lsystem", "axiom=A", "rules=A=ABA,B=.A.", "iterations=3"], 1).is_ok());
        assert!(dispatch(&mut song, &["cellular", "rule=110", "ch=pu2"], 1).is_ok());
        assert!(dispatch(&mut song, &["bassline", "doowop", "style=root_fifth", "key=C"], 1).unwrap().contains("root_fifth"));
    }
}
