//! Serialize a [`Module`] against a [`Driver`] into a complete NSF.
//!
//! Image layout ($8000-based, single 32 KB bank, no bankswitching):
//!
//! ```text
//! $8000  driver.bin
//!        song_table: N words
//!        per song: header, order list, instrument table, envelopes,
//!                  DPCM table, channel streams (deduplicated)
//! $C000  DPCM sample data, 64-byte aligned, 16n+1 bytes each
//! ```

use crate::ir::*;
use crate::Driver;
use anyhow::{bail, Result};
use std::collections::HashMap;

const OP_OFF: u8 = 0x60;
const OP_VOL: u8 = 0x61;
const OP_DUTY: u8 = 0x62;
const OP_INSTR: u8 = 0x63;
const OP_RETRIG: u8 = 0x64;
const OP_SLIDE: u8 = 0x65;
const OP_VIBRATO: u8 = 0x66;
const OP_ARP: u8 = 0x67;
const OP_ENV_RESET: u8 = 0x68;
const OP_ROW_END: u8 = 0x80;

const DPCM_BASE: usize = 0xC000;
const IMAGE_END: usize = 0x10000;

#[derive(Debug)]
pub struct EmitResult {
    pub nsf: Vec<u8>,
    /// Bytes of song data (everything after the driver, before samples).
    pub data_bytes: usize,
    pub sample_bytes: usize,
    pub warnings: Vec<String>,
}

fn encode_event(e: &Event, out: &mut Vec<u8>) {
    match *e {
        Event::Note(n) => out.push(n.min(0x5F)),
        Event::Off => out.push(OP_OFF),
        Event::Vol(v) => out.extend([OP_VOL, v.min(15)]),
        Event::Duty(d) => out.extend([OP_DUTY, d & 3]),
        Event::Instr(i) => out.extend([OP_INSTR, i]),
        Event::Retrig(r) => out.extend([OP_RETRIG, r]),
        Event::Slide(s) => out.extend([OP_SLIDE, s]),
        Event::Vibrato { depth, rate } => out.extend([OP_VIBRATO, (depth.min(15) << 4) | rate.min(15)]),
        Event::Arp { x, y } => out.extend([OP_ARP, (x.min(15) << 4) | y.min(15)]),
        Event::EnvReset => out.push(OP_ENV_RESET),
    }
}

/// Encode one channel's rows of a pattern into a stream. Events must be
/// ordered so state changes precede notes; the caller (lowering) owns
/// that. Empty rows compress into the previous ROW_END's skip count.
pub fn encode_stream(rows: &[Vec<Event>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        for e in &rows[i] {
            encode_event(e, &mut out);
        }
        // count following empty rows (max 63 per ROW_END)
        let mut skip = 0usize;
        while i + 1 + skip < rows.len() && rows[i + 1 + skip].is_empty() && skip < 63 {
            skip += 1;
        }
        out.push(OP_ROW_END | skip as u8);
        i += 1 + skip;
    }
    out
}

fn fixed_8_8(frames: f64) -> Result<(u8, u8)> {
    if frames < 1.0 {
        bail!("frames per row {:.3} is below 1 (BPM too high for 16th-note rows)", frames);
    }
    if frames >= 256.0 {
        bail!("frames per row {:.1} exceeds 255 (BPM too low)", frames);
    }
    let fixed = (frames * 256.0).round() as u32;
    Ok(((fixed & 0xFF) as u8, (fixed >> 8) as u8))
}

