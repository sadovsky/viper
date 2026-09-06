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
use crate::{Cell, DpcmRef, Instrument, Phrase, Song, CHANNELS, INSTRUMENTS, STEPS_PER_PHRASE};

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
    pub vibrato: Option<(u8, u8)>,
    pub vibrato_min_rows: usize,
}

#[derive(Clone, Copy, Debug)]
pub enum DrumTarget {
    Dpcm(u8),
    Noi { note: u8, instr: u8 },
}

#[derive(Clone, Debug, Default)]
pub struct Map {
    pub title: String,
    pub artist: String,
    pub arranger: String,
    pub bpm: Option<u16>,
    pub transpose: i32,
    pub tracks: Vec<TrackMap>,
    pub drums: BTreeMap<u8, DrumTarget>,
    pub priority_dpcm: Vec<u8>,
    pub priority_noi: Vec<u8>,
    pub fallback: BTreeMap<u8, DrumTarget>,
    pub instruments: Vec<(usize, Instrument)>,
    pub samples: Vec<DpcmRef>,
    pub driver: Option<(PathBuf, PathBuf)>,
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
    let mut m = Map::default();
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
                        _ => {}
                    }
                }
            }
            "track" => {
                let mut t = TrackMap { midi: String::new(), ch: None, instr: 0, flatten: "top".into(), octave: 0, drums: false, vibrato: None, vibrato_min_rows: 2 };
                for (k, v) in kv(args) {
                    match k.as_str() {
                        "midi" => t.midi = v,
                        "ch" => t.ch = Some(match v.to_ascii_uppercase().as_str() { "PU1" => 0, "PU2" => 1, "TRI" => 2, "NOI" => 3, "DPCM" => 4, _ => bail!("{}: unknown channel {:?}", ctx(), v) }),
                        "instr" => t.instr = u8::from_str_radix(&v, 16).with_context(ctx)?,
                        "flatten" => t.flatten = v,
                        "octave" => t.octave = v.parse().with_context(ctx)?,
                        "drums" => t.drums = v == "1" || v == "on",
                        "vibrato" => t.vibrato = parse_fx(&v),
                        "vibrato_min_rows" => t.vibrato_min_rows = v.parse().with_context(ctx)?,
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
    pub drum_dpcm_conflicts: usize,
    pub drum_fallbacks: usize,
    pub drum_dropped: usize,
    pub noi_conflicts: usize,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} BPM, {} rows → {} phrases ({} unique); tracks: {}\nchords flattened {} (voiced across PU1/PU2 {}), notes clamped {}\ndrums: DPCM conflicts {} (fallback {}, dropped {}), noise conflicts {}",
            self.bpm, self.rows, self.phrases_total, self.phrases_unique, self.tracks.join(", "),
            self.chords_flattened, self.voiced_chords, self.notes_clamped,
            self.drum_dpcm_conflicts, self.drum_fallbacks, self.drum_dropped, self.noi_conflicts
        );
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
}

fn find_track<'a>(midi: &'a Midi, name: &str) -> Option<&'a MidiTrack> {
    let want = name.to_ascii_lowercase();
    midi.tracks.iter().find(|t| t.name.to_ascii_lowercase().contains(&want) && !t.notes.is_empty())
}

