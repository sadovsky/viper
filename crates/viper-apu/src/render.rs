//! Offline rendering. Deterministic: the same NSF and options produce
//! bit-identical output.

use crate::apu::{CH_ALL, CH_DMC, CH_NOI, CH_PU1, CH_PU2, CH_TRI};
use crate::host::{Player, Trigger, TriggerKind};
use crate::nsf::Nsf;
use anyhow::Result;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct RenderOptions {
    pub song: u8,
    pub sample_rate: u32,
    /// How many times through the detected loop. The song length is found
    /// by hashing RAM at frame boundaries; if no loop is found within
    /// `max_seconds`, the render stops there.
    pub loops: u32,
    pub max_seconds: f64,
    /// Seconds of tail after the last loop.
    pub tail_seconds: f64,
    /// Render per-channel stems (and per-sample DPCM stems).
    pub stems: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { song: 0, sample_rate: 44_100, loops: 1, max_seconds: 300.0, tail_seconds: 1.0, stems: false }
    }
}

#[derive(Clone, Debug)]
pub struct Stem {
    pub name: String,
    pub samples: Vec<f32>,
}

#[derive(Debug)]
pub struct RenderResult {
    pub mix: Vec<f32>,
    pub stems: Vec<Stem>,
    pub log: Vec<crate::host::RegWrite>,
    pub triggers: Vec<Trigger>,
    /// Frames in one loop (None if no loop was detected).
    pub loop_frames: Option<u32>,
    pub total_frames: u32,
    /// DPCM `$4012` values in order of first use → stem index.
    pub dpcm_samples: Vec<u8>,
    pub sample_rate: u32,
}

/// Find where the song loops, without rendering it. Wraps the same
/// RAM-hashing pass the renderer uses, for callers that want the length
/// but not the audio — `viper rip`, notably.
pub fn find_loop(nsf: &Nsf, song: u8, max_seconds: f64) -> Result<Option<(u32, u32)>> {
    let opts = RenderOptions { song, max_seconds, ..RenderOptions::default() };
    Ok(analyze(nsf, &opts)?.0)
}

/// First pass: find the song length by RAM-state hashing and gather the
/// DPCM samples used. Returns (loop_start_frame, loop_len_frames) when
/// the driver state repeats, plus the frame count to render.
fn analyze(nsf: &Nsf, opts: &RenderOptions) -> Result<(Option<(u32, u32)>, Vec<u8>)> {
    let mut p = Player::new(nsf.clone(), opts.sample_rate);
    p.keep_log = false;
    p.init(opts.song)?;
    let max_frames = (opts.max_seconds * 60.0988) as u32;
    let mut seen: HashMap<u64, u32> = HashMap::new();
    let mut dpcm: Vec<u8> = Vec::new();
    let mut found = None;
    for f in 0..max_frames {
        let h = p.ram_hash();
        if let Some(&first) = seen.get(&h) {
            found = Some((first, f - first));
            break;
        }
        seen.insert(h, f);
        p.frame()?;
        p.samples.clear();
        for t in p.triggers.drain(..) {
            if let TriggerKind::Dpcm { addr_reg } = t.kind {
                if !dpcm.contains(&addr_reg) {
                    dpcm.push(addr_reg);
                }
            }
        }
    }
    Ok((found, dpcm))
}

fn run(nsf: &Nsf, opts: &RenderOptions, frames: u32, mask: u8, dmc_filter: Option<u8>, keep_log: bool) -> Result<Player> {
    let mut p = Player::new(nsf.clone(), opts.sample_rate);
    p.keep_log = keep_log;
    p.set_mask(mask);
    p.apu.dmc_filter = dmc_filter;
    p.init(opts.song)?;
    for _ in 0..frames {
        p.frame()?;
    }
    Ok(p)
}