pub fn emit(module: &Module, driver: &Driver) -> Result<EmitResult> {
    if module.songs.is_empty() {
        bail!("module has no songs");
    }
    if module.songs.len() > 255 {
        bail!("NSF holds at most 255 songs");
    }
    let mut warnings = Vec::new();
    let base = driver.load as usize;
    let mut image: Vec<u8> = Vec::with_capacity(0x8000);
    image.extend_from_slice(&driver.bin);
    debug_assert_eq!(base + image.len(), driver.song_table as usize);

    // --- DPCM samples: collect across songs, dedupe by content ---
    // Placed at $C000+ after we know everything else fits.
    let mut sample_blobs: Vec<Vec<u8>> = Vec::new();
    let mut sample_index: HashMap<Vec<u8>, usize> = HashMap::new();
    // per song: sample id -> global blob index
    let mut song_sample_map: Vec<Vec<usize>> = Vec::new();
    for song in &module.songs {
        let mut map = Vec::new();
        for s in &song.samples {
            let mut data = s.data.clone();
            if data.is_empty() {
                data.push(0x55);
            }
            // pad to 16n+1
            let want = ((data.len() - 1 + 15) / 16) * 16 + 1;
            data.resize(want, 0x55);
            if data.len() > 0xFF1 {
                bail!("DPCM sample `{}` is {} bytes; max is 4081", s.name, data.len());
            }
            let idx = *sample_index.entry(data.clone()).or_insert_with(|| {
                sample_blobs.push(data);
                sample_blobs.len() - 1
            });
            map.push(idx);
        }
        song_sample_map.push(map);
    }
    // assign sample addresses
    let mut sample_addr: Vec<usize> = Vec::new();
    let mut cursor = DPCM_BASE;
    for blob in &sample_blobs {
        cursor = (cursor + 63) & !63;
        sample_addr.push(cursor);
        cursor += blob.len();
    }
    if cursor > IMAGE_END {
        bail!("DPCM samples need {} bytes at $C000; only 16384 available", cursor - DPCM_BASE);
    }
    let sample_end = cursor;

    // --- song table placeholder ---
    let table_at = image.len();
    image.resize(table_at + 2 * module.songs.len(), 0);

    for (si, song) in module.songs.iter().enumerate() {
        if song.order.is_empty() {
            bail!("song {} has an empty order list", si);
        }
        if song.order.len() > 255 {
            bail!("song {}: order list longer than 255", si);
        }
        if song.loop_pos >= song.order.len() {
            bail!("song {}: loop position {} is past the order list", si, song.loop_pos);
        }
        if song.rows_per_pattern == 0 {
            bail!("song {}: rows_per_pattern is 0", si);
        }
        for (pi, p) in song.patterns.iter().enumerate() {
            if p.rows.len() != song.rows_per_pattern as usize {
                bail!("song {}: pattern {} has {} rows, expected {}", si, pi, p.rows.len(), song.rows_per_pattern);
            }
        }
        for &o in &song.order {
            if o >= song.patterns.len() {
                bail!("song {}: order references pattern {} but there are {}", si, o, song.patterns.len());
            }
        }
        if song.instruments.len() > 64 {
            bail!("song {}: more than 64 instruments", si);
        }
        let song_addr = base + image.len();
        image[table_at + 2 * si] = (song_addr & 0xFF) as u8;
        image[table_at + 2 * si + 1] = (song_addr >> 8) as u8;

        // header
        let (sp_lo, sp_hi) = fixed_8_8(song.frames_per_row)?;
        image.extend([sp_lo, sp_hi, song.rows_per_pattern, song.order.len() as u8, song.loop_pos as u8]);
        let instr_ptr_at = image.len();
        image.extend([0, 0, 0, 0]); // instr ptr, dpcm ptr
        // order list placeholder: 10 bytes per entry
        let order_at = image.len();
        image.resize(order_at + 10 * song.order.len(), 0);

        // envelopes + instrument table
        let mut env_addr: Vec<Option<usize>> = Vec::new();
        for ins in &song.instruments {
            match &ins.envelope {
                Some(env) => {
                    if env.values.is_empty() || env.values.len() > 252 {
                        bail!("song {}: envelope length {} out of range 1..=252", si, env.values.len());
                    }
                    env_addr.push(Some(base + image.len()));
                    image.extend(env.encode());
                }
                None => env_addr.push(None),
            }
        }
        let instr_table = base + image.len();
        for (i, ins) in song.instruments.iter().enumerate() {
            let e = env_addr[i].unwrap_or(0);
            image.extend([ins.duty.map(|d| d & 3).unwrap_or(0xFF), (e & 0xFF) as u8, (e >> 8) as u8, 0]);
        }
        if song.instruments.is_empty() {
            // one default instrument so INSTR 0 is always valid
            image.extend([0xFF, 0, 0, 0]);
        }
        // dpcm table
        let dpcm_table = base + image.len();
        for (k, s) in song.samples.iter().enumerate() {
            let gi = song_sample_map[si][k];
            let addr = sample_addr[gi];
            let len = sample_blobs[gi].len();
            let rate = (s.rate & 0x0F) | if s.loop_ { 0x40 } else { 0 };
            image.extend([rate, ((addr - 0xC000) >> 6) as u8, ((len - 1) >> 4) as u8, 0]);
        }
        if song.samples.is_empty() {
            image.extend([0x0F, 0, 0, 0]);
        }
        image[instr_ptr_at] = (instr_table & 0xFF) as u8;
        image[instr_ptr_at + 1] = (instr_table >> 8) as u8;
        image[instr_ptr_at + 2] = (dpcm_table & 0xFF) as u8;
        image[instr_ptr_at + 3] = (dpcm_table >> 8) as u8;

        // streams, deduplicated by content
        let mut stream_addr: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut pattern_streams: Vec<[usize; 5]> = Vec::new();
        for p in &song.patterns {
            let mut addrs = [0usize; 5];
            for (ci, _) in CHANNELS.iter().enumerate() {
                let rows: Vec<Vec<Event>> = p.rows.iter().map(|r| r[ci].clone()).collect();
                let bytes = encode_stream(&rows);
                let a = *stream_addr.entry(bytes.clone()).or_insert_with(|| {
                    let a = base + image.len();
                    image.extend(&bytes);
                    a
                });
                addrs[ci] = a;
            }
            pattern_streams.push(addrs);
        }
        for (oi, &pi) in song.order.iter().enumerate() {
            for ci in 0..5 {
                let a = pattern_streams[pi][ci];
                let at = order_at + oi * 10 + ci * 2;
                image[at] = (a & 0xFF) as u8;
                image[at + 1] = (a >> 8) as u8;
            }
        }
        if base + image.len() > DPCM_BASE && !sample_blobs.is_empty() {
            bail!("song data reaches ${:04X}, colliding with DPCM at $C000", base + image.len());
        }
        if base + image.len() > IMAGE_END {
            bail!("song data exceeds the 32 KB image");
        }
    }
    let data_bytes = image.len() - driver.bin.len();

    // --- samples ---
    let sample_bytes = if sample_blobs.is_empty() {
        0
    } else {
        image.resize(DPCM_BASE - base, 0);
        for (i, blob) in sample_blobs.iter().enumerate() {
            let off = sample_addr[i] - base;
            if image.len() < off {
                image.resize(off, 0);
            }
            image.extend_from_slice(blob);
        }
        sample_end - DPCM_BASE
    };
    if image.len() > 0x8000 {
        bail!("image is {} bytes; max 32768", image.len());
    }
    if module.expansion != Expansion::None {
        warnings.push("expansion audio requested but the ABI v1 driver is strict 2A03; header flag set anyway".into());
    }

    let title = module.songs[0].title.as_str();
    let header = viper_nsf_header(
        driver.load,
        driver.init,
        driver.play,
        module.songs.len() as u8,
        title,
        &module.artist,
        &module.copyright,
        module.expansion.nsf_bits(),
    );
    let mut nsf = Vec::with_capacity(128 + image.len());
    nsf.extend_from_slice(&header);
    nsf.extend_from_slice(&image);
    Ok(EmitResult { nsf, data_bytes, sample_bytes, warnings })
}

