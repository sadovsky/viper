//! Built-in DPCM drum bank and 1-bit delta encoding.
//!
//! viper ships three synthesized samples (kick, snare, hat) so a song can
//! use the DPCM column without shipping sample files. `@dpcm` entries in
//! a `.vip` override them. The float waveforms also drive the internal
//! synth's DPCM preview voice so what you hear while editing is the same
//! material the NSF plays.

/// DMC rate index 15 on NTSC.
pub const RATE_HZ: f32 = 33_143.9;
pub const RATE_INDEX: u8 = 15;

pub struct Sample {
    pub name: String,
    pub wave: Vec<f32>,
    /// Playback rate of `wave` in Hz (the DMC rate it was made for).
    pub rate_hz: f32,
}

fn kick(sr: f32) -> Vec<f32> {
    let n = (sr * 0.09) as usize;
    let mut w = Vec::with_capacity(n);
    let mut ph = 0.0f32;
    for i in 0..n {
        let t = i as f32 / sr;
        let f = 40.0 + 180.0 * (-t * 35.0).exp();
        ph += 2.0 * std::f32::consts::PI * f / sr;
        let env = (-t * 22.0).exp();
        let click = (-t * 600.0).exp() * 0.6;
        w.push((ph.sin() * env + click).clamp(-1.0, 1.0));
    }
    w
}

/// Deterministic xorshift so the bank is bit-identical across builds.
struct Rng(u32);
impl Rng {
    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}

fn snare(sr: f32) -> Vec<f32> {
    let n = (sr * 0.11) as usize;
    let mut rng = Rng(0x2545F491);
    let mut w = Vec::with_capacity(n);
    let mut ph = 0.0f32;
    let mut lp = 0.0f32;
    for i in 0..n {
        let t = i as f32 / sr;
        let noise = rng.next();
        lp += (noise - lp) * 0.6;
        ph += 2.0 * std::f32::consts::PI * 185.0 / sr;
        let body = ph.sin() * (-t * 60.0).exp() * 0.5;
        let env = (-t * 28.0).exp();
        w.push((lp * env * 0.9 + body).clamp(-1.0, 1.0));
    }
    w
}

fn hat(sr: f32) -> Vec<f32> {
    let n = (sr * 0.03) as usize;
    let mut rng = Rng(0x9E3779B9);
    let mut w = Vec::with_capacity(n);
    let (mut prev, mut hp_prev) = (0.0f32, 0.0f32);
    for i in 0..n {
        let t = i as f32 / sr;
        let x = rng.next();
        let hp = x - prev + 0.7 * hp_prev;
        prev = x;
        hp_prev = hp;
        w.push((hp * (-t * 120.0).exp()).clamp(-1.0, 1.0));
    }
    w
}

pub fn default_bank() -> Vec<Sample> {
    vec![
        Sample { name: "kick".into(), wave: kick(RATE_HZ), rate_hz: RATE_HZ },
        Sample { name: "snare".into(), wave: snare(RATE_HZ), rate_hz: RATE_HZ },
        Sample { name: "hat".into(), wave: hat(RATE_HZ), rate_hz: RATE_HZ },
    ]
}

fn tom(sr: f32) -> Vec<f32> {
    let n = (sr * 0.14) as usize;
    let mut w = Vec::with_capacity(n);
    let mut ph = 0.0f32;
    for i in 0..n {
        let t = i as f32 / sr;
        let f = 70.0 + 50.0 * (-t * 25.0).exp();
        ph += 2.0 * std::f32::consts::PI * f / sr;
        let env = (-t * 18.0).exp();
        let click = (-t * 400.0).exp() * 0.4;
        w.push((ph.sin() * env + click).clamp(-1.0, 1.0));
    }
    w
}

/// Built-in recipes as source material for the encoder / workbench.
pub fn synth(name: &str, sr: f32) -> Option<Vec<f32>> {
    match name {
        "kick" => Some(kick(sr)),
        "snare" => Some(snare(sr)),
        "hat" => Some(hat(sr)),
        "tom" => Some(tom(sr)),
        _ => None,
    }
}

pub const SYNTH_NAMES: [&str; 4] = ["kick", "snare", "hat", "tom"];