pub fn import(midi: &Midi, map: &Map) -> Result<(Song, Report)> {
    let mut report = Report::default();
    let bpm = match map.bpm {
        Some(b) => b,
        None => {
            let first = midi.tempos.first().map(|t| t.1).unwrap_or(120.0);
            if midi.tempos.iter().any(|t| (t.1 - first).abs() > 0.5) {
                report.warnings.push(format!("tempo changes in the MIDI are ignored; using the first ({:.1} BPM)", first));
            }
            first.round() as u16
        }
    };
    report.bpm = bpm;
    let tpq = midi.ticks_per_quarter as u64;
    let row_ticks = tpq / 4;
    let to_row = |tick: u64| -> usize { ((tick as f64 / row_ticks as f64).round()) as usize };

    // --- pitched tracks: notes per row, chord sets kept for the cross-track rule
    let mut chords: Vec<BTreeMap<usize, Vec<(u8, usize)>>> = Vec::new(); // per track: row -> [(key, len_rows)]
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
        let mut rows: BTreeMap<usize, Vec<(u8, usize)>> = BTreeMap::new();
        for n in &t.notes {
            let r = to_row(n.tick);
            let len = ((n.len as f64 / row_ticks as f64).round() as usize).max(1);
            rows.entry(r).or_default().push((n.key, len));
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
            let pcs = |c: &Vec<(u8, usize)>| -> Vec<u8> { let mut v: Vec<u8> = c.iter().map(|(k, _)| k % 12).collect(); v.sort(); v.dedup(); v };
            if pcs(ca) != pcs(cb) { continue; }
            let root = cb.iter().map(|(k, _)| *k).min().unwrap();
            let fifth = ca.iter().map(|(k, _)| *k).filter(|k| (k + 12 - root % 12) % 12 == 7).min();
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
            let (key, len) = if let Some(&k) = voiced.get(&(ti, *r)) {
                let l = cands.iter().find(|(kk, _)| *kk == k).map(|(_, l)| *l).unwrap_or(1);
                (k, l)
            } else {
                if cands.len() > 1 { report.chords_flattened += 1; }
                let pick = match tm.flatten.as_str() {
                    "root" => cands.iter().min_by_key(|(k, _)| *k),
                    _ => cands.iter().max_by_key(|(k, _)| *k),
                }.unwrap();
                *pick
            };
            let mut k = key as i32 + tm.octave * 12 + map.transpose;
            let floor = match tm.ch { Some(2) => 24, _ => 33 }; // TRI down to C1, pulses down to A-1
            if k < floor { k += 12 * ((floor - k + 11) / 12); report.notes_clamped += 1; }
            if k > 119 { k -= 12 * ((k - 119 + 11) / 12); report.notes_clamped += 1; }
            lane.insert(*r, RowNote { key: k as u8, len_rows: len });
        }
        lanes.push((tm.ch.unwrap(), lane, tm));
    }

    // --- drums
    let mut drum_rows: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    if let Some((t, _)) = drum_track {
        for n in &t.notes {
            drum_rows.entry(to_row(n.tick)).or_default().push(n.key);
        }
    }

    // --- total rows
    let mut last_row = 0usize;
    for (_, lane, _) in &lanes {
        for (r, n) in lane { last_row = last_row.max(r + n.len_rows); }
    }
    if let Some((&r, _)) = drum_rows.iter().next_back() { last_row = last_row.max(r + 1); }
    let total_rows = ((last_row + STEPS_PER_PHRASE - 1) / STEPS_PER_PHRASE) * STEPS_PER_PHRASE;
    report.rows = total_rows;
    let nphr = total_rows / STEPS_PER_PHRASE;

    // --- grid
    let mut grid: Vec<[Cell; CHANNELS]> = vec![[Cell::default(); CHANNELS]; total_rows];
    for (ch, lane, tm) in &lanes {
        for (r, n) in lane {
            let fx = if let Some(v) = tm.vibrato { if n.len_rows >= tm.vibrato_min_rows { Some(v) } else { None } } else { None };
            grid[*r][*ch] = Cell { note: Some(n.key), instr: tm.instr, vol: 0, fx, hold: false };
            for h in 1..n.len_rows {
                let rr = r + h;
                if rr >= total_rows { break; }
                if grid[rr][*ch].note.is_some() { break; } // a new onset wins
                grid[rr][*ch] = Cell::hold();
            }
        }
    }
    for (r, keys) in &drum_rows {
        let mut dpcm_hits: Vec<(u8, u8)> = Vec::new(); // (gm key, slot)
        let mut noi_hits: Vec<(u8, u8, u8)> = Vec::new(); // (gm key, note, instr)
        let mut seen = Vec::new();
        for &k in keys {
            if seen.contains(&k) { continue; }
            seen.push(k);
            match map.drums.get(&k) {
                Some(DrumTarget::Dpcm(slot)) => dpcm_hits.push((k, *slot)),
                Some(DrumTarget::Noi { note, instr }) => noi_hits.push((k, *note, *instr)),
                None => {}
            }
        }
        let rank = |list: &Vec<u8>, k: u8| list.iter().position(|&x| x == k).unwrap_or(usize::MAX);
        if dpcm_hits.len() > 1 { report.drum_dpcm_conflicts += 1; }
        dpcm_hits.sort_by_key(|(k, _)| rank(&map.priority_dpcm, *k));
        if noi_hits.len() > 1 { report.noi_conflicts += 1; }
        noi_hits.sort_by_key(|(k, _, _)| rank(&map.priority_noi, *k));
        if let Some((_, slot)) = dpcm_hits.first() {
            grid[*r][4] = Cell { note: Some(60 + slot), instr: 0, vol: 0, fx: None, hold: false };
        }
        // losers on the DPCM slot may fall back to the noise channel
        for (k, _) in dpcm_hits.iter().skip(1) {
            if let Some(DrumTarget::Noi { note, instr }) = map.fallback.get(k) {
                if noi_hits.is_empty() {
                    noi_hits.push((*k, *note, *instr));
                    report.drum_fallbacks += 1;
                } else {
                    report.drum_dropped += 1;
                }
            } else {
                report.drum_dropped += 1;
            }
        }
        if let Some((_, note, instr)) = noi_hits.first() {
            grid[*r][3] = Cell { note: Some(*note), instr: *instr, vol: 0, fx: None, hold: false };
        }
    }

    // --- phrases + order (dedupe identical phrases)
    let mut song = Song::default();
    song.bpm = bpm;
    song.phrases.clear();
    let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
    for p in 0..nphr {
        let mut ph = Phrase::default();
        for s in 0..STEPS_PER_PHRASE {
            ph.cells[s] = grid[p * STEPS_PER_PHRASE + s];
        }
        let key: Vec<u8> = ph.cells.iter().flatten().flat_map(|c| [c.note.unwrap_or(if c.hold { 0xFE } else { 0xFF }), c.instr, c.vol, c.fx.map(|f| f.0).unwrap_or(0), c.fx.map(|f| f.1).unwrap_or(0)]).collect();
        let idx = *index.entry(key).or_insert_with(|| {
            song.phrases.push(ph.clone());
            song.phrases.len() - 1
        });
        song.order.push(idx);
    }
    report.phrases_total = nphr;
    report.phrases_unique = song.phrases.len();
    if song.phrases.len() > 256 {
        bail!("{} unique phrases; the .vip format holds 256", song.phrases.len());
    }
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

    /// Build a tiny format-1 SMF: tempo + one named track of (tick, len, key) notes.
    fn smf(bpm: u32, tracks: &[(&str, u8, &[(u64, u64, u8)])]) -> Vec<u8> {
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
            for (t, l, k) in notes.iter() {
                ev.push((*t, vec![0x90 | ch, *k, 100]));
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
}
