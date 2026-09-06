//! `viper import`: Standard MIDI File → `.vip`, driven by a map file.
//!
//! The map (`.vmap`, same `@directive key=value` grammar as `.vip` and
//! `.vps`) holds every decision — which MIDI track goes to which channel,
//! how chords collapse to one voice, what each GM drum becomes — so the
//! importer stays generic and the arrangement stays a text file next to
//! the source.
//!
//! Row grid: 16th notes (four rows per quarter). Held notes become `===`
//! cells. Drums resolve one DPCM hit and one NOI hit per row by the map's
//! priority lists; a kick that loses the DPCM slot can fall back to a
//! noise thump when the noise channel is free.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};

use crate::style::kv;
use crate::{Cell, DpcmRef, Instrument, Song, CHANNELS, INSTRUMENTS, STEPS_PER_PHRASE};

// ---------------------------------------------------------------- SMF reader

#[derive(Clone, Debug)]
pub struct MidiNote {
    pub tick: u64,
    pub len: u64,
    pub key: u8,
    pub vel: u8,
}

#[derive(Clone, Debug, Default)]
pub struct MidiTrack {
    pub name: String,
    pub notes: Vec<MidiNote>,
    pub channels: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Midi {
    pub ticks_per_quarter: u16,
    pub tempos: Vec<(u64, f64)>, // (tick, bpm)
    pub tracks: Vec<MidiTrack>,
}

fn vlq(b: &[u8], i: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    loop {
        let c = *b.get(*i).ok_or_else(|| anyhow!("truncated MIDI (vlq)"))?;
        *i += 1;
        v = (v << 7) | (c & 0x7F) as u64;
        if c & 0x80 == 0 {
            return Ok(v);
        }
    }
}

pub fn parse_midi(bytes: &[u8]) -> Result<Midi> {
    if bytes.len() < 14 || &bytes[0..4] != b"MThd" {
        bail!("not a Standard MIDI File");
    }
    let ntrk = u16::from_be_bytes([bytes[10], bytes[11]]) as usize;
    let div = u16::from_be_bytes([bytes[12], bytes[13]]);
    if div & 0x8000 != 0 {
        bail!("SMPTE time division is not supported");
    }
    let hdr_len = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let mut pos = 8 + hdr_len;
    let mut tempos = Vec::new();
    let mut tracks = Vec::new();
    for _ in 0..ntrk {
        if pos + 8 > bytes.len() || &bytes[pos..pos + 4] != b"MTrk" {
            bail!("expected MTrk chunk at byte {}", pos);
        }
        let len = u32::from_be_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]) as usize;
        let body = bytes.get(pos + 8..pos + 8 + len).ok_or_else(|| anyhow!("truncated MTrk"))?;
        pos += 8 + len;
        let mut i = 0usize;
        let mut tick = 0u64;
        let mut running: Option<u8> = None;
        let mut track = MidiTrack::default();
        let mut on: HashMap<(u8, u8), (u64, u8)> = HashMap::new();
        while i < body.len() {
            tick += vlq(body, &mut i)?;
            let mut st = body[i];
            if st == 0xFF {
                let typ = body[i + 1];
                i += 2;
                let ln = vlq(body, &mut i)? as usize;
                let data = &body[i..i + ln];
                i += ln;
                match typ {
                    0x03 => track.name = String::from_utf8_lossy(data).trim().to_string(),
                    0x51 if ln == 3 => {
                        let us = ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | data[2] as u32;
                        tempos.push((tick, 60_000_000.0 / us as f64));
                    }
                    _ => {}
                }
                continue;
            }
            if st == 0xF0 || st == 0xF7 {
                i += 1;
                let ln = vlq(body, &mut i)? as usize;
                i += ln;
                continue;
            }
            if st & 0x80 != 0 {
                running = Some(st);
                i += 1;
            } else {
                st = running.ok_or_else(|| anyhow!("running status without a status byte"))?;
            }
            let kind = st & 0xF0;
            let ch = st & 0x0F;
            let (a, b) = match kind {
                0xC0 | 0xD0 => {
                    let a = body[i];
                    i += 1;
                    (a, 0)
                }
                _ => {
                    let (a, b) = (body[i], body[i + 1]);
                    i += 2;
                    (a, b)
                }
            };
            if !track.channels.contains(&ch) && matches!(kind, 0x80 | 0x90) {
                track.channels.push(ch);
            }
            match kind {
                0x90 if b > 0 => {
                    on.insert((ch, a), (tick, b));
                }
                0x80 | 0x90 => {
                    if let Some((t0, v)) = on.remove(&(ch, a)) {
                        track.notes.push(MidiNote { tick: t0, len: tick.saturating_sub(t0).max(1), key: a, vel: v });
                    }
                }
                _ => {}
            }
        }
        track.notes.sort_by_key(|n| (n.tick, n.key));
        tracks.push(track);
    }
    tempos.sort_by_key(|t| t.0);
    Ok(Midi { ticks_per_quarter: div, tempos, tracks })
}

// ---------------------------------------------------------------- map file

#[derive(Clone, Debug)]
pub struct TrackMap {
    pub midi: String,
    pub ch: Option<usize>,
    pub instr: u8,
    pub flatten: String,
    pub octave: i32,
    pub drums: bool,
    /// Only write where the channel is still free (a second source for a
    /// channel: a harmoniser under the rhythm part, strings in an intro).
    pub fill: bool,
    pub vibrato: Option<(u8, u8)>,
    pub vibrato_min_rows: usize,
    /// Per-track override of the song's velocity handling.
    pub velocity: Option<Velocity>,
}