/// The per-channel stems this NSF has. The VRC6 three appear only when the
/// header declares the chip, so a plain 2A03 file still renders exactly the
/// five it always did. Shared because `render` and `render_frames` had the
/// same list written out twice.
fn stem_specs(nsf: &Nsf) -> Vec<(&'static str, u8)> {
    let mut v = vec![("pu1", CH_PU1), ("pu2", CH_PU2), ("tri", CH_TRI), ("noi", CH_NOI)];
    if nsf.expansion & 0x01 != 0 {
        v.extend_from_slice(&[
            ("vp1", crate::vrc6::CH_VP1),
            ("vp2", crate::vrc6::CH_VP2),
            ("saw", crate::vrc6::CH_SAW),
        ]);
    }
    v
}

pub fn render(nsf: &Nsf, opts: &RenderOptions) -> Result<RenderResult> {
    let (loop_info, dpcm_samples) = analyze(nsf, opts)?;
    let fps = 60.0988;
    let tail = (opts.tail_seconds * fps) as u32;
    let (loop_frames, total_frames) = match loop_info {
        Some((start, len)) => (Some(len), start + len * opts.loops.max(1) + tail),
        None => (None, (opts.max_seconds * fps) as u32),
    };

    let full = run(nsf, opts, total_frames, CH_ALL, None, true)?;
    let mut result = RenderResult {
        mix: full.samples,
        stems: Vec::new(),
        log: full.log,
        triggers: full.triggers,
        loop_frames,
        total_frames,
        dpcm_samples: dpcm_samples.clone(),
        sample_rate: opts.sample_rate,
    };
    if opts.stems {
        for (name, mask) in stem_specs(nsf) {
            let p = run(nsf, opts, total_frames, mask, None, false)?;
            result.stems.push(Stem { name: name.to_string(), samples: p.samples });
        }
        if dpcm_samples.is_empty() {
            let p = run(nsf, opts, total_frames, CH_DMC, None, false)?;
            result.stems.push(Stem { name: "dpcm".to_string(), samples: p.samples });
        } else {
            for (i, &addr) in dpcm_samples.iter().enumerate() {
                let p = run(nsf, opts, total_frames, CH_DMC, Some(addr), false)?;
                result.stems.push(Stem { name: format!("dpcm{}", i), samples: p.samples });
            }
        }
    }
    Ok(result)
}

/// Render exactly `frames` frames (no loop detection), with stems if
/// requested. Used when the caller already knows the song length.
pub fn render_frames(nsf: &Nsf, opts: &RenderOptions, frames: u32) -> Result<RenderResult> {
    let (_, dpcm_samples) = analyze(nsf, &RenderOptions { max_seconds: frames as f64 / 60.0988 + 0.1, ..opts.clone() })?;
    let full = run(nsf, opts, frames, CH_ALL, None, true)?;
    let mut result = RenderResult {
        mix: full.samples,
        stems: Vec::new(),
        log: full.log,
        triggers: full.triggers,
        loop_frames: None,
        total_frames: frames,
        dpcm_samples: dpcm_samples.clone(),
        sample_rate: opts.sample_rate,
    };
    if opts.stems {
        for (name, mask) in stem_specs(nsf) {
            let p = run(nsf, opts, frames, mask, None, false)?;
            result.stems.push(Stem { name: name.to_string(), samples: p.samples });
        }
        if dpcm_samples.is_empty() {
            let p = run(nsf, opts, frames, CH_DMC, None, false)?;
            result.stems.push(Stem { name: "dpcm".to_string(), samples: p.samples });
        } else {
            for (i, &addr) in dpcm_samples.iter().enumerate() {
                let p = run(nsf, opts, frames, CH_DMC, Some(addr), false)?;
                result.stems.push(Stem { name: format!("dpcm{}", i), samples: p.samples });
            }
        }
    }
    Ok(result)
}

