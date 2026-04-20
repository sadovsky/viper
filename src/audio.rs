//! Stage 2-3: cpal audio thread with a tiny chip synth.
//! One voice per channel: PU1/PU2 = pulse, TRI = triangle, NOI = xorshift noise.
//! Each voice runs an ADSR envelope sourced from the cell's instrument.
//! Stage 5: live gate events — UI can push realtime gate_on/off while stopped.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

use crate::{Instrument, Phrase, CHANNELS, INSTRUMENTS, STEPS_PER_PHRASE};

/// Out-of-band gate event pushed by the UI thread (Stage 5 live monitor).
#[derive(Clone, Copy, Debug)]
pub enum LiveEvent {
    /// Gate a voice on. `hold_ms = Some(t)` auto-releases after t ms so the
    /// instrument's ADSR release segment fires — terminals don't emit KeyUp,
    /// so without this the voice would sustain forever.
    GateOn { ch: u8, note: u8, instr: u8, vel: f32, hold_ms: Option<u32> },
    GateOff { ch: u8 },
    AllOff,
}

pub struct Transport {
    pub playing: bool,
    pub bpm: u16,
    pub step: usize,
    pub phrase: Phrase,
    pub instruments: [Instrument; INSTRUMENTS],
    /// Queue of live-monitor events applied on the next audio callback.
    pub live_events: VecDeque<LiveEvent>,
    /// Per-channel mute. A muted channel's voice is killed instantly on
    /// mute and suppressed for pattern-driven and live-driven gate-ons.
    pub muted: [bool; CHANNELS],
    /// Stage 9: latest per-voice state snapshot, overwritten by the audio
    /// thread at the end of every callback. UI reads this on each tick to
    /// drive the visualizer. Single slot — newest sample wins; we don't
    /// accumulate history because the UI runs at ~60Hz and never catches up.
    pub frame: VizFrame,
}

/// One voice's state at the end of an audio callback. `env_level` is the
/// ADSR amplitude (0..1); `gate` is true while the voice is not Idle.
/// `freq` is the oscillator frequency in Hz (0 when idle), `vel` is the
/// per-note velocity captured at gate-on.
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)] // fields are the bus contract; Stage 10+ consumes them
pub struct VoiceFrame {
    pub gate: bool,
    pub env_level: f32,
    pub freq: f32,
    pub vel: f32,
}

/// Full snapshot the audio thread publishes for the UI to render.
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)] // fields are the bus contract; Stage 10+ consumes them
pub struct VizFrame {
    pub playing: bool,
    pub step: usize,
    /// 0..1 position within the current 16th-note step, from the audio
    /// thread's sample counter. Lets the UI interpolate sub-step motion.
    pub step_phase: f32,
    pub voices: [VoiceFrame; CHANNELS],
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            playing: false,
            bpm: 140,
            step: 0,
            phrase: Phrase::default(),
            instruments: [Instrument::default(); INSTRUMENTS],
            live_events: VecDeque::new(),
            muted: [false; CHANNELS],
            frame: VizFrame::default(),
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
    /// Per-note velocity in 0..=1, captured at gate-on from the cell's volume column.
    vel: f32,
    env: EnvPhase,
    instrument: Instrument,
    noise_state: u32,
    /// Samples remaining before an auto-release fires. Set by live GateOn
    /// with `hold_ms`; zero means "no auto-release pending".
    auto_release: u32,
}

impl Voice {
    fn new(kind: u8) -> Self {
        Self {
            kind,
            freq: 0.0,
            phase: 0.0,
            level: 0.0,
            vel: 1.0,
            env: EnvPhase::Idle,
            instrument: Instrument::default(),
            noise_state: 0xACE1u32.wrapping_add((kind as u32).wrapping_mul(0x9E3779B9)),
            auto_release: 0,
        }
    }

    fn gate_on(&mut self, freq: f32, instr: Instrument, vel: f32) {
        self.freq = freq;
        self.instrument = instr;
        self.vel = vel.clamp(0.0, 1.0);
        self.env = EnvPhase::Attack;
        // Pattern-driven gate cancels any live auto-release: the step grid
        // is authoritative while playing.
        self.auto_release = 0;
        // Don't hard-reset level: retriggers ramp smoothly from the current level.
    }

    fn gate_off(&mut self) {
        if !matches!(self.env, EnvPhase::Idle) {
            self.env = EnvPhase::Release;
        }
        self.auto_release = 0;
    }

    fn kill(&mut self) {
        self.env = EnvPhase::Idle;
        self.level = 0.0;
        self.auto_release = 0;
    }

