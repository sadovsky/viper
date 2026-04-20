//! Stage 15b: MIDI export.
//!
//! Walks the current phrase one pass and emits a format-1 Standard MIDI File
//! with a conductor track (tempo) plus four instrument tracks — one per
//! channel. PU1/PU2/TRI go out on MIDI channels 0/1/2; NOI goes out on
//! channel 9 (GM drum map) with a small pitch→drum-slot remap so kick/snare/
//! hat land on sensible GM notes in any DAW. No external midly dep —
//! Standard MIDI File is compact enough to hand-write.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};

use crate::{Cell, Instrument, Phrase, CHANNELS, INSTRUMENTS, STEPS_PER_PHRASE};

/// Ticks per quarter-note. 24 ticks per 16th means every step lands on a
/// clean grid boundary at any BPM.
const PPQN: u16 = 96;
const TICKS_PER_STEP: u32 = (PPQN / 4) as u32;

/// Remap a viper NOI pitch to a GM drum slot. Low = kick, mid = snare,
/// high = hi-hat. Keeps the demo song's 36/50/60 notes landing on
/// 36/38/42 in GM, which is what users expect in a DAW.
fn noi_to_gm_drum(pitch: u8) -> u8 {
    match pitch {
        0..=44 => 36,   // Bass Drum 1
        45..=54 => 38,  // Acoustic Snare
        _ => 42,        // Closed Hi-Hat
    }
}

#[derive(Clone, Copy, Debug)]
struct TrackEvent {
    tick: u32,
    /// Lower ordering at the same tick = emit first. Note-offs before
    /// note-ons so same-tick retrigger chains don't eat each other.
    order: u8,
    kind: EventKind,
}

#[derive(Clone, Copy, Debug)]
enum EventKind {
    NoteOn { channel: u8, note: u8, vel: u8 },
    NoteOff { channel: u8, note: u8 },
    EndOfTrack,
}

pub fn export_phrase_to_midi(
    path: &Path,
    phrase: &Phrase,
    _instruments: &[Instrument; INSTRUMENTS],
    bpm: u16,
    loops: u32,
) -> Result<()> {
    let loops = loops.max(1);
    let f = File::create(path).with_context(|| format!("midi: create {}", path.display()))?;
    let mut w = BufWriter::new(f);

    // --- Header chunk (MThd) ---
    let ntrks: u16 = 1 /* conductor */ + CHANNELS as u16;
    w.write_all(b"MThd")?;
    w.write_all(&6u32.to_be_bytes())?;   // chunk length
    w.write_all(&1u16.to_be_bytes())?;   // format 1
    w.write_all(&ntrks.to_be_bytes())?;
    w.write_all(&PPQN.to_be_bytes())?;

    // --- Conductor track (tempo + end) ---
    write_conductor_track(&mut w, bpm)?;

    // --- Instrument tracks (one per channel) ---
    for ch in 0..CHANNELS {
        let events = collect_channel_events(phrase, ch, loops);
        write_track(&mut w, &events)?;
    }

    w.flush()?;
    Ok(())
}

fn collect_channel_events(phrase: &Phrase, ch: usize, loops: u32) -> Vec<TrackEvent> {
    let is_noi = ch == 3;
    let midi_channel: u8 = if is_noi { 9 } else { ch as u8 };
    let mut events: Vec<TrackEvent> = Vec::new();

    for loop_i in 0..loops {
        let base_tick = loop_i * STEPS_PER_PHRASE as u32 * TICKS_PER_STEP;
        for step in 0..STEPS_PER_PHRASE {
            let cell: Cell = phrase.cells[step][ch];
            let Some(raw) = cell.note else { continue; };
            let note = if is_noi { noi_to_gm_drum(raw) } else { raw };
            let vel = if cell.vol == 0 { 100 } else {
                ((cell.vol as u32) * 127 / 15).min(127) as u8
            };
            let on_tick = base_tick + step as u32 * TICKS_PER_STEP;
            let off_tick = on_tick + TICKS_PER_STEP;
            events.push(TrackEvent {
                tick: on_tick,
                order: 1,
                kind: EventKind::NoteOn { channel: midi_channel, note, vel },
            });
            events.push(TrackEvent {
                tick: off_tick,
                order: 0,
                kind: EventKind::NoteOff { channel: midi_channel, note },
            });
        }
    }

    // Stable sort by (tick, order): note-offs before note-ons at the same
    // tick so a retrigger of the same pitch doesn't drop the new note-on
    // into an already-released state.
    events.sort_by_key(|e| (e.tick, e.order));

    let last_tick = events.last().map(|e| e.tick).unwrap_or(0);
    events.push(TrackEvent {
        tick: last_tick,
        order: 9,
        kind: EventKind::EndOfTrack,
    });
    events
}

