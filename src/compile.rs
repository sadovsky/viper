//! Stage 18: lower a viper [`Song`] to the viper-nsf IR and emit an NSF.
//!
//! The mapping from the tracker grid to driver events:
//!
//! * A cell with a note gates the channel on for that step; the first empty
//!   step afterwards releases it — exactly what the internal synth does.
//! * Instrument and volume changes are emitted only when they differ from
//!   the stream's last value (tracked per pattern so patterns stay
//!   order-independent).
//! * Effect column letters: `Rxx` retrig every xx frames, `Dxx` duty,
//!   `Sxx` slide speed, `Vdr` vibrato depth/rate, `Axy` arpeggio, `E00`
//!   envelope reset.
//! * PU notes index the period table as MIDI − 24; TRI adds an octave;
//!   NOI maps pitch to the 16 noise periods; DPCM maps C-4.. to samples.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use viper_nsf::{Channel, DpcmSample, Driver, Envelope, Event, Expansion, Instrument as NsfInstrument, Module, Pattern, Song as NsfSong};

use crate::{dpcm, Instrument, Song, CHANNELS, INSTRUMENTS, STEPS_PER_PHRASE};

/// NES noise period index for a MIDI note: higher note = brighter.
/// C-2 (36) and below → 15 (rumble), C-7 (96) and above → 0 (hiss).
pub fn noise_period_index(note: u8) -> u8 {
    (15 - ((note as i32 - 36) / 4).clamp(0, 15)) as u8
}

/// Quantize viper's continuous duty to the 2A03's four settings.
pub fn nes_duty(duty: f32) -> u8 {
    if duty < 0.19 { 0 } else if duty < 0.375 { 1 } else if duty < 0.625 { 2 } else { 3 }
}

fn lower_instrument(inst: &Instrument) -> NsfInstrument {
    NsfInstrument {
        duty: Some(nes_duty(inst.duty)),
        envelope: Some(Envelope::from_adsr(inst.attack_ms, inst.decay_ms, inst.sustain, inst.release_ms, inst.volume)),
    }
}

pub struct Lowered {
    pub module: Module,
    pub warnings: Vec<String>,
}

/// Lower a song. `base_dir` resolves `@dpcm` sample paths.
pub fn lower(song: &Song, base_dir: Option<&Path>) -> Result<Lowered> {
    let mut warnings: Vec<String> = Vec::new();
    let mut unknown_fx: BTreeSet<char> = BTreeSet::new();
    let mut low_notes = 0usize;

    // samples
    let mut samples: Vec<DpcmSample> = Vec::new();
    if song.samples.is_empty() {
        for s in dpcm::default_bank() {
            samples.push(DpcmSample { name: s.name.to_string(), data: dpcm::encode_dmc(&s.wave), rate: dpcm::RATE_INDEX, loop_: false });
        }
    } else {
        for (name, path) in &song.samples {
            let p = match base_dir {
                Some(d) if path.is_relative() => d.join(path),
                _ => path.clone(),
            };
            let data = std::fs::read(&p).with_context(|| format!("@dpcm {}: read {}", name, p.display()))?;
            samples.push(DpcmSample { name: name.clone(), data, rate: dpcm::RATE_INDEX, loop_: false });
        }
    }

    let instruments: Vec<NsfInstrument> = song.instruments.iter().map(lower_instrument).collect();

    let mut patterns = Vec::with_capacity(song.phrases.len());
    for (pi, phrase) in song.phrases.iter().enumerate() {
        let mut pat = Pattern::new(STEPS_PER_PHRASE);
        for ch in 0..CHANNELS {
            let mut cur_instr: Option<u8> = None;
            let mut cur_vol: Option<u8> = None;
            let mut sounding = false;
            for step in 0..STEPS_PER_PHRASE {
                let cell = phrase.cells[step][ch];
                let ev = &mut pat.rows[step][ch];
                match cell.note {
                    Some(n) => {
                        let instr = cell.instr.min((INSTRUMENTS - 1) as u8);
                        if cur_instr != Some(instr) && ch != 4 {
                            ev.push(Event::Instr(instr));
                            cur_instr = Some(instr);
                        }
                        let vol = if cell.vol == 0 { 15 } else { cell.vol.min(15) };
                        if cur_vol != Some(vol) && ch != 4 {
                            ev.push(Event::Vol(vol));
                            cur_vol = Some(vol);
                        }
                        if let Some((cmd, p)) = cell.fx {
                            match cmd.to_ascii_uppercase() {
                                b'R' => ev.push(Event::Retrig(p)),
                                b'D' => ev.push(Event::Duty(p & 3)),
                                b'S' => ev.push(Event::Slide(p)),
                                b'V' => ev.push(Event::Vibrato { depth: p >> 4, rate: p & 15 }),
                                b'A' => ev.push(Event::Arp { x: p >> 4, y: p & 15 }),
                                b'E' => ev.push(Event::EnvReset),
                                c => { unknown_fx.insert(c as char); }
                            }
                        }
                        let idx: u8 = match ch {
                            0 | 1 => {
                                if n < 33 { low_notes += 1; }
                                (n as i32 - 24).clamp(0, 95) as u8
                            }
                            2 => (n as i32 - 24 + 12).clamp(0, 95) as u8,
                            3 => noise_period_index(n),
                            _ => {
                                let id = dpcm::note_to_sample(n);
                                if id >= samples.len() {
                                    warnings.push(format!("phrase {:02X} step {:02X}: DPCM note {} maps to sample {} but only {} exist", pi, step, n, id, samples.len()));
                                }
                                id.min(samples.len().saturating_sub(1)) as u8
                            }
                        };
                        ev.push(Event::Note(idx));
                        sounding = ch != 4;
                    }
                    None => {
                        if sounding || (step == 0 && ch != 4) {
                            ev.push(Event::Off);
                            sounding = false;
                        }
                    }
                }
            }
        }
        patterns.push(pat);
    }

    let order: Vec<usize> = if song.order.is_empty() {
        warnings.push("no order list; playing all phrases in index order".into());
        (0..song.phrases.len()).collect()
    } else {
        song.order.clone()
    };
    if low_notes > 0 {
        warnings.push(format!("{} pulse notes below A-1 clamp to the lowest 2A03 period", low_notes));
    }
    for c in unknown_fx {
        warnings.push(format!("effect `{}` has no NSF meaning; ignored", c));
    }

    let title = if song.title.is_empty() { "viper".to_string() } else { song.title.clone() };
    let nsf_song = NsfSong {
        title,
        frames_per_row: NsfSong::frames_per_row_for_bpm(song.bpm as f64),
        rows_per_pattern: STEPS_PER_PHRASE as u8,
        patterns,
        order,
        loop_pos: song.loop_pos.min(song.order.len().saturating_sub(1)),
        instruments,
        samples,
    };
    let _ = Channel::Pu1; // keep the channel enum referenced for expansion work
    Ok(Lowered {
        module: Module {
            songs: vec![nsf_song],
            artist: song.artist.clone(),
            copyright: song.copyright.clone(),
            expansion: if song.expansion { Expansion::Vrc6 } else { Expansion::None },
        },
        warnings,
    })
}