    /// Advance the ADSR envelope by one sample.
    fn advance_env(&mut self, sr: f32) {
        // Live auto-release countdown: once the hold timer expires, drop into
        // the instrument's Release segment so the note fades naturally.
        if self.auto_release > 0 {
            self.auto_release -= 1;
            if self.auto_release == 0 && !matches!(self.env, EnvPhase::Idle | EnvPhase::Release) {
                self.env = EnvPhase::Release;
            }
        }
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
        raw * self.level * self.instrument.volume * self.vel
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

/// Stage 15a: render `phrase` to 16-bit mono PCM WAV at `path`, offline.
/// Mirrors the realtime step scheduler (same `spb`, same voice model), then
/// keeps rendering after the last step until every voice is Idle or we hit
/// a 2-second tail cap — ensures release tails finish cleanly. Returns the
/// number of audio frames written.
pub fn bounce_to_wav(
    path: &Path,
    phrase: &Phrase,
    instruments: &[Instrument; INSTRUMENTS],
    bpm: u16,
    loops: u32,
    sample_rate: u32,
) -> Result<u32> {
    if loops == 0 {
        bail!("bounce: loops must be ≥ 1");
    }
    let sr_f = sample_rate as f32;
    let spb = (sr_f * 60.0 / bpm.max(1) as f32 / 4.0).max(1.0) as u32;
    let total_steps = (loops as usize).saturating_mul(STEPS_PER_PHRASE);
    let tail_cap = sample_rate * 2;

    let mut voices: [Voice; CHANNELS] = [
        Voice::new(0),
        Voice::new(1),
        Voice::new(2),
        Voice::new(3),
    ];
    let mut samples: Vec<f32> = Vec::with_capacity(
        (total_steps as u64 * spb as u64).min(u32::MAX as u64) as usize
    );

    let render_sample = |voices: &mut [Voice; CHANNELS], samples: &mut Vec<f32>| {
        let mut mix = 0.0f32;
        for v in voices.iter_mut() {
            mix += v.tick(sr_f);
        }
        samples.push((mix * 0.2).clamp(-1.0, 1.0));
    };

    for global_step in 0..total_steps {
        let step = global_step % STEPS_PER_PHRASE;
        for (ch, v) in voices.iter_mut().enumerate() {
            let cell = phrase.cells[step][ch];
            if let Some(n) = cell.note {
                let idx = (cell.instr as usize).min(INSTRUMENTS - 1);
                let vel = if cell.vol == 0 { 1.0 } else { (cell.vol as f32 / 15.0).min(1.0) };
                v.gate_on(midi_to_hz(n), instruments[idx], vel);
            } else {
                v.gate_off();
            }
        }
        for _ in 0..spb {
            render_sample(&mut voices, &mut samples);
        }
    }
    // Release-tail rendering: gate every voice off, then keep ticking until
    // they all settle or we hit the cap (prevents runaway release times).
    for v in voices.iter_mut() {
        v.gate_off();
    }
    let mut tail = 0u32;
    while tail < tail_cap
        && voices.iter().any(|v| !matches!(v.env, EnvPhase::Idle))
    {
        render_sample(&mut voices, &mut samples);
        tail += 1;
    }

    write_wav_pcm16_mono(path, sample_rate, &samples)?;
    Ok(samples.len() as u32)
}

/// Minimal 16-bit PCM mono WAV writer — RIFF header + fmt chunk + data chunk.
/// No compression, no extra fmt bytes. Clamps input to [-1, 1] before scaling
/// to i16.
fn write_wav_pcm16_mono(path: &Path, sample_rate: u32, samples: &[f32]) -> Result<()> {
    let f = File::create(path)
        .with_context(|| format!("bounce: create {}", path.display()))?;
    let mut w = BufWriter::new(f);
    let num_channels: u16 = 1;
    let bits: u16 = 16;
    let block_align = num_channels * bits / 8;
    let byte_rate = sample_rate * block_align as u32;
    let data_size = (samples.len() as u32).saturating_mul(block_align as u32);
    let riff_size = 36 + data_size;

    w.write_all(b"RIFF")?;
    w.write_all(&riff_size.to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?;            // PCM
    w.write_all(&num_channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&bits.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_size.to_le_bytes())?;
    for s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let v = (clamped * i16::MAX as f32).round() as i16;
        w.write_all(&v.to_le_bytes())?;
    }
    w.flush()?;
    Ok(())
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
    let mut was_playing = false;

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
            // Kill any voice that just got muted this frame. Checked before
            // draining events so a mute-then-gate pair ends silent.
            for (ch, v) in voices.iter_mut().enumerate() {
                if tr.muted[ch] && !matches!(v.env, EnvPhase::Idle) {
                    v.kill();
                }
            }
            // Drain any pending live-monitor events before rendering this buffer.
            while let Some(ev) = tr.live_events.pop_front() {
                match ev {
                    LiveEvent::GateOn { ch, note, instr, vel, hold_ms } => {
                        if tr.muted[ch as usize] {
                            continue;
                        }
                        if let Some(v) = voices.get_mut(ch as usize) {
                            let idx = (instr as usize).min(INSTRUMENTS - 1);
                            v.gate_on(midi_to_hz(note), tr.instruments[idx], vel);
                            if let Some(ms) = hold_ms {
                                v.auto_release = ((ms as f32) * sample_rate * 0.001) as u32;
                            }
                        }
                    }
                    LiveEvent::GateOff { ch } => {
                        if let Some(v) = voices.get_mut(ch as usize) {
                            v.gate_off();
                        }
                    }
                    LiveEvent::AllOff => {
                        for v in &mut voices {
                            v.kill();
                        }
                    }
                }
            }

            // Transitions: on stop, silence hanging voices; on start, reset step timer
            // so step 0 re-gates cleanly on the next tick.
            if tr.playing && !was_playing {
                last_step = usize::MAX;
                sample_in_step = 0;
                tr.step = 0;
            } else if !tr.playing && was_playing {
                for v in &mut voices {
                    v.kill();
                }
            }
            was_playing = tr.playing;

            let spb = (sample_rate * 60.0 / tr.bpm.max(1) as f32 / 4.0).max(1.0) as u32;
            for frame in data.chunks_mut(out_channels) {
                if tr.playing && tr.step != last_step {
                    last_step = tr.step;
                    for (ch, v) in voices.iter_mut().enumerate() {
                        if tr.muted[ch] {
                            v.gate_off();
                            continue;
                        }
                        let cell = tr.phrase.cells[tr.step][ch];
                        if let Some(n) = cell.note {
                            let idx = (cell.instr as usize).min(INSTRUMENTS - 1);
                            // vol=0 is treated as "default/full" so notes entered
                            // in insert mode (which leaves vol=0) play normally.
                            // vol=1..=15 maps linearly to 1/15..=1.0.
                            let vel = if cell.vol == 0 {
                                1.0
                            } else {
                                (cell.vol as f32 / 15.0).min(1.0)
                            };
                            v.gate_on(midi_to_hz(n), tr.instruments[idx], vel);
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
                if tr.playing {
                    sample_in_step += 1;
                    if sample_in_step >= spb {
                        sample_in_step = 0;
                        tr.step = (tr.step + 1) % STEPS_PER_PHRASE;
                    }
                }
            }
            // Stage 9: publish the latest state so the UI visualizer has
            // something to read on its next tick. One write per callback
            // (≈hundreds of Hz at 512-frame buffers) is plenty for 60Hz UI.
            let mut voices_out = [VoiceFrame::default(); CHANNELS];
            for (i, v) in voices.iter().enumerate() {
                voices_out[i] = VoiceFrame {
                    // `gate` reports "note held" — Attack/Decay/Sustain only.
                    // Release counts as note-off even though the envelope is
                    // still audible, so modulation bindings on `ch.gate` fall
                    // cleanly at note-off instead of latching on through the
                    // release tail.
                    gate: matches!(v.env, EnvPhase::Attack | EnvPhase::Decay | EnvPhase::Sustain),
                    env_level: v.level,
                    freq: v.freq,
                    vel: v.vel,
                };
            }
            tr.frame = VizFrame {
                playing: tr.playing,
                step: tr.step,
                step_phase: (sample_in_step as f32 / spb as f32).min(1.0),
                voices: voices_out,
            };
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Phrase};

    fn demo_phrase() -> Phrase {
        let mut p = Phrase::default();
        // PU1 hit on step 0, step 4, step 8, step 12.
        for s in (0..STEPS_PER_PHRASE).step_by(4) {
            p.cells[s][0] = Cell {
                note: Some(60 + (s as u8 % 12)),
                instr: 0,
                vol: 0,
                fx: None,
            };
        }
        // NOI hit every step (kick).
        for s in 0..STEPS_PER_PHRASE {
            p.cells[s][3] = Cell { note: Some(40), instr: 0, vol: 0, fx: None };
        }
        p
    }

    #[test]
    fn bounce_writes_valid_wav_header_and_nonzero_audio() {
        let path = std::env::temp_dir().join("viper_bounce_test.wav");
        let _ = std::fs::remove_file(&path);
        let instr = [Instrument::default(); INSTRUMENTS];
        let phrase = demo_phrase();
        let frames = bounce_to_wav(&path, &phrase, &instr, 140, 1, 44_100)
            .expect("bounce should succeed");

        // At 140 BPM, 16 steps = 60/140 * 4 = ~1.714 sec, plus release tail.
        // 44100 * 1.7 ≈ 75k frames — anything under 60k would be suspicious.
        assert!(frames > 60_000, "too few frames: {}", frames);

        let bytes = std::fs::read(&path).expect("read back");
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[36..40], b"data");

        // Count non-zero samples — silence would fail this.
        let data = &bytes[44..];
        let nonzero = data
            .chunks(2)
            .filter(|c| i16::from_le_bytes([c[0], c[1]]) != 0)
            .count();
        assert!(nonzero > 1000, "too few non-zero samples: {}", nonzero);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bounce_rejects_zero_loops() {
        let path = std::env::temp_dir().join("viper_bounce_zero.wav");
        let instr = [Instrument::default(); INSTRUMENTS];
        let err = bounce_to_wav(&path, &Phrase::default(), &instr, 140, 0, 44_100)
            .expect_err("zero loops should fail");
        assert!(err.to_string().contains("loops"));
    }
}