/// Output rate in Hz of a DMC rate index (NTSC).
pub fn rate_hz(idx: u8) -> f32 {
    1_789_773.0 / viper_apu::apu::DMC_RATE[(idx & 15) as usize] as f32
}

// ---------------------------------------------------------------- encoder

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Viterbi search over the 64 reachable DAC levels: L2-optimal, sees
    /// transients coming, returns to the start level at the end.
    Trellis,
    /// Sign-of-error per bit, no end-level guarantee. For A/B only.
    Greedy,
}

#[derive(Clone, Debug)]
pub struct EncodeOptions {
    pub rate: u8,
    /// Peak scaling; > 1 clips against the DAC rails on purpose.
    pub gain: f32,
    /// DAC level the sample starts and ends at (what $4011 holds).
    pub level: u8,
    pub mode: Mode,
    /// High-pass corner in Hz after DC removal; 0 = off.
    pub hp_hz: f32,
    /// Silence threshold for trimming, dB below peak; None = no trim.
    pub trim_db: Option<f32>,
    pub max_bytes: usize,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self { rate: 15, gain: 1.0, level: 64, mode: Mode::Trellis, hp_hz: 20.0, trim_db: Some(-60.0), max_bytes: 4081 }
    }
}

#[derive(Clone, Debug, Default)]
pub struct EncodeStats {
    pub bytes: usize,
    pub seconds: f32,
    pub min_level: u8,
    pub max_level: u8,
    pub clamps: usize,
    pub snr_db: f32,
    pub snr_greedy_db: f32,
}

#[derive(Clone, Debug)]
pub struct Encoded {
    pub data: Vec<u8>,
    pub stats: EncodeStats,
}

/// Steps before quantization: trim, DC/HP, resample to the DMC rate,
/// normalize. Returns absolute DAC-unit targets (0..127 scale).
pub fn prepare(wave: &[f32], src_hz: f32, opts: &EncodeOptions) -> Vec<f32> {
    let mut x: Vec<f32> = wave.to_vec();
    if x.is_empty() {
        return Vec::new();
    }
    // trim
    if let Some(db) = opts.trim_db {
        let peak = x.iter().fold(0f32, |m, v| m.max(v.abs()));
        let thr = peak * 10f32.powf(db / 20.0);
        let first = x.iter().position(|v| v.abs() > thr).unwrap_or(0);
        let last = x.iter().rposition(|v| v.abs() > thr).unwrap_or(x.len() - 1);
        let pre = (src_hz * 0.001) as usize;
        let post = (src_hz * 0.005) as usize;
        let a = first.saturating_sub(pre);
        let b = (last + post + 1).min(x.len());
        x = x[a..b].to_vec();
    }
    // DC + high-pass
    let mean = x.iter().sum::<f32>() / x.len() as f32;
    for v in x.iter_mut() {
        *v -= mean;
    }
    if opts.hp_hz > 0.0 {
        let a = (-2.0 * std::f32::consts::PI * opts.hp_hz / src_hz).exp();
        let (mut prev_in, mut prev_out) = (0f32, 0f32);
        for v in x.iter_mut() {
            let y = a * (prev_out + *v - prev_in);
            prev_in = *v;
            prev_out = y;
            *v = y;
        }
    }
    // resample
    let dst_hz = rate_hz(opts.rate);
    if (src_hz - dst_hz).abs() > 1.0 {
        if src_hz > dst_hz {
            let a = (-2.0 * std::f32::consts::PI * (0.45 * dst_hz) / src_hz).exp();
            for _ in 0..2 {
                let mut y = 0f32;
                for v in x.iter_mut() {
                    y = a * y + (1.0 - a) * *v;
                    *v = y;
                }
            }
        }
        let n_out = ((x.len() as f64) * (dst_hz as f64) / (src_hz as f64)).round() as usize;
        let mut out = Vec::with_capacity(n_out);
        let at = |i: isize| -> f32 { x[i.clamp(0, x.len() as isize - 1) as usize] };
        for i in 0..n_out {
            let pos = i as f64 * (src_hz as f64) / (dst_hz as f64);
            let k = pos.floor() as isize;
            let t = (pos - k as f64) as f32;
            let (p0, p1, p2, p3) = (at(k - 1), at(k), at(k + 1), at(k + 2));
            let v = p1 + 0.5 * t * (p2 - p0 + t * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3 + t * (3.0 * (p1 - p2) + p3 - p0)));
            out.push(v);
        }
        x = out;
    }
    // normalize around the start level
    let peak = x.iter().fold(0f32, |m, v| m.max(v.abs())).max(1e-6);
    let scale = 62.0 * opts.gain / peak;
    let level = opts.level as f32;
    x.iter().map(|v| (level + v * scale).clamp(0.0, 127.0)).collect()
}

