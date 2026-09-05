//! Minimal Standard MIDI File writer for drum triggers: one format-0
//! track, channel 10, fixed-length notes.

use std::io::Write;

pub struct DrumHit {
    /// Time in seconds from the start of the render.
    pub time_s: f64,
    pub note: u8,
    pub velocity: u8,
}

fn vlq(buf: &mut Vec<u8>, mut n: u32) {
    let mut bytes = vec![(n & 0x7F) as u8];
    n >>= 7;
    while n > 0 {
        bytes.push((n & 0x7F) as u8 | 0x80);
        n >>= 7;
    }
    bytes.reverse();
    buf.extend_from_slice(&bytes);
}

/// Write hits at `bpm` with 480 ticks per quarter. Each note lasts a 32nd.
pub fn write_drum_midi<W: Write>(mut w: W, bpm: f64, hits: &[DrumHit]) -> std::io::Result<()> {
    const TPQ: u32 = 480;
    let ticks_per_sec = TPQ as f64 * bpm / 60.0;
    let dur = TPQ / 8;
    // Build (tick, on/off, note, vel) events, sorted.
    let mut ev: Vec<(u32, bool, u8, u8)> = Vec::new();
    for h in hits {
        let t = (h.time_s * ticks_per_sec).round() as u32;
        ev.push((t, true, h.note, h.velocity));
        ev.push((t + dur, false, h.note, 0));
    }
    ev.sort_by_key(|e| (e.0, e.1)); // offs before ons at the same tick
    let mut track = Vec::new();
    // tempo meta
    vlq(&mut track, 0);
    let us_per_q = (60_000_000.0 / bpm).round() as u32;
    track.extend_from_slice(&[0xFF, 0x51, 0x03, (us_per_q >> 16) as u8, (us_per_q >> 8) as u8, us_per_q as u8]);
    let mut last = 0u32;
    for (t, on, note, vel) in ev {
        vlq(&mut track, t - last);
        last = t;
        track.push(if on { 0x99 } else { 0x89 });
        track.push(note);
        track.push(vel);
    }
    vlq(&mut track, 0);
    track.extend_from_slice(&[0xFF, 0x2F, 0x00]);

    w.write_all(b"MThd")?;
    w.write_all(&6u32.to_be_bytes())?;
    w.write_all(&0u16.to_be_bytes())?;
    w.write_all(&1u16.to_be_bytes())?;
    w.write_all(&(TPQ as u16).to_be_bytes())?;
    w.write_all(b"MTrk")?;
    w.write_all(&(track.len() as u32).to_be_bytes())?;
    w.write_all(&track)
}