/// What an imported note does with its MIDI velocity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Velocity {
    /// Map 1..=127 onto viper's volume column, 1..=15. The default: a MIDI
    /// file's dynamics are usually the difference between a cover that
    /// breathes and one that is flat out at every hit.
    #[default]
    Dynamic,
    /// Ignore velocity. Every note gets `vol: 0`, which viper reads as
    /// "channel default", i.e. full. Useful when a source's velocities are
    /// junk — tab exports often pin everything to one value.
    Off,
    /// Pin every note to one volume, 1..=15.
    Fixed(u8),
}

impl Velocity {
    fn parse(v: &str) -> Result<Self> {
        match v {
            "on" | "dynamic" => Ok(Self::Dynamic),
            "off" | "none" | "full" => Ok(Self::Off),
            n => {
                let n: u8 = n.parse().map_err(|_| {
                    anyhow!("velocity= wants on / off / a volume 1-15, got {:?}", v)
                })?;
                if !(1..=15).contains(&n) {
                    bail!("velocity={} is out of range (1-15, or on / off)", n);
                }
                Ok(Self::Fixed(n))
            }
        }
    }

    /// The volume column for a note played at `vel`.
    fn vol(self, vel: u8) -> u8 {
        match self {
            Self::Dynamic => {
                // Linear across the usable MIDI range. Never 0: a cell with
                // `vol: 0` means "default/full" everywhere else in viper, so
                // mapping a quiet note to 0 would play it at full blast.
                let v = vel.clamp(1, 127) as u32;
                (1 + (v - 1) * 14 / 126) as u8
            }
            Self::Off => 0,
            Self::Fixed(n) => n,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum DrumTarget {
    Dpcm(u8),
    Noi { note: u8, instr: u8 },
}

#[derive(Clone, Debug, Default)]
pub struct Map {
    pub title: String,
    /// Note value one row represents: 16 (default) or 32.
    pub grid: usize,
    pub artist: String,
    pub arranger: String,
    pub bpm: Option<u16>,
    pub transpose: i32,
    /// Song-wide default; a `@track` may override it.
    pub velocity: Velocity,
    pub tracks: Vec<TrackMap>,
    pub drums: BTreeMap<u8, DrumTarget>,
    pub priority_dpcm: Vec<u8>,
    pub priority_noi: Vec<u8>,
    pub fallback: BTreeMap<u8, DrumTarget>,
    pub instruments: Vec<(usize, Instrument)>,
    pub samples: Vec<DpcmRef>,
    pub driver: Option<(PathBuf, PathBuf)>,
    /// `@source midi=...` — the MIDI this map is written for, relative to
    /// the map file. Lets `viper import --map x.vmap` stand alone.
    pub source: Option<PathBuf>,
}

fn parse_noi_target(v: &str) -> Result<DrumTarget> {
    let (n, i) = v.split_once('/').ok_or_else(|| anyhow!("noi= wants NOTE/INSTR, got {:?}", v))?;
    let note = crate::vip::decode_note_pub(n).ok_or_else(|| anyhow!("bad note {:?}", n))?;
    Ok(DrumTarget::Noi { note, instr: u8::from_str_radix(i, 16).context("noi instrument")? })
}

fn parse_fx(s: &str) -> Option<(u8, u8)> {
    let b = s.as_bytes();
    if b.len() != 3 {
        return None;
    }
    Some((b[0].to_ascii_uppercase(), u8::from_str_radix(&s[1..], 16).ok()?))
}

fn strip_comment(s: &str) -> &str {
    let mut in_token = false;
    for (i, c) in s.char_indices() {
        if c.is_whitespace() {
            in_token = false;
        } else if c == '#' && !in_token {
            return &s[..i];
        } else {
            in_token = true;
        }
    }
    s
}

pub fn parse_map(text: &str) -> Result<Map> {
    let mut m = Map { grid: 16, ..Default::default() };
    for (ln, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let Some(rest) = line.strip_prefix('@') else { bail!("line {}: expected an @directive", ln + 1) };
        let (dir, args) = match rest.find(char::is_whitespace) {
            Some(i) => (&rest[..i], rest[i..].trim()),
            None => (rest, ""),
        };
        let ctx = || format!("line {}: @{}", ln + 1, dir);
        match dir {
            "song" => {
                for (k, v) in kv(args) {
                    match k.as_str() {
                        "title" => m.title = v,
                        "artist" => m.artist = v,
                        "arranger" => m.arranger = v,
                        "bpm" => m.bpm = if v == "auto" { None } else { Some(v.parse().with_context(ctx)?) },
                        "transpose" => m.transpose = v.parse().with_context(ctx)?,
                        "velocity" => m.velocity = Velocity::parse(&v).with_context(ctx)?,
                        "grid" => {
                            m.grid = v.parse().with_context(ctx)?;
                            if m.grid != 16 && m.grid != 32 { bail!("{}: grid must be 16 or 32", ctx()); }
                        }
                        _ => {}
                    }
                }
            }
            "track" => {
                let mut t = TrackMap { midi: String::new(), ch: None, instr: 0, flatten: "top".into(), octave: 0, drums: false, fill: false, vibrato: None, vibrato_min_rows: 2, velocity: None };
                for (k, v) in kv(args) {
                    match k.as_str() {
                        "midi" => t.midi = v,
                        "ch" => t.ch = Some(match v.to_ascii_uppercase().as_str() { "PU1" => 0, "PU2" => 1, "TRI" => 2, "NOI" => 3, "DPCM" => 4, _ => bail!("{}: unknown channel {:?}", ctx(), v) }),
                        "instr" => t.instr = u8::from_str_radix(&v, 16).with_context(ctx)?,
                        "flatten" => t.flatten = v,
                        "octave" => t.octave = v.parse().with_context(ctx)?,
                        "drums" => t.drums = v == "1" || v == "on",
                        "fill" => t.fill = v == "1" || v == "on",
                        "vibrato" => t.vibrato = parse_fx(&v),
                        "vibrato_min_rows" => t.vibrato_min_rows = v.parse().with_context(ctx)?,
                        "velocity" => t.velocity = Some(Velocity::parse(&v).with_context(ctx)?),
                        _ => {}
                    }
                }
            if t.midi.is_empty() { bail!("{}: needs midi=", ctx()); }
                if !t.drums && t.ch.is_none() { bail!("{}: needs ch= or drums=1", ctx()); }
                m.tracks.push(t);
            }
            "drum" | "fallback" => {
                let (keys, rest) = match args.find(char::is_whitespace) {
                    Some(i) => (&args[..i], args[i..].trim()),
                    None => bail!("{}: want KEYS dpcm=N | noi=NOTE/INSTR", ctx()),
                };
                let mut target = None;
                for (k, v) in kv(rest) {
                    match k.as_str() {
                        "dpcm" => target = Some(DrumTarget::Dpcm(v.parse().with_context(ctx)?)),
                        "noi" => target = Some(parse_noi_target(&v).with_context(ctx)?),
                        _ => {}
                    }
                }
                let target = target.ok_or_else(|| anyhow!("{}: needs dpcm= or noi=", ctx()))?;
                for k in keys.split(',') {
                    let key: u8 = k.trim().parse().with_context(ctx)?;
                    if dir == "drum" { m.drums.insert(key, target); } else { m.fallback.insert(key, target); }
                }
            }
            "priority" => {
                for (k, v) in kv(args) {
                    let list: Vec<u8> = v.split(',').map(|t| t.trim().parse::<u8>()).collect::<Result<_, _>>().with_context(ctx)?;
                    match k.as_str() {
                        "dpcm" => m.priority_dpcm = list,
                        "noi" => m.priority_noi = list,
                        _ => {}
                    }
                }
            }
            "instr" => {
                let (idx_tok, rest) = match args.find(char::is_whitespace) {
                    Some(i) => (&args[..i], args[i..].trim()),
                    None => (args, ""),
                };
                let idx = usize::from_str_radix(idx_tok, 16).with_context(ctx)?;
                if idx >= INSTRUMENTS { bail!("{}: instrument out of range", ctx()); }
                let mut inst = Instrument::default();
                for (k, v) in kv(rest) {
                    match k.as_str() {
                        "a" => inst.attack_ms = v.parse().with_context(ctx)?,
                        "d" => inst.decay_ms = v.parse().with_context(ctx)?,
                        "s" => inst.sustain = v.parse().with_context(ctx)?,
                        "r" => inst.release_ms = v.parse().with_context(ctx)?,
                        "duty" => inst.duty = v.parse().with_context(ctx)?,
                        "vol" => inst.volume = v.parse().with_context(ctx)?,
                        _ => {}
                    }
                }
                m.instruments.push((idx, inst));
            }
            "dpcm" => {
                let (idx_tok, rest) = match args.find(char::is_whitespace) {
                    Some(i) => (&args[..i], args[i..].trim()),
                    None => bail!("{}: want NN name= path= [rate=]", ctx()),
                };
                let idx = usize::from_str_radix(idx_tok, 16).with_context(ctx)?;
                let mut r = DpcmRef { name: format!("sample{:02X}", idx), path: PathBuf::new(), rate: 15 };
                for (k, v) in kv(rest) {
                    match k.as_str() {
                        "name" => r.name = v,
                        "path" => r.path = PathBuf::from(v),
                        "rate" => r.rate = v.parse().with_context(ctx)?,
                        _ => {}
                    }
                }
                while m.samples.len() <= idx {
                    m.samples.push(DpcmRef { name: String::new(), path: PathBuf::new(), rate: 15 });
                }
                m.samples[idx] = r;
            }
            "source" => {
                for (k, v) in kv(args) {
                    if k == "midi" { m.source = Some(PathBuf::from(v)); }
                }
            }
            "driver" => {
                let (mut bin, mut sym) = (None, None);
                for (k, v) in kv(args) {
                    match k.as_str() {
                        "bin" => bin = Some(PathBuf::from(v)),
                        "sym" => sym = Some(PathBuf::from(v)),
                        _ => {}
                    }
                }
                if let Some(b) = bin {
                    let s = sym.unwrap_or_else(|| b.with_extension("sym"));
                    m.driver = Some((b, s));
                }
            }
            _ => bail!("{}: unknown directive", ctx()),
        }
    }
    if m.tracks.is_empty() {
        bail!("map has no @track lines");
    }
    Ok(m)
}

// ---------------------------------------------------------------- import

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub bpm: u16,
    pub rows: usize,
    pub phrases_total: usize,
    pub phrases_unique: usize,
    pub tracks: Vec<String>,
    pub chords_flattened: usize,
    pub voiced_chords: usize,
    pub notes_clamped: usize,
    pub tempo_changes: usize,
    pub filled_rows: usize,
    pub drum_dpcm_conflicts: usize,
    pub drum_fallbacks: usize,
    pub drum_dropped: usize,
    pub noi_conflicts: usize,
    /// Lowest and highest volume written, so a source with no dynamics is
    /// visible rather than quietly disappointing.
    pub vol_lo: u8,
    pub vol_hi: u8,
    pub warnings: Vec<String>,
}

impl Report {
    fn note_volume(&mut self, vol: u8) {
        if self.vol_lo == 0 || vol < self.vol_lo {
            self.vol_lo = vol;
        }
        self.vol_hi = self.vol_hi.max(vol);
    }

    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} BPM, {} rows → {} phrases ({} unique); tracks: {}\nchords flattened {} (voiced across PU1/PU2 {}), notes clamped {}, tempo changes {}, fill notes placed {}\ndrums: DPCM conflicts {} (fallback {}, dropped {}), noise conflicts {}",
            self.bpm, self.rows, self.phrases_total, self.phrases_unique, self.tracks.join(", "),
            self.chords_flattened, self.voiced_chords, self.notes_clamped, self.tempo_changes, self.filled_rows,
            self.drum_dpcm_conflicts, self.drum_fallbacks, self.drum_dropped, self.noi_conflicts
        );
        // vol 0 means "channel default", i.e. velocity was turned off.
        s.push_str(&match (self.vol_lo, self.vol_hi) {
            (0, 0) => "\nvelocity: off (every note at channel default)".to_string(),
            (lo, hi) if lo == hi => format!(
                "\nvelocity: flat at {} — the source has no dynamics; `velocity=off` says so explicitly",
                hi,
            ),
            (lo, hi) => format!("\nvelocity: volumes {}-{}", lo, hi),
        });
        for w in &self.warnings {
            s.push_str("\nwarning: ");
            s.push_str(w);
        }
        s
    }
}