pub struct Compiled {
    pub nsf: Vec<u8>,
    pub warnings: Vec<String>,
    pub data_bytes: usize,
    pub sample_bytes: usize,
    pub total_frames: u32,
    pub frames_per_row: f64,
}

pub fn compile(song: &Song, driver: &Driver, base_dir: Option<&Path>) -> Result<Compiled> {
    let lowered = lower(song, base_dir)?;
    let out = viper_nsf::emit(&lowered.module, driver)?;
    let s = &lowered.module.songs[0];
    let mut warnings = lowered.warnings;
    warnings.extend(out.warnings);
    Ok(Compiled {
        nsf: out.nsf,
        warnings,
        data_bytes: out.data_bytes,
        sample_bytes: out.sample_bytes,
        total_frames: s.total_frames(),
        frames_per_row: s.frames_per_row,
    })
}

/// Resolve the song's `@driver` paths (relative to the `.vip`) and load it.
pub fn load_song_driver(song: &Song, base_dir: Option<&Path>) -> Result<Driver> {
    let (bin, sym) = song.driver.as_ref().context("song has no @driver directive")?;
    let resolve = |p: &Path| match base_dir {
        Some(d) if p.is_relative() => d.join(p),
        _ => p.to_path_buf(),
    };
    Driver::load(&resolve(bin), &resolve(sym))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_mapping_is_monotonic() {
        assert_eq!(noise_period_index(36), 15);
        assert_eq!(noise_period_index(60), 9);
        assert_eq!(noise_period_index(96), 0);
        assert_eq!(noise_period_index(120), 0);
    }

    #[test]
    fn lowering_emits_instr_vol_note_and_off() {
        let mut song = Song::default();
        song.phrases[0].cells[0][0] = crate::Cell { note: Some(64), instr: 1, vol: 0, fx: None };
        song.phrases[0].cells[2][0] = crate::Cell { note: Some(66), instr: 1, vol: 8, fx: Some((b'V', 0x63)) };
        let l = lower(&song, None).unwrap();
        let rows = &l.module.songs[0].patterns[0].rows;
        assert_eq!(rows[0][0], vec![Event::Instr(1), Event::Vol(15), Event::Note(40)]);
        assert_eq!(rows[1][0], vec![Event::Off]);
        assert_eq!(rows[2][0], vec![Event::Vol(8), Event::Vibrato { depth: 6, rate: 3 }, Event::Note(42)]);
        assert_eq!(rows[3][0], vec![Event::Off]);
        // other channels: an Off at row 0 only
        assert_eq!(rows[0][1], vec![Event::Off]);
        assert!(rows[1][1].is_empty());
        // DPCM never gets Off
        assert!(rows[0][4].is_empty());
        assert_eq!(l.module.songs[0].samples.len(), 3);
    }
}