fn step(level: i32, bit: u8) -> i32 {
    if bit == 1 {
        if level <= 125 { level + 2 } else { level }
    } else if level >= 2 {
        level - 2
    } else {
        level
    }
}

/// Viterbi over DAC levels of the start level's parity. Returns the bit
/// sequence whose final state is `start` (targets are extended with the
/// start level until that is reachable).
fn quantize_trellis(target: &[f32], start: u8) -> Vec<u8> {
    let start = start as i32;
    let parity = start & 1;
    let n_states = 64usize;
    let idx = |l: i32| -> usize { ((l - parity) / 2) as usize };
    let lvl = |i: usize| -> i32 { i as i32 * 2 + parity };
    let mut tgt: Vec<f32> = target.to_vec();
    // multiple of 8 bits, then room to come home
    while tgt.len() % 8 != 0 {
        tgt.push(start as f32);
    }
    loop {
        let n = tgt.len();
        let inf = f32::INFINITY;
        let mut cost = vec![inf; n_states];
        cost[idx(start)] = 0.0;
        let mut back: Vec<u8> = vec![0; n * n_states]; // bit taken to reach state at step t
        let mut prev_state: Vec<u8> = vec![0; n * n_states];
        for t in 0..n {
            let mut next = vec![inf; n_states];
            for si in 0..n_states {
                let c = cost[si];
                if c == inf {
                    continue;
                }
                let l = lvl(si);
                for bit in 0..2u8 {
                    let nl = step(l, bit);
                    let ni = idx(nl);
                    let e = nl as f32 - tgt[t];
                    let nc = c + e * e;
                    if nc < next[ni] {
                        next[ni] = nc;
                        back[t * n_states + ni] = bit;
                        prev_state[t * n_states + ni] = si as u8;
                    }
                }
            }
            cost = next;
        }
        if cost[idx(start)] == inf {
            for _ in 0..8 {
                tgt.push(start as f32);
            }
            continue;
        }
        let mut bits = vec![0u8; n];
        let mut si = idx(start);
        for t in (0..n).rev() {
            bits[t] = back[t * n_states + si];
            si = prev_state[t * n_states + si] as usize;
        }
        return bits;
    }
}

fn quantize_greedy(target: &[f32], start: u8) -> Vec<u8> {
    let mut l = start as i32;
    let mut bits = Vec::with_capacity(target.len() + 8);
    for &t in target {
        let bit = if t > l as f32 { 1 } else { 0 };
        l = step(l, bit);
        bits.push(bit);
    }
    while bits.len() % 8 != 0 {
        bits.push((bits.len() % 2) as u8);
    }
    bits
}

/// Pack bits LSB-first and pad to 16n+1 bytes with 0x55 (net zero).
fn finish(bits: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bits.len() / 8 + 17);
    for chunk in bits.chunks(8) {
        let mut b = 0u8;
        for (j, &bit) in chunk.iter().enumerate() {
            b |= bit << j;
        }
        out.push(b);
    }
    let want = ((out.len().max(1) - 1 + 15) / 16) * 16 + 1;
    while out.len() < want {
        out.push(0x55);
    }
    out
}

/// Decode a .dmc through the real DMC model to −1..1 floats.
pub fn decode_wave(data: &[u8], rate_idx: u8, level: u8) -> Vec<f32> {
    viper_apu::apu::decode_dmc(data, rate_idx, level).iter().map(|&l| (l as f32 - level as f32) / 64.0).collect()
}