/// Format the register log as text: one `frame addr value` per line, hex.
pub fn format_log(log: &[crate::host::RegWrite]) -> String {
    let mut s = String::with_capacity(log.len() * 14);
    for w in log {
        s.push_str(&format!("{} {:04X} {:02X}\n", w.frame, w.addr, w.value));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pulse on channel 1 and nothing else, hand-assembled — the same
    /// trick `host.rs` uses, because viper cannot assemble 6502.
    fn pulse_nsf(expansion: u8) -> Nsf {
        let mut code: Vec<u8> = vec![
            0xA9, 0x01, 0x8D, 0x15, 0x40, // LDA #$01 / STA $4015 — pulse 1 on
            0xA9, 0xBF, 0x8D, 0x00, 0x40, // duty 2, halt, constant volume 15
            0xA9, 0xFD, 0x8D, 0x02, 0x40, // period low
            0xA9, 0x08, 0x8D, 0x03, 0x40, // length + period high
            0x60,
        ];
        let play = 0x8000 + code.len() as u16;
        code.push(0x60);
        let header = Nsf::build_header(0x8000, 0x8000, play, 1, "p", "", "", expansion);
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(&code);
        Nsf::parse(&bytes).unwrap()
    }

    fn opts(stems: bool) -> RenderOptions {
        RenderOptions { max_seconds: 0.5, stems, ..RenderOptions::default() }
    }

    #[test]
    fn the_same_nsf_renders_bit_identically_every_time() {
        // The first line of this module promises it. Nothing checked it, and
        // the pipeline test that looks similar compares two register logs
        // rather than two renders — so a nondeterministic mixer, resampler
        // or loop detector would have slipped straight through.
        let nsf = pulse_nsf(0);
        let a = render_frames(&nsf, &opts(false), 60).unwrap();
        let b = render_frames(&nsf, &opts(false), 60).unwrap();
        assert_eq!(a.mix, b.mix, "audio");
        assert_eq!(a.log, b.log, "register writes");
        assert_eq!(a.total_frames, b.total_frames);
    }

    #[test]
    fn a_stem_holds_one_channel_and_the_silent_ones_stay_silent() {
        let r = render_frames(&pulse_nsf(0), &opts(true), 60).unwrap();
        let names: Vec<&str> = r.stems.iter().map(|s| s.name.as_str()).collect();
        assert!(names.starts_with(&["pu1", "pu2", "tri", "noi"]), "{:?}", names);
        let peak = |n: &str| {
            r.stems.iter().find(|s| s.name == n).unwrap().samples.iter().fold(0f32, |a, b| a.max(b.abs()))
        };
        assert!(peak("pu1") > 0.01, "the channel that is playing");
        for quiet in ["pu2", "noi"] {
            assert!(peak(quiet) < 1e-6, "{} should be silent, peaked at {}", quiet, peak(quiet));
        }
        // Every stem is the same length as the mix, or they cannot be lined
        // up in a DAW.
        assert!(r.stems.iter().all(|s| s.samples.len() == r.mix.len()));
    }

    #[test]
    fn the_expansion_byte_decides_how_many_stems_there_are() {
        // A plain 2A03 file must render exactly the five it always did; the
        // VRC6 three appear only when the header declares the chip, so
        // adding expansion support cannot quietly change what an old song
        // splits into.
        assert_eq!(stem_specs(&pulse_nsf(0)).len(), 4, "plus DPCM, added separately");
        let vrc6 = stem_specs(&pulse_nsf(0x01));
        assert_eq!(vrc6.len(), 7);
        assert!(vrc6.iter().any(|(n, _)| *n == "saw"));
    }

    #[test]
    fn the_mix_lasts_as_long_as_the_frames_it_reports() {
        // Not exact: frames run at 60.0988 a second and the resampler
        // carries a fractional accumulator, so this catches a render that is
        // half or double the length it claims rather than one sample out.
        let r = render(&pulse_nsf(0), &opts(false)).unwrap();
        assert!(r.total_frames > 0);
        let want = r.total_frames as f64 * 44_100.0 / 60.0988;
        let ratio = r.mix.len() as f64 / want;
        assert!((0.99..=1.01).contains(&ratio), "{} samples for {} frames", r.mix.len(), r.total_frames);
    }
}