fn write_conductor_track<W: Write>(w: &mut W, bpm: u16) -> Result<()> {
    // Microseconds per quarter-note.
    let us_per_q: u32 = (60_000_000f32 / bpm.max(1) as f32) as u32;
    let mut body: Vec<u8> = Vec::new();
    // delta = 0, FF 51 03 tt tt tt
    write_vlq(&mut body, 0);
    body.extend_from_slice(&[0xFF, 0x51, 0x03]);
    body.extend_from_slice(&us_per_q.to_be_bytes()[1..4]);
    // End of track
    write_vlq(&mut body, 0);
    body.extend_from_slice(&[0xFF, 0x2F, 0x00]);

    w.write_all(b"MTrk")?;
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(&body)?;
    Ok(())
}

fn write_track<W: Write>(w: &mut W, events: &[TrackEvent]) -> Result<()> {
    let mut body: Vec<u8> = Vec::new();
    let mut last_tick: u32 = 0;
    for e in events {
        let delta = e.tick.saturating_sub(last_tick);
        last_tick = e.tick;
        write_vlq(&mut body, delta);
        match e.kind {
            EventKind::NoteOn { channel, note, vel } => {
                body.push(0x90 | (channel & 0x0F));
                body.push(note & 0x7F);
                body.push(vel & 0x7F);
            }
            EventKind::NoteOff { channel, note } => {
                body.push(0x80 | (channel & 0x0F));
                body.push(note & 0x7F);
                body.push(64); // release velocity
            }
            EventKind::EndOfTrack => {
                body.extend_from_slice(&[0xFF, 0x2F, 0x00]);
            }
        }
    }
    w.write_all(b"MTrk")?;
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(&body)?;
    Ok(())
}

/// Variable-length quantity used throughout SMF for delta-times and meta
/// lengths. 7 data bits per byte, continuation bit set on all but the last.
fn write_vlq(buf: &mut Vec<u8>, mut n: u32) {
    let mut bytes = [0u8; 5];
    let mut i = 0;
    bytes[i] = (n & 0x7F) as u8;
    n >>= 7;
    while n > 0 {
        i += 1;
        bytes[i] = ((n & 0x7F) as u8) | 0x80;
        n >>= 7;
    }
    // Written MSB-first.
    while i > 0 {
        buf.push(bytes[i]);
        i -= 1;
    }
    buf.push(bytes[0]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_phrase() -> Phrase {
        let mut p = Phrase::default();
        p.cells[0][0] = Cell { note: Some(60), instr: 0, vol: 0, fx: None };  // PU1 C4
        p.cells[4][0] = Cell { note: Some(64), instr: 0, vol: 0, fx: None };  // PU1 E4
        p.cells[0][3] = Cell { note: Some(36), instr: 0, vol: 0, fx: None };  // NOI kick
        p.cells[4][3] = Cell { note: Some(50), instr: 0, vol: 0, fx: None };  // NOI snare
        p.cells[8][3] = Cell { note: Some(60), instr: 0, vol: 0, fx: None };  // NOI hat
        p
    }

    #[test]
    fn vlq_encoding_roundtrips_standard_examples() {
        // Examples from the SMF spec.
        let cases = [
            (0u32, vec![0x00]),
            (0x40, vec![0x40]),
            (0x7F, vec![0x7F]),
            (0x80, vec![0x81, 0x00]),
            (0x2000, vec![0xC0, 0x00]),
            (0x1FFFFF, vec![0xFF, 0xFF, 0x7F]),
        ];
        for (val, want) in cases {
            let mut buf = Vec::new();
            write_vlq(&mut buf, val);
            assert_eq!(buf, want, "VLQ({:#x})", val);
        }
    }

    #[test]
    fn noi_mapping_matches_demo_intent() {
        assert_eq!(noi_to_gm_drum(36), 36);  // kick → kick
        assert_eq!(noi_to_gm_drum(50), 38);  // midway → snare
        assert_eq!(noi_to_gm_drum(60), 42);  // high → hat
    }

    #[test]
    fn midi_export_writes_valid_header_and_tracks() {
        let path = std::env::temp_dir().join("viper_midi_test.mid");
        let _ = std::fs::remove_file(&path);
        let instr = [Instrument::default(); INSTRUMENTS];
        export_phrase_to_midi(&path, &test_phrase(), &instr, 140, 1)
            .expect("midi export");
        let bytes = std::fs::read(&path).expect("read back");
        assert_eq!(&bytes[..4], b"MThd");
        // format = 1
        assert_eq!(bytes[8], 0);
        assert_eq!(bytes[9], 1);
        // ntrks = 5 (conductor + 4)
        assert_eq!(bytes[10], 0);
        assert_eq!(bytes[11], 5);
        // division = 96
        assert_eq!(u16::from_be_bytes([bytes[12], bytes[13]]), PPQN);
        // Exactly 5 MTrk markers in the file.
        let trk_count = bytes.windows(4).filter(|w| *w == b"MTrk").count();
        assert_eq!(trk_count, 5);
        let _ = std::fs::remove_file(&path);
    }
}