/// Band-limited SNR in dB of `decoded` against `reference` (same rate),
/// both low-passed at `band_hz` so the comparison ignores the bit noise
/// above what the output stage passes anyway.
pub fn snr_db(reference: &[f32], decoded: &[f32], hz: f32, band_hz: f32) -> f32 {
    let n = reference.len().min(decoded.len());
    if n == 0 {
        return 0.0;
    }
    let a = (-2.0 * std::f32::consts::PI * band_hz / hz).exp();
    let lp = |x: &[f32]| -> Vec<f32> {
        let mut y = 0f32;
        x.iter().take(n).map(|v| { y = a * y + (1.0 - a) * v; y }).collect()
    };
    let r = lp(reference);
    let d = lp(decoded);
    let sig: f32 = r.iter().map(|v| v * v).sum();
    let err: f32 = r.iter().zip(d.iter()).map(|(x, y)| (x - y) * (x - y)).sum();
    10.0 * (sig / err.max(1e-12)).log10()
}

pub fn encode(wave: &[f32], src_hz: f32, opts: &EncodeOptions) -> anyhow::Result<Encoded> {
    let target = prepare(wave, src_hz, opts);
    if target.is_empty() {
        anyhow::bail!("nothing to encode (empty or all-silent input)");
    }
    // fade to the start level so the trellis can come home cheaply
    let mut tgt = target.clone();
    let last = *tgt.last().unwrap();
    for i in 0..32 {
        tgt.push(last + (opts.level as f32 - last) * (i as f32 + 1.0) / 32.0);
    }
    let bits = match opts.mode {
        Mode::Trellis => quantize_trellis(&tgt, opts.level),
        Mode::Greedy => quantize_greedy(&tgt, opts.level),
    };
    let data = finish(&bits);
    if data.len() > opts.max_bytes {
        anyhow::bail!(
            "encoded sample is {} bytes, over the {}-byte limit ({:.0} ms at rate {}); lower --rate, trim, or shorten the source",
            data.len(), opts.max_bytes, data.len() as f32 * 8.0 / rate_hz(opts.rate) * 1000.0, opts.rate
        );
    }
    let hz = rate_hz(opts.rate);
    let reference: Vec<f32> = tgt.iter().map(|t| (t - opts.level as f32) / 64.0).collect();
    let decoded = decode_wave(&data, opts.rate, opts.level);
    let greedy = finish(&quantize_greedy(&tgt, opts.level));
    let decoded_greedy = decode_wave(&greedy, opts.rate, opts.level);
    let levels = viper_apu::apu::decode_dmc(&data, opts.rate, opts.level);
    let clamps = levels.windows(2).filter(|w| w[0] == w[1] && (w[0] == 0 || w[0] >= 126)).count();
    let stats = EncodeStats {
        bytes: data.len(),
        seconds: data.len() as f32 * 8.0 / hz,
        min_level: levels.iter().copied().min().unwrap_or(opts.level),
        max_level: levels.iter().copied().max().unwrap_or(opts.level),
        clamps,
        snr_db: snr_db(&reference, &decoded, hz, 10_000.0),
        snr_greedy_db: snr_db(&reference, &decoded_greedy, hz, 10_000.0),
    };
    Ok(Encoded { data, stats })
}

/// The bank a song plays: the built-in one, or every `@dpcm` file decoded
/// through the DMC model at its own rate (for the tracker's preview).
pub fn load_bank(song: &crate::Song, base: Option<&std::path::Path>) -> anyhow::Result<Vec<Sample>> {
    if song.samples.is_empty() {
        return Ok(default_bank());
    }
    let mut out = Vec::new();
    for r in &song.samples {
        let p = match base {
            Some(d) if r.path.is_relative() => d.join(&r.path),
            _ => r.path.clone(),
        };
        let data = std::fs::read(&p).map_err(|e| anyhow::anyhow!("@dpcm {}: {}: {}", r.name, p.display(), e))?;
        out.push(Sample { name: r.name.clone(), wave: decode_wave(&data, r.rate, 64), rate_hz: rate_hz(r.rate) });
    }
    Ok(out)
}