fn viper_nsf_header(load: u16, init: u16, play: u16, songs: u8, name: &str, artist: &str, copyright: &str, expansion: u8) -> [u8; 128] {
    let mut h = [0u8; 128];
    h[0..5].copy_from_slice(b"NESM\x1A");
    h[5] = 1;
    h[6] = songs;
    h[7] = 1;
    h[8..10].copy_from_slice(&load.to_le_bytes());
    h[10..12].copy_from_slice(&init.to_le_bytes());
    h[12..14].copy_from_slice(&play.to_le_bytes());
    let put = |h: &mut [u8; 128], o: usize, s: &str| {
        let b = s.as_bytes();
        let n = b.len().min(31);
        h[o..o + n].copy_from_slice(&b[..n]);
    };
    put(&mut h, 0x0E, name);
    put(&mut h, 0x2E, artist);
    put(&mut h, 0x4E, copyright);
    h[0x6E..0x70].copy_from_slice(&0x411Au16.to_le_bytes());
    h[0x78..0x7A].copy_from_slice(&0x4E20u16.to_le_bytes());
    h[0x7B] = expansion;
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_compresses_empty_rows() {
        let rows: Vec<Vec<Event>> = vec![vec![Event::Instr(0), Event::Note(40)], vec![], vec![], vec![Event::Off]];
        assert_eq!(encode_stream(&rows), vec![0x63, 0, 40, 0x82, 0x60, 0x80]);
        let empty: Vec<Vec<Event>> = vec![vec![]; 16];
        assert_eq!(encode_stream(&empty), vec![0x8F]);
    }

    #[test]
    fn adsr_envelope_shape() {
        let e = Envelope::from_adsr(0, 100, 0.5, 50, 1.0);
        assert_eq!(e.values[0], 15);
        let lp = e.loop_point.unwrap() as usize;
        assert_eq!(e.values[lp], 8);
        assert_eq!(*e.values.last().unwrap(), 0);
        assert!(e.release_point.unwrap() as usize > lp);
        let silent = Envelope::from_adsr(0, 60, 0.0, 20, 0.6);
        assert_eq!(silent.loop_point, None);
        assert_eq!(silent.values[0], 9);
    }

    #[test]
    fn fixed_point_speed() {
        assert_eq!(fixed_8_8(4.5).unwrap(), (0x80, 0x04));
        assert!(fixed_8_8(0.9).is_err());
    }
}
