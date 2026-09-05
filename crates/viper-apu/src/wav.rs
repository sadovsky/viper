//! 16-bit PCM mono WAV writer.

use std::io::Write;

pub fn write_wav<W: Write>(mut w: W, sample_rate: u32, samples: &[f32]) -> std::io::Result<()> {
    let data_size = (samples.len() * 2) as u32;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_size).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&(sample_rate * 2).to_le_bytes())?;
    w.write_all(&2u16.to_le_bytes())?;
    w.write_all(&16u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_size.to_le_bytes())?;
    let mut buf = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        buf.extend_from_slice(&v.to_le_bytes());
    }
    w.write_all(&buf)
}

/// Read a RIFF/WAVE file to mono f32 in −1..1 and its sample rate. Accepts
/// PCM 8/16/24/32-bit and IEEE float32, any channel count (averaged),
/// and skips unknown chunks (LIST, fact, ...).
pub fn read_wav(bytes: &[u8]) -> std::io::Result<(u32, Vec<f32>)> {
    use std::io::{Error, ErrorKind};
    let bad = |m: &str| Error::new(ErrorKind::InvalidData, m.to_string());
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("not a RIFF/WAVE file"));
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // tag, channels, rate, bits
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]) as usize;
        let body_end = (pos + 8 + len).min(bytes.len());
        let body = &bytes[pos + 8..body_end];
        match id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err(bad("fmt chunk too short"));
                }
                let tag = u16::from_le_bytes([body[0], body[1]]);
                let ch = u16::from_le_bytes([body[2], body[3]]);
                let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let bits = u16::from_le_bytes([body[14], body[15]]);
                let tag = if tag == 0xFFFE && body.len() >= 26 { u16::from_le_bytes([body[24], body[25]]) } else { tag };
                fmt = Some((tag, ch, rate, bits));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        pos = pos + 8 + len + (len & 1);
    }
    let (tag, ch, rate, bits) = fmt.ok_or_else(|| bad("no fmt chunk"))?;
    let data = data.ok_or_else(|| bad("no data chunk"))?;
    if ch == 0 || rate == 0 {
        return Err(bad("zero channels or sample rate"));
    }
    let bps = (bits / 8) as usize;
    if bps == 0 {
        return Err(bad("zero bits per sample"));
    }
    let frames = data.len() / (bps * ch as usize);
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut acc = 0f32;
        for c in 0..ch as usize {
            let o = (f * ch as usize + c) * bps;
            let s = &data[o..o + bps];
            let v = match (tag, bits) {
                (3, 32) => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
                (3, 64) => f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]) as f32,
                (1, 8) => (s[0] as f32 - 128.0) / 128.0,
                (1, 16) => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
                (1, 24) => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
                (1, 32) => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
                _ => return Err(bad("unsupported WAV format (need PCM 8/16/24/32 or float32)")),
            };
            acc += v;
        }
        out.push(acc / ch as f32);
    }
    Ok((rate, out))
}

#[cfg(test)]
mod read_tests {
    use super::*;

    #[test]
    fn read_roundtrip_and_odd_formats() {
        let src: Vec<f32> = (0..1000).map(|i| ((i as f32) * 0.05).sin() * 0.8).collect();
        let mut buf = Vec::new();
        write_wav(&mut buf, 22050, &src).unwrap();
        let (rate, back) = read_wav(&buf).unwrap();
        assert_eq!(rate, 22050);
        assert_eq!(back.len(), src.len());
        for (a, b) in src.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1.0e-4);
        }
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&8000u32.to_le_bytes());
        w.extend_from_slice(&16000u32.to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&8u16.to_le_bytes());
        w.extend_from_slice(b"LIST");
        w.extend_from_slice(&3u32.to_le_bytes());
        w.extend_from_slice(&[1, 2, 3, 0]);
        w.extend_from_slice(b"data");
        w.extend_from_slice(&4u32.to_le_bytes());
        w.extend_from_slice(&[255, 128, 0, 128]);
        let (rate, m) = read_wav(&w).unwrap();
        assert_eq!(rate, 8000);
        assert_eq!(m.len(), 2);
        assert!((m[0] - 0.496).abs() < 0.01 && (m[1] + 0.5).abs() < 0.01);
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&3u16.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&44100u32.to_le_bytes());
        w.extend_from_slice(&176400u32.to_le_bytes());
        w.extend_from_slice(&4u16.to_le_bytes());
        w.extend_from_slice(&32u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&8u32.to_le_bytes());
        w.extend_from_slice(&0.25f32.to_le_bytes());
        w.extend_from_slice(&(-0.5f32).to_le_bytes());
        let (_, m) = read_wav(&w).unwrap();
        assert_eq!(m, vec![0.25, -0.5]);
    }
}