/// Greedy 1-bit delta encoding: the 7-bit DAC follows the waveform ±2 per
/// bit, LSB first, padded to 16n+1 bytes with a silent toggle pattern.
pub fn encode_dmc(wave: &[f32]) -> Vec<u8> {
    let mut dac: i32 = 64;
    let mut bits: Vec<u8> = Vec::with_capacity(wave.len() + 64);
    for &x in wave {
        let target = 64.0 + x * 60.0;
        if target > dac as f32 && dac < 126 {
            dac += 2;
            bits.push(1);
        } else if dac > 1 {
            dac -= 2;
            bits.push(0);
        } else {
            bits.push(1);
        }
    }
    let nbytes = (bits.len() + 7) / 8;
    let nbytes = ((nbytes.max(1) - 1 + 15) / 16) * 16 + 1;
    while bits.len() < nbytes * 8 {
        bits.push((bits.len() % 2) as u8);
    }
    let mut out = Vec::with_capacity(nbytes);
    for chunk in bits.chunks(8) {
        let mut b = 0u8;
        for (j, &bit) in chunk.iter().enumerate() {
            b |= bit << j;
        }
        out.push(b);
    }
    out
}

/// Map a DPCM-column MIDI note to a sample index: C-4 = 0, C#4 = 1, ...
pub fn note_to_sample(note: u8) -> usize {
    (note as i32 - 60).max(0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bank_is_deterministic_and_padded() {
        let a = default_bank();
        let b = default_bank();
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.wave, y.wave);
            let enc = encode_dmc(&x.wave);
            assert_eq!(enc.len() % 16, 1, "{} not 16n+1", x.name);
            assert!(enc.len() < 0xFF1);
        }
    }

    #[test]
    fn trellis_beats_greedy_and_comes_home() {
        for name in ["kick", "snare"] {
            let wave = synth(name, RATE_HZ).unwrap();
            let opts = EncodeOptions::default();
            let e = encode(&wave, RATE_HZ, &opts).unwrap();
            assert!(e.stats.snr_db >= e.stats.snr_greedy_db - 0.01, "{}: trellis {} < greedy {}", name, e.stats.snr_db, e.stats.snr_greedy_db);
            // Kicks sweep through the delta-modulation slope limit (a full-swing
            // sine is slope-limited above ~170 Hz at rate 15), so the bar is
            // modest; the point is trellis >= greedy on the same material.
            // A noise burst (snare) has no waveform for delta modulation to
            // track, so only the tonal kick gets an absolute bar.
            if name == "kick" {
                assert!(e.stats.snr_db > 12.0, "{}: snr {}", name, e.stats.snr_db);
            }
            let levels = viper_apu::apu::decode_dmc(&e.data, 15, 64);
            assert_eq!(*levels.last().unwrap(), 64, "{}: must end at the start level", name);
            assert_eq!(e.data.len() % 16, 1);
            assert!(e.data.len() <= 4081);
            let again = encode(&wave, RATE_HZ, &opts).unwrap();
            assert_eq!(e.data, again.data, "deterministic");
        }
    }

    #[test]
    fn encoder_invariants_across_rates_and_edge_inputs() {
        for rate in [15u8, 12] {
            for name in SYNTH_NAMES {
                let wave = synth(name, 44_100.0).unwrap();
                let e = encode(&wave, 44_100.0, &EncodeOptions { rate, ..Default::default() }).unwrap();
                assert_eq!(e.data.len() % 16, 1, "{} @{}", name, rate);
                assert!(e.data.len() <= 4081);
                let levels = viper_apu::apu::decode_dmc(&e.data, rate, 64);
                assert_eq!(*levels.last().unwrap(), 64);
                assert_eq!(levels.len(), e.data.len() * 8);
            }
        }
        let one = encode(&[0.5], 44_100.0, &EncodeOptions { trim_db: None, ..Default::default() }).unwrap();
        assert_eq!(one.data.len() % 16, 1);
        assert!(encode(&[], 44_100.0, &EncodeOptions::default()).is_err());
    }

    #[test]
    fn sample_note_mapping() {
        assert_eq!(note_to_sample(60), 0);
        assert_eq!(note_to_sample(62), 2);
        assert_eq!(note_to_sample(40), 0);
    }
}
