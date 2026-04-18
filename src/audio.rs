//! Stage 2-3: cpal audio thread with a tiny chip synth.
//! One voice per channel: PU1/PU2 = pulse, TRI = triangle, NOI = xorshift noise.
//! Each voice runs an ADSR envelope sourced from the cell's instrument.

use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

use crate::{Instrument, Phrase, CHANNELS, INSTRUMENTS, STEPS_PER_PHRASE};

pub struct Transport {
    pub playing: bool,
    pub bpm: u16,
    pub step: usize,
    pub phrase: Phrase,
    pub instruments: [Instrument; INSTRUMENTS],
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            playing: false,
            bpm: 140,
            step: 0,
            phrase: Phrase::default(),
            instruments: [Instrument::default(); INSTRUMENTS],
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum EnvPhase {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy)]
struct Voice {
    kind: u8, // 0=PU1, 1=PU2, 2=TRI, 3=NOI
    freq: f32,
    phase: f32,
    level: f32,
    env: EnvPhase,
    instrument: Instrument,
    noise_state: u32,
}

impl Voice {
    fn new(kind: u8) -> Self {
        Self {
            kind,
            freq: 0.0,
            phase: 0.0,
            level: 0.0,
            env: EnvPhase::Idle,
            instrument: Instrument::default(),
            noise_state: 0xACE1u32.wrapping_add((kind as u32).wrapping_mul(0x9E3779B9)),
        }
    }

    fn gate_on(&mut self, freq: f32, instr: Instrument) {
        self.freq = freq;
        self.instrument = instr;
        self.env = EnvPhase::Attack;
        // Don't hard-reset level: retriggers ramp smoothly from the current level.
    }

    fn gate_off(&mut self) {
        if !matches!(self.env, EnvPhase::Idle) {
            self.env = EnvPhase::Release;
        }
    }

    fn kill(&mut self) {
        self.env = EnvPhase::Idle;
        self.level = 0.0;
    }

    /// Advance the ADSR envelope by one sample.
    fn advance_env(&mut self, sr: f32) {
        let inst = self.instrument;
        let per_ms = sr * 0.001;
        // Linear slope per sample for each segment, with a 1-sample floor so
        // atk=0 / dec=0 / rel=0 mean "instant" without divide-by-zero.
        let atk_rate = 1.0 / (inst.attack_ms as f32 * per_ms).max(1.0);
        let dec_span = (1.0 - inst.sustain).max(0.0);
        let dec_rate = dec_span / (inst.decay_ms as f32 * per_ms).max(1.0);
        let rel_rate = 1.0 / (inst.release_ms as f32 * per_ms).max(1.0);

        match self.env {
            EnvPhase::Idle => {}
            EnvPhase::Attack => {
                self.level += atk_rate;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.env = EnvPhase::Decay;
                }
            }
            EnvPhase::Decay => {
                self.level -= dec_rate;
                if self.level <= inst.sustain {
                    self.level = inst.sustain;
                    self.env = EnvPhase::Sustain;
                }
            }
            EnvPhase::Sustain => {
                self.level = inst.sustain;
            }
            EnvPhase::Release => {
                self.level -= rel_rate;
                if self.level <= 0.0 {
                    self.level = 0.0;
                    self.env = EnvPhase::Idle;
                }
            }
        }
    }

    fn tick(&mut self, sr: f32) -> f32 {
        self.advance_env(sr);
        if matches!(self.env, EnvPhase::Idle) {
            return 0.0;
        }
        let inc = self.freq / sr;
        let raw = match self.kind {
            0 | 1 => {
                let duty = self.instrument.duty.clamp(0.05, 0.95);
                let s = if self.phase < duty { 1.0 } else { -1.0 };
                self.phase = (self.phase + inc).fract();
                s
            }
            2 => {
                let s = 1.0 - 4.0 * (self.phase - 0.5).abs();
                self.phase = (self.phase + inc).fract();
                s
            }
            3 => {
                let mut x = self.noise_state.max(1);
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                self.noise_state = x;
                ((x & 0xFFFF) as f32 / 32768.0) - 1.0
            }
            _ => 0.0,
        };
        raw * self.level * self.instrument.volume
    }
}

fn midi_to_hz(note: u8) -> f32 {
    440.0 * 2.0f32.powf((note as f32 - 69.0) / 12.0)
}

pub struct AudioEngine {
    pub transport: Arc<Mutex<Transport>>,
    _stream: cpal::Stream,
}

impl AudioEngine {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("no default output device")?;
        let supported = device.default_output_config()?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        let transport = Arc::new(Mutex::new(Transport::default()));

        let stream = match sample_format {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, transport.clone())?,
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, transport.clone())?,
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, transport.clone())?,
            sf => bail!("unsupported sample format: {:?}", sf),
        };
        stream.play()?;

        Ok(Self { transport, _stream: stream })
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    transport: Arc<Mutex<Transport>>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let sample_rate = config.sample_rate.0 as f32;
    let out_channels = config.channels as usize;

    let mut voices: [Voice; CHANNELS] = [
        Voice::new(0),
        Voice::new(1),
        Voice::new(2),
        Voice::new(3),
    ];
    let mut sample_in_step: u32 = 0;
    let mut last_step: usize = usize::MAX;

    let err_fn = |e| eprintln!("audio stream error: {}", e);
    let stream = device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
            let mut tr = match transport.lock() {
                Ok(g) => g,
                Err(_) => {
                    for s in data.iter_mut() {
                        *s = T::from_sample(0.0);
                    }
                    return;
                }
            };
            let spb = (sample_rate * 60.0 / tr.bpm.max(1) as f32 / 4.0).max(1.0) as u32;
            for frame in data.chunks_mut(out_channels) {
                if !tr.playing {
                    last_step = usize::MAX;
                    sample_in_step = 0;
                    tr.step = 0;
                    for v in &mut voices {
                        v.kill();
                    }
                    for s in frame.iter_mut() {
                        *s = T::from_sample(0.0);
                    }
                    continue;
                }
                if tr.step != last_step {
                    last_step = tr.step;
                    for (ch, v) in voices.iter_mut().enumerate() {
                        let cell = tr.phrase.cells[tr.step][ch];
                        if let Some(n) = cell.note {
                            let idx = (cell.instr as usize).min(INSTRUMENTS - 1);
                            v.gate_on(midi_to_hz(n), tr.instruments[idx]);
                        } else {
                            v.gate_off();
                        }
                    }
                }
                let mut mix = 0.0f32;
                for v in &mut voices {
                    mix += v.tick(sample_rate);
                }
                let s = (mix * 0.2).clamp(-1.0, 1.0);
                let out = T::from_sample(s);
                for o in frame.iter_mut() {
                    *o = out;
                }
                sample_in_step += 1;
                if sample_in_step >= spb {
                    sample_in_step = 0;
                    tr.step = (tr.step + 1) % STEPS_PER_PHRASE;
                }
            }
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}