/// Per-row note events for one pitched channel after flattening.
#[derive(Clone, Copy, Debug)]
struct RowNote {
    key: u8,
    len_rows: usize,
    vel: u8,
}

/// One note competing for a row on one channel, before chord flattening.
/// Named rather than a bare tuple because velocity made it a triple and the
/// flatten rules read much better against field names.
#[derive(Clone, Copy, Debug)]
struct Cand {
    key: u8,
    len: usize,
    vel: u8,
}

fn find_track<'a>(midi: &'a Midi, name: &str) -> Option<&'a MidiTrack> {
    let want = name.to_ascii_lowercase();
    midi.tracks.iter().find(|t| t.name.to_ascii_lowercase().contains(&want) && !t.notes.is_empty())
}

pub fn import(midi: &Midi, map: &Map) -> Result<(Song, Report)> {
    let mut report = Report::default();
    let first_tempo = midi.tempos.first().map(|t| t.1).unwrap_or(120.0);
    let bpm = map.bpm.unwrap_or(first_tempo.round() as u16);
    report.bpm = bpm;
    let tpq = midi.ticks_per_quarter as u64;
    let rows_per_quarter = if map.grid == 32 { 8 } else { 4 } as u64;
    let row_ticks = (tpq / rows_per_quarter).max(1);
    let to_row = |tick: u64| -> usize { ((tick as f64 / row_ticks as f64).round()) as usize };

    // A row is a 16th (or 32nd with grid=32), so at grid=32 the row clock
    // runs twice as fast: the driver's speed is set from bpm * 2 there, and
    // mid-song tempo changes ride along in the same units.
    let grid_mul = rows_per_quarter as f64 / 4.0;

    // --- pitched tracks: notes per row, chord sets kept for the cross-track rule
    let mut chords: Vec<BTreeMap<usize, Vec<Cand>>> = Vec::new(); // per track: row -> candidates
    let mut pitched: Vec<(usize, &TrackMap)> = Vec::new();
    let mut drum_track: Option<(&MidiTrack, &TrackMap)> = None;
    for tm in &map.tracks {
        let Some(t) = find_track(midi, &tm.midi) else {
            bail!("no MIDI track matching {:?} (have: {})", tm.midi, midi.tracks.iter().map(|t| format!("{:?}", t.name)).collect::<Vec<_>>().join(", "));
        };
        report.tracks.push(format!("{} → {}", t.name, if tm.drums { "drums".to_string() } else { ["PU1", "PU2", "TRI", "NOI", "DPCM"][tm.ch.unwrap()].to_string() }));
        if tm.drums {
            drum_track = Some((t, tm));
            continue;
        }
        let mut rows: BTreeMap<usize, Vec<Cand>> = BTreeMap::new();
        for n in &t.notes {
            let r = to_row(n.tick);
            let len = ((n.len as f64 / row_ticks as f64).round() as usize).max(1);
            rows.entry(r).or_default().push(Cand { key: n.key, len, vel: n.vel });
        }
        chords.push(rows);
        pitched.push((chords.len() - 1, tm));
    }

    // cross-track rule: PU1 and PU2 share a chord → PU2 root, PU1 fifth
    let pu1 = pitched.iter().find(|(_, tm)| tm.ch == Some(0)).map(|(i, _)| *i);
    let pu2 = pitched.iter().find(|(_, tm)| tm.ch == Some(1)).map(|(i, _)| *i);
    let mut voiced: HashMap<(usize, usize), u8> = HashMap::new(); // (track idx, row) -> key
    if let (Some(a), Some(b)) = (pu1, pu2) {
        let rows_a: Vec<usize> = chords[a].keys().copied().collect();
        for r in rows_a {
            let (Some(ca), Some(cb)) = (chords[a].get(&r), chords[b].get(&r)) else { continue };
            if ca.len() < 2 || cb.len() < 2 { continue; }
            let pcs = |c: &Vec<Cand>| -> Vec<u8> { let mut v: Vec<u8> = c.iter().map(|c| c.key % 12).collect(); v.sort(); v.dedup(); v };
            if pcs(ca) != pcs(cb) { continue; }
            let root = cb.iter().map(|c| c.key).min().unwrap();
            let fifth = ca.iter().map(|c| c.key).filter(|k| (k + 12 - root % 12) % 12 == 7).min();
            if let Some(f) = fifth {
                voiced.insert((a, r), f);
                voiced.insert((b, r), root);
                report.voiced_chords += 1;
            }
        }
    }

    // flatten to one note per row per channel
    let mut lanes: Vec<(usize, BTreeMap<usize, RowNote>, &TrackMap)> = Vec::new();
    for &(ti, tm) in &pitched {
        let mut lane: BTreeMap<usize, RowNote> = BTreeMap::new();
        for (r, cands) in &chords[ti] {
            let picked = if let Some(&k) = voiced.get(&(ti, *r)) {
                // The voicing rule chose a pitch; take that candidate whole so
                // its own velocity travels with it.
                cands.iter().find(|c| c.key == k).copied()
                    .unwrap_or(Cand { key: k, len: 1, vel: 100 })
            } else {
                if cands.len() > 1 { report.chords_flattened += 1; }
                *match tm.flatten.as_str() {
                    "root" => cands.iter().min_by_key(|c| c.key),
                    _ => cands.iter().max_by_key(|c| c.key),
                }.unwrap()
            };
            let (key, len) = (picked.key, picked.len);
            let mut k = key as i32 + tm.octave * 12 + map.transpose;
            let floor = match tm.ch { Some(2) => 24, _ => 33 }; // TRI down to C1, pulses down to A-1
            if k < floor { k += 12 * ((floor - k + 11) / 12); report.notes_clamped += 1; }
            if k > 119 { k -= 12 * ((k - 119 + 11) / 12); report.notes_clamped += 1; }
            lane.insert(*r, RowNote { key: k as u8, len_rows: len, vel: picked.vel });
        }
        lanes.push((tm.ch.unwrap(), lane, tm));
    }

    // --- drums
    // (gm key, velocity) per row: a ghost note and an accent are the same
    // drum, and dropping the difference is what made imported kits sound flat.
    let mut drum_rows: BTreeMap<usize, Vec<(u8, u8)>> = BTreeMap::new();
    if let Some((t, _)) = drum_track {
        for n in &t.notes {
            drum_rows.entry(to_row(n.tick)).or_default().push((n.key, n.vel));
        }
    }

    // --- tempo map: every change after the first, at the row it lands on
    let mut tempo_map: Vec<(usize, u16)> = Vec::new();
    for (tick, t) in midi.tempos.iter().skip(1) {
        let row = to_row(*tick);
        let val = (t * grid_mul).round() as u16;
        if tempo_map.last().map(|l| l.1) != Some(val) && val > 0 {
            tempo_map.push((row, val));
        }
    }
    tempo_map.dedup_by_key(|e| e.0);
    report.tempo_changes = tempo_map.len();

    // --- total rows
    let mut last_row = 0usize;
    for (_, lane, _) in &lanes {
        for (r, n) in lane { last_row = last_row.max(r + n.len_rows); }
    }
    if let Some((&r, _)) = drum_rows.iter().next_back() { last_row = last_row.max(r + 1); }
    let total_rows = last_row.div_ceil(STEPS_PER_PHRASE) * STEPS_PER_PHRASE;
    report.rows = total_rows;
    let nphr = total_rows / STEPS_PER_PHRASE;

    // --- grid
    let mut grid: Vec<[Cell; CHANNELS]> = vec![[Cell::default(); CHANNELS]; total_rows];
    for (ch, lane, tm) in &lanes {
        for (r, n) in lane {
            // a fill track only writes where its channel is still free
            if tm.fill {
                let busy = (0..n.len_rows).any(|h| r + h < total_rows && (grid[r + h][*ch].note.is_some() || grid[r + h][*ch].hold));
                if busy { continue; }
                report.filled_rows += 1;
            }
            // The predicate ignores the vibrato itself: a note shorter than the
            // threshold gets no vibrato whatever depth was configured.
            let fx = tm.vibrato.filter(|_| n.len_rows >= tm.vibrato_min_rows);
            let vol = tm.velocity.unwrap_or(map.velocity).vol(n.vel);
            report.note_volume(vol);
            grid[*r][*ch] = Cell { note: Some(n.key), instr: tm.instr, vol, fx, hold: false };
            for h in 1..n.len_rows {
                let rr = r + h;
                if rr >= total_rows { break; }
                if grid[rr][*ch].note.is_some() { break; } // a new onset wins
                grid[rr][*ch] = Cell::hold();
            }
        }
    }
    let drum_vel = drum_track
        .map(|(_, tm)| tm.velocity.unwrap_or(map.velocity))
        .unwrap_or(map.velocity);
    for (r, keys) in &drum_rows {
        let mut dpcm_hits: Vec<(u8, u8, u8)> = Vec::new(); // (gm key, slot, vel)
        let mut noi_hits: Vec<(u8, u8, u8, u8)> = Vec::new(); // (gm key, note, instr, vel)
        let mut seen = Vec::new();
        for &(k, vel) in keys {
            if seen.contains(&k) { continue; }
            seen.push(k);
            match map.drums.get(&k) {
                Some(DrumTarget::Dpcm(slot)) => dpcm_hits.push((k, *slot, vel)),
                Some(DrumTarget::Noi { note, instr }) => noi_hits.push((k, *note, *instr, vel)),
                None => {}
            }
        }
        let rank = |list: &Vec<u8>, k: u8| list.iter().position(|&x| x == k).unwrap_or(usize::MAX);
        if dpcm_hits.len() > 1 { report.drum_dpcm_conflicts += 1; }
        dpcm_hits.sort_by_key(|(k, _, _)| rank(&map.priority_dpcm, *k));
        if noi_hits.len() > 1 { report.noi_conflicts += 1; }
        noi_hits.sort_by_key(|(k, _, _, _)| rank(&map.priority_noi, *k));
        if let Some((_, slot, vel)) = dpcm_hits.first() {
            let vol = drum_vel.vol(*vel);
            report.note_volume(vol);
            grid[*r][4] = Cell { note: Some(60 + slot), instr: 0, vol, fx: None, hold: false };
        }
        // losers on the DPCM slot may fall back to the noise channel
        for (k, _, vel) in dpcm_hits.iter().skip(1) {
            if let Some(DrumTarget::Noi { note, instr }) = map.fallback.get(k) {
                if noi_hits.is_empty() {
                    noi_hits.push((*k, *note, *instr, *vel));
                    report.drum_fallbacks += 1;
                } else {
                    report.drum_dropped += 1;
                }
            } else {
                report.drum_dropped += 1;
            }
        }
        if let Some((_, note, instr, vel)) = noi_hits.first() {
            let vol = drum_vel.vol(*vel);
            report.note_volume(vol);
            grid[*r][3] = Cell { note: Some(*note), instr: *instr, vol, fx: None, hold: false };
        }
    }

    // --- phrases + order (dedupe identical phrases)
    let mut song = Song { bpm: ((bpm as f64 * grid_mul).round() as u16).max(1), ..Default::default() };
    song.tempo_map = tempo_map;
    let (phrases, order) = crate::phrases_from_rows(&grid)?;
    song.phrases = phrases;
    song.order = order;
    report.phrases_total = nphr;
    report.phrases_unique = song.phrases.len();
    song.loop_pos = 0;
    song.current_phrase = song.order.first().copied().unwrap_or(0);
    for (i, inst) in &map.instruments {
        song.instruments[*i] = *inst;
    }
    song.title = map.title.clone();
    song.artist = if map.arranger.is_empty() { map.artist.clone() } else { format!("{} (arr. {})", map.artist, map.arranger) };
    song.samples = map.samples.clone();
    song.driver = map.driver.clone();
    Ok((song, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny format-1 SMF from (tick, len, key) notes, all at
    /// velocity 100. Most tests do not care about dynamics.
    fn smf(bpm: u32, tracks: &[(&str, u8, &[(u64, u64, u8)])]) -> Vec<u8> {
        let with_vel: Vec<(&str, u8, Vec<(u64, u64, u8, u8)>)> = tracks
            .iter()
            .map(|(n, c, ns)| (*n, *c, ns.iter().map(|(t, l, k)| (*t, *l, *k, 100u8)).collect()))
            .collect();
        let borrowed: Vec<(&str, u8, &[(u64, u64, u8, u8)])> =
            with_vel.iter().map(|(n, c, ns)| (*n, *c, ns.as_slice())).collect();
        smf_vel(bpm, &borrowed)
    }

    /// The same, with an explicit velocity per note.
    fn smf_vel(bpm: u32, tracks: &[(&str, u8, &[(u64, u64, u8, u8)])]) -> Vec<u8> {
        fn vlq(v: u64) -> Vec<u8> {
            let mut b = vec![(v & 0x7F) as u8];
            let mut v = v >> 7;
            while v > 0 { b.push((v & 0x7F) as u8 | 0x80); v >>= 7; }
            b.reverse();
            b
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"MThd");
        out.extend_from_slice(&6u32.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&((tracks.len() + 1) as u16).to_be_bytes());
        out.extend_from_slice(&480u16.to_be_bytes());
        let us = 60_000_000 / bpm;
        let mut t0 = vec![0x00, 0xFF, 0x51, 0x03, (us >> 16) as u8, (us >> 8) as u8, us as u8, 0x00, 0xFF, 0x2F, 0x00];
        let mut chunk = b"MTrk".to_vec();
        chunk.extend_from_slice(&(t0.len() as u32).to_be_bytes());
        chunk.append(&mut t0);
        out.extend_from_slice(&chunk);
        for (name, ch, notes) in tracks {
            let mut ev: Vec<(u64, Vec<u8>)> = Vec::new();
            for (t, l, k, vel) in notes.iter() {
                ev.push((*t, vec![0x90 | ch, *k, *vel]));
                ev.push((*t + *l, vec![0x80 | ch, *k, 0]));
            }
            ev.sort_by_key(|e| e.0);
            let mut body = vec![0x00, 0xFF, 0x03];
            body.extend_from_slice(&vlq(name.len() as u64));
            body.extend_from_slice(name.as_bytes());
            let mut last = 0;
            for (t, bytes) in ev {
                body.extend_from_slice(&vlq(t - last));
                last = t;
                body.extend_from_slice(&bytes);
            }
            body.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
            out.extend_from_slice(b"MTrk");
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(&body);
        }
        out
    }

    const MAP: &str = "@song title=\"T\" artist=\"A\" bpm=auto\n@track midi=lead ch=PU1 instr=00 flatten=top vibrato=V42 vibrato_min_rows=2\n@track midi=rhythm ch=PU2 instr=01 flatten=root octave=-1\n@track midi=bass ch=TRI instr=02\n@track midi=drums drums=1\n@drum 36 dpcm=0\n@drum 38 dpcm=1\n@drum 42 noi=C-6/03\n@drum 49 noi=C-5/04\n@priority dpcm=38,36 noi=49,42\n@fallback 36 noi=C-2/05\n@instr 00 a=0 d=20 s=0.8 r=40 duty=0.25 vol=0.8\n";

    #[test]
    fn parses_running_status_tempo_and_names() {
        let bytes = smf(217, &[("Lead Guitar", 0, &[(0, 480, 64), (480, 240, 66)])]);
        let m = parse_midi(&bytes).unwrap();
        assert_eq!(m.ticks_per_quarter, 480);
        assert!((m.tempos[0].1 - 217.0).abs() < 0.1);
        assert_eq!(m.tracks[1].name, "Lead Guitar");
        assert_eq!(m.tracks[1].notes.len(), 2);
        assert_eq!(m.tracks[1].notes[0].len, 480);
    }

    #[test]
    fn import_holds_flattens_voices_and_resolves_drums() {
        // lead: E4 for 2 rows then a power chord (E4 B4 E5) on row 4; rhythm: same chord on row 4, plus E2 root; bass; drums with kick+snare together
        let bytes = smf(200, &[
            ("Lead", 0, &[(0, 240, 64), (480, 120, 64), (480, 120, 71), (480, 120, 76)]),
            ("Rhythm", 1, &[(0, 120, 40), (480, 120, 64), (480, 120, 71), (480, 120, 76)]),
            ("Bass", 2, &[(0, 480, 28)]),
            ("Drums", 9, &[(0, 60, 36), (0, 60, 38), (0, 60, 42), (120, 60, 36), (120, 60, 49)]),
        ]);
        let midi = parse_midi(&bytes).unwrap();
        let map = parse_map(MAP).unwrap();
        let (song, rep) = import(&midi, &map).unwrap();
        assert_eq!(rep.bpm, 200);
        let p = &song.phrases[song.order[0]];
        // lead E4 on row 0 with vibrato (2 rows) and a hold on row 1, release on row 2
        assert_eq!(p.cells[0][0].note, Some(64));
        assert_eq!(p.cells[0][0].fx, Some((b'V', 0x42)));
        assert!(p.cells[1][0].hold);
        assert!(!p.cells[2][0].hold && p.cells[2][0].note.is_none());
        // row 4: same chord on both → PU2 root (E4 -12 = E3), PU1 fifth (B4)
        assert_eq!(p.cells[4][0].note, Some(71));
        assert_eq!(p.cells[4][1].note, Some(52));
        assert_eq!(rep.voiced_chords, 1);
        // bass 4 rows: note + 3 holds
        assert_eq!(p.cells[0][2].note, Some(28));
        assert!(p.cells[1][2].hold && p.cells[3][2].hold);
        // drums row 0: snare wins DPCM (slot 1 → C#4), kick can't fall back (hat on NOI) → dropped; hat on NOI
        assert_eq!(p.cells[0][4].note, Some(61));
        assert_eq!(p.cells[0][3].note, Some(84));
        // row 1: kick alone... no: row 1 has kick + crash → kick on DPCM, crash on NOI
        assert_eq!(p.cells[1][4].note, Some(60));
        assert_eq!(p.cells[1][3].note, Some(72));
        assert_eq!(rep.drum_dpcm_conflicts, 1);
        assert_eq!(rep.drum_dropped, 1);
        assert_eq!(song.title, "T");
        // round trip through .vip keeps holds
        let text = crate::vip::to_vip(&song);
        let (back, _) = crate::vip::from_vip(&text).unwrap();
        assert!(back.phrases[back.order[0]].cells[1][0].hold);
    }

    #[test]
    fn fallback_kick_lands_on_free_noise_channel() {
        let bytes = smf(200, &[("Drums", 9, &[(0, 60, 36), (0, 60, 38)])]);
        let midi = parse_midi(&bytes).unwrap();
        let map = parse_map("@track midi=drums drums=1\n@drum 36 dpcm=0\n@drum 38 dpcm=1\n@priority dpcm=38,36\n@fallback 36 noi=C-2/05\n").unwrap();
        let (song, rep) = import(&midi, &map).unwrap();
        let p = &song.phrases[0];
        assert_eq!(p.cells[0][4].note, Some(61));
        assert_eq!(p.cells[0][3].note, Some(36));
        assert_eq!(p.cells[0][3].instr, 5);
        assert_eq!(rep.drum_fallbacks, 1);
    }
    #[test]
    fn velocity_maps_onto_the_volume_column() {
        // The mapping never yields 0: viper reads `vol: 0` as "channel
        // default", i.e. full, so a quiet note mapped to 0 would blare.
        assert_eq!(Velocity::Dynamic.vol(1), 1);
        assert_eq!(Velocity::Dynamic.vol(127), 15);
        assert_eq!(Velocity::Dynamic.vol(64), 8);
        assert_eq!(Velocity::Dynamic.vol(0), 1, "a zero-velocity note still sounds");
        // Monotonic across the whole range.
        let mut prev = 0;
        for v in 1..=127u8 {
            let got = Velocity::Dynamic.vol(v);
            assert!(got >= prev && (1..=15).contains(&got), "vel {} -> {}", v, got);
            prev = got;
        }
        assert_eq!(Velocity::Off.vol(90), 0);
        assert_eq!(Velocity::Fixed(6).vol(127), 6);
    }

    #[test]
    fn velocity_parses_its_three_forms_and_rejects_the_rest() {
        assert_eq!(Velocity::parse("on").unwrap(), Velocity::Dynamic);
        assert_eq!(Velocity::parse("off").unwrap(), Velocity::Off);
        assert_eq!(Velocity::parse("9").unwrap(), Velocity::Fixed(9));
        for bad in ["0", "16", "loud", ""] {
            assert!(Velocity::parse(bad).is_err(), "{:?} should be rejected", bad);
        }
    }

    #[test]
    fn an_imported_note_carries_its_own_velocity() {
        // Three notes, quiet to loud, on the lead; the drum track gets an
        // accent and a ghost note on the same drum.
        let bytes = smf_vel(200, &[
            ("Lead", 0, &[(0, 120, 64, 20), (120, 120, 65, 80), (240, 120, 66, 127)]),
            ("Drums", 9, &[(0, 60, 36, 127), (120, 60, 36, 30)]),
        ]);
        let midi = parse_midi(&bytes).unwrap();
        let map = parse_map(
            "@song title=\"T\" bpm=auto\n@track midi=Lead ch=PU1 instr=00\n@track midi=Drums drums=1\n@drum 36 dpcm=0\n",
        ).unwrap();
        let (song, report) = import(&midi, &map).unwrap();
        let p = &song.phrases[0];
        assert_eq!(p.cells[0][0].vol, Velocity::Dynamic.vol(20));
        assert_eq!(p.cells[1][0].vol, Velocity::Dynamic.vol(80));
        assert_eq!(p.cells[2][0].vol, Velocity::Dynamic.vol(127));
        assert!(p.cells[0][0].vol < p.cells[2][0].vol, "quiet note is quieter");
        // Drums too: an accent and a ghost note are the same drum.
        assert_eq!(p.cells[0][4].vol, Velocity::Dynamic.vol(127));
        assert_eq!(p.cells[1][4].vol, Velocity::Dynamic.vol(30));
        assert!(report.summary().contains("velocity: volumes"), "{}", report.summary());
    }

    #[test]
    fn velocity_can_be_turned_off_globally_or_pinned_per_track() {
        let bytes = smf_vel(200, &[
            ("Lead", 0, &[(0, 120, 64, 20)]),
            ("Bass", 2, &[(0, 120, 40, 20)]),
        ]);
        let midi = parse_midi(&bytes).unwrap();

        // Off: every note back to the channel default, which is what the
        // importer did before it read velocity at all.
        let off = parse_map(
            "@song bpm=auto velocity=off\n@track midi=Lead ch=PU1 instr=00\n@track midi=Bass ch=TRI instr=02\n",
        ).unwrap();
        let (song, report) = import(&midi, &off).unwrap();
        assert_eq!(song.phrases[0].cells[0][0].vol, 0);
        assert!(report.summary().contains("velocity: off"), "{}", report.summary());

        // A per-track override beats the song default in both directions.
        let mixed = parse_map(
            "@song bpm=auto velocity=off\n@track midi=Lead ch=PU1 instr=00 velocity=on\n@track midi=Bass ch=TRI instr=02 velocity=12\n",
        ).unwrap();
        let (song, _) = import(&midi, &mixed).unwrap();
        assert_eq!(song.phrases[0].cells[0][0].vol, Velocity::Dynamic.vol(20));
        assert_eq!(song.phrases[0].cells[0][2].vol, 12);
    }

    #[test]
    fn a_source_with_no_dynamics_says_so() {
        // Tab exports routinely pin every note to one velocity. Mapping that
        // is harmless but pointless, and the report should name it rather
        // than leave you wondering why nothing varies.
        let bytes = smf_vel(200, &[("Lead", 0, &[(0, 120, 64, 100), (120, 120, 65, 100)])]);
        let midi = parse_midi(&bytes).unwrap();
        let map = parse_map("@song bpm=auto\n@track midi=Lead ch=PU1 instr=00\n").unwrap();
        let (_, report) = import(&midi, &map).unwrap();
        assert!(report.summary().contains("velocity: flat at"), "{}", report.summary());
        assert!(report.summary().contains("velocity=off"), "and suggests the fix");
    }

}
