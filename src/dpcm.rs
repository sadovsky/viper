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
    pub name: &'static str,
    pub wave: Vec<f32>,
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
        Sample { name: "kick", wave: kick(RATE_HZ) },
        Sample { name: "snare", wave: snare(RATE_HZ) },
        Sample { name: "hat", wave: hat(RATE_HZ) },
    ]
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
    fn sample_note_mapping() {
        assert_eq!(note_to_sample(60), 0);
        assert_eq!(note_to_sample(62), 2);
        assert_eq!(note_to_sample(40), 0);
    }
}
