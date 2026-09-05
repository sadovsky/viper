//! Stage 2-3: cpal audio thread with a tiny chip synth.
//! One voice per channel: PU1/PU2 = pulse, TRI = triangle, NOI = 15-bit
//! LFSR noise at NES rates, DPCM = built-in sample preview.
//! Each pitched voice runs an ADSR envelope sourced from the cell's instrument.
//! Stage 5: live gate events — UI can push realtime gate_on/off while stopped.
//! Stage 16-lite: song mode — the transport walks `order` across phrases.
//! Stage 19: APU engine — when a compiled NSF is loaded, playback runs the
//! real driver through viper-apu and the synth only serves live monitoring.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

use crate::{compile, dpcm, Instrument, Phrase, CHANNELS, INSTRUMENTS, STEPS_PER_PHRASE};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Synth,
    Apu,
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
    /// Song mode: when true and `order` is non-empty, the audio thread
    /// advances through `order` at phrase boundaries using `phrases`.
    pub song_mode: bool,
    pub order: Vec<usize>,
    pub loop_pos: usize,
    pub phrases: Vec<Phrase>,
    /// Current order position and the phrase index being played, mirrored
    /// back to the UI so the grid can follow.
    pub order_pos: usize,
    pub playing_phrase: usize,
    /// Stage 23: per-16th sample offsets and per-channel wrap lengths.
    /// Both shape the synth engine only; the APU path plays the NSF.
    pub groove: [i16; 16],
    pub channel_length: [u8; CHANNELS],
    /// Stage 23: `(arrangement slot, position in chain)` per order entry,
    /// mirrored into `VizFrame` so the song pane can highlight the live
    /// slot. Empty without an arrangement.
    pub arrangement_map: Vec<(usize, usize)>,
    /// Which engine drives pattern playback.
    pub engine: Engine,
    /// A compiled NSF for the APU engine, plus a generation counter so the
    /// audio thread knows when to reload it.
    pub nsf: Option<Arc<Vec<u8>>>,
    pub nsf_generation: u64,
    pub frames_per_row: f64,
    /// Set by the audio thread when the APU engine could not start.
    pub engine_error: Option<String>,
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
    /// Stage 23: where the transport is in the song — order position, and
    /// the arrangement slot / chain position it maps to (0 without an
    /// arrangement).
    pub order_pos: usize,
    pub arr_slot: usize,
    pub chain_pos: usize,
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
            song_mode: false,
            order: Vec::new(),
            loop_pos: 0,
            phrases: Vec::new(),
            order_pos: 0,
            playing_phrase: 0,
            groove: [0; 16],
            channel_length: [STEPS_PER_PHRASE as u8; CHANNELS],
            arrangement_map: Vec::new(),
            engine: Engine::Synth,
            nsf: None,
            nsf_generation: 0,
            frames_per_row: 4.0,
            engine_error: None,
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

const NOISE_PERIOD: [u16; 16] = [4, 8, 16, 32, 64, 96, 128, 160, 202, 254, 380, 508, 762, 1016, 2034, 4068];
const CPU_HZ: f32 = 1_789_773.0;

#[derive(Clone)]
struct Voice {
    kind: u8, // 0=PU1, 1=PU2, 2=TRI, 3=NOI, 4=DPCM
    freq: f32,
    phase: f32,
    level: f32,
    /// Per-note velocity in 0..=1, captured at gate-on from the cell's volume column.
    vel: f32,
    env: EnvPhase,
    instrument: Instrument,
    /// NES-style 15-bit LFSR for the noise voice.
    lfsr: u16,
    noise_acc: f32,
    noise_step: f32,
    noise_bit: f32,
    /// DPCM preview: which bank sample is playing and where.
    sample: Option<usize>,
    sample_pos: f32,
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
            lfsr: 1,
            noise_acc: 0.0,
            noise_step: 0.0,
            noise_bit: 1.0,
            sample: None,
            sample_pos: 0.0,
            auto_release: 0,
        }
    }

    fn gate_on(&mut self, note: u8, instr: Instrument, vel: f32) {
        self.freq = midi_to_hz(note);
        self.instrument = instr;
        self.vel = vel.clamp(0.0, 1.0);
        self.env = EnvPhase::Attack;
        // Pattern-driven gate cancels any live auto-release: the step grid
        // is authoritative while playing.
        self.auto_release = 0;
        match self.kind {
            3 => {
                let idx = compile::noise_period_index(note) as usize;
                self.noise_step = CPU_HZ / NOISE_PERIOD[idx] as f32;
            }
            4 => {
                self.sample = Some(dpcm::note_to_sample(note));
                self.sample_pos = 0.0;
                self.level = 1.0;
            }
            _ => {}
        }
        // Don't hard-reset level: retriggers ramp smoothly from the current level.
    }

    fn gate_off(&mut self) {
        // DPCM is one-shot: a release would cut the sample.
        if self.kind == 4 {
            return;
        }
        if !matches!(self.env, EnvPhase::Idle) {
            self.env = EnvPhase::Release;
        }
        self.auto_release = 0;
    }

    fn kill(&mut self) {
        self.env = EnvPhase::Idle;
        self.level = 0.0;
        self.auto_release = 0;
        self.sample = None;
    }

    fn held(&self) -> bool {
        matches!(self.env, EnvPhase::Attack | EnvPhase::Decay | EnvPhase::Sustain)
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

    fn tick(&mut self, sr: f32, bank: &[dpcm::Sample]) -> f32 {
        if self.kind == 4 {
            // One-shot sample playback at the DMC rate, no envelope.
            let Some(si) = self.sample else { return 0.0 };
            let Some(s) = bank.get(si) else { self.sample = None; return 0.0 };
            let idx = self.sample_pos as usize;
            if idx >= s.wave.len() {
                self.sample = None;
                self.env = EnvPhase::Idle;
                self.level = 0.0;
                return 0.0;
            }
            self.env = EnvPhase::Sustain;
            self.sample_pos += dpcm::RATE_HZ / sr;
            return s.wave[idx] * self.vel * 0.9;
        }
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
                // Clock the LFSR at the NES period rate; average the bits
                // that fall inside this output sample.
                self.noise_acc += self.noise_step / sr;
                let mut clocks = self.noise_acc as u32;
                self.noise_acc -= clocks as f32;
                clocks = clocks.min(64);
                for _ in 0..clocks {
                    let fb = (self.lfsr & 1) ^ ((self.lfsr >> 1) & 1);
                    self.lfsr = (self.lfsr >> 1) | (fb << 14);
                    self.noise_bit = if self.lfsr & 1 == 0 { 1.0 } else { -1.0 };
                }
                self.noise_bit
            }
            _ => 0.0,
        };
        raw * self.level * self.instrument.volume * self.vel
    }
}

fn midi_to_hz(note: u8) -> f32 {
    440.0 * 2.0f32.powf((note as f32 - 69.0) / 12.0)
}

fn new_voices() -> [Voice; CHANNELS] {
    std::array::from_fn(|i| Voice::new(i as u8))
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

/// Gate every voice from a step's cells. Shared by the realtime callback
/// and the offline bounce.
/// Stage 23 polymeter: a channel with `channel_length[ch] < 16` reads
/// `cells[step % len]`, cycling inside the phrase.
fn gate_step(
    voices: &mut [Voice; CHANNELS],
    phrase: &Phrase,
    step: usize,
    instruments: &[Instrument; INSTRUMENTS],
    muted: &[bool; CHANNELS],
    channel_length: &[u8; CHANNELS],
) {
    for (ch, v) in voices.iter_mut().enumerate() {
        if muted[ch] {
            v.gate_off();
            continue;
        }
        let len = (channel_length[ch].max(1) as usize).min(STEPS_PER_PHRASE);
        let cell = phrase.cells[step % len][ch];
        if let Some(n) = cell.note {
            let idx = (cell.instr as usize).min(INSTRUMENTS - 1);
            // vol=0 is treated as "default/full" so notes entered in insert
            // mode (which leaves vol=0) play normally. vol=1..=15 maps
            // linearly to 1/15..=1.0.
            let vel = if cell.vol == 0 { 1.0 } else { (cell.vol as f32 / 15.0).min(1.0) };
            v.gate_on(n, instruments[idx], vel);
        } else {
            v.gate_off();
        }
    }
}

/// Samples in a given 16th: the straight step length plus the groove
/// offset for that step, never less than one sample so playback always
/// advances.
fn step_samples(base_spb: u32, groove: &[i16; 16], step: usize) -> u32 {
    ((base_spb as i32) + groove[step % 16] as i32).max(1) as u32
}

/// Stage 15a: render a phrase sequence to 16-bit mono PCM WAV at `path`,
/// offline. Mirrors the realtime step scheduler (same `spb`, same voice
/// model, same groove and polymeter), then keeps rendering after the last
/// step until every voice is Idle or we hit a 2-second tail cap — ensures
/// release tails finish cleanly. Returns the number of audio frames written.
pub fn bounce_to_wav(
    path: &Path,
    sequence: &[Phrase],
    instruments: &[Instrument; INSTRUMENTS],
    bpm: u16,
    loops: u32,
    sample_rate: u32,
    groove: &[i16; 16],
    channel_length: &[u8; CHANNELS],
) -> Result<u32> {
    if loops == 0 {
        bail!("bounce: loops must be ≥ 1");
    }
    if sequence.is_empty() {
        bail!("bounce: nothing to render");
    }
    let sr_f = sample_rate as f32;
    let base_spb = (sr_f * 60.0 / bpm.max(1) as f32 / 4.0).max(1.0) as u32;
    let total_steps = (loops as usize).saturating_mul(STEPS_PER_PHRASE * sequence.len());
    let tail_cap = sample_rate * 2;
    let bank = dpcm::default_bank();

    let mut voices = new_voices();
    let mut samples: Vec<f32> = Vec::with_capacity(
        (total_steps as u64 * base_spb as u64).min(u32::MAX as u64) as usize
    );
    let no_mute = [false; CHANNELS];

    let render_sample = |voices: &mut [Voice; CHANNELS], samples: &mut Vec<f32>| {
        let mut mix = 0.0f32;
        for v in voices.iter_mut() {
            mix += v.tick(sr_f, &bank);
        }
        samples.push((mix * 0.2).clamp(-1.0, 1.0));
    };

    for global_step in 0..total_steps {
        let step = global_step % STEPS_PER_PHRASE;
        let phrase = &sequence[(global_step / STEPS_PER_PHRASE) % sequence.len()];
        gate_step(&mut voices, phrase, step, instruments, &no_mute, channel_length);
        for _ in 0..step_samples(base_spb, groove, step) {
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

/// Stage 19: the APU engine state owned by the audio thread.
struct ApuState {
    generation: u64,
    player: viper_apu::Player,
    buf: Vec<f32>,
    buf_pos: usize,
    frames: u64,
    total_rows: usize,
    loop_row: usize,
}

impl ApuState {
    fn new(nsf: &[u8], generation: u64, sample_rate: u32, order_len: usize, loop_pos: usize) -> Result<Self> {
        let parsed = viper_apu::Nsf::parse(nsf)?;
        let mut player = viper_apu::Player::new(parsed, sample_rate);
        player.keep_log = false;
        player.init(0)?;
        Ok(Self {
            generation,
            player,
            buf: Vec::new(),
            buf_pos: 0,
            frames: 0,
            total_rows: order_len.max(1) * STEPS_PER_PHRASE,
            loop_row: loop_pos.min(order_len.saturating_sub(1)) * STEPS_PER_PHRASE,
        })
    }

    fn next_sample(&mut self) -> f32 {
        if self.buf_pos >= self.buf.len() {
            self.buf_pos = 0;
            // A failing frame means a broken NSF; output silence rather
            // than panic the audio thread.
            if self.player.frame().is_err() {
                self.buf = vec![0.0; 735];
            } else {
                self.buf = self.player.take_samples();
                self.frames += 1;
            }
            if self.buf.is_empty() {
                return 0.0;
            }
        }
        let s = self.buf[self.buf_pos];
        self.buf_pos += 1;
        s
    }

    /// (row within pattern, order position) for the current frame count.
    fn position(&self, frames_per_row: f64) -> (usize, usize) {
        let mut row = (self.frames.saturating_sub(1) as f64 / frames_per_row.max(1.0)) as usize;
        if row >= self.total_rows {
            let span = self.total_rows - self.loop_row;
            row = self.loop_row + (row - self.loop_row) % span.max(1);
        }
        (row % STEPS_PER_PHRASE, row / STEPS_PER_PHRASE)
    }

    fn viz_voices(&self) -> [VoiceFrame; CHANNELS] {
        let lv = self.player.apu.levels();
        let per = self.player.apu.periods();
        let mut out = [VoiceFrame::default(); CHANNELS];
        for ch in 0..CHANNELS {
            let level = lv[ch] as f32 / 15.0;
            let freq = match ch {
                0 | 1 => CPU_HZ / (16.0 * (per[ch] as f32 + 1.0)),
                2 => CPU_HZ / (32.0 * (per[2] as f32 + 1.0)),
                _ => 0.0,
            };
            out[ch] = VoiceFrame { gate: level > 0.0, env_level: level, freq, vel: 1.0 };
        }
        out
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
    let sample_rate_u = config.sample_rate.0;
    let out_channels = config.channels as usize;

    let mut voices = new_voices();
    let bank = dpcm::default_bank();
    let mut sample_in_step: u32 = 0;
    let mut last_step: usize = usize::MAX;
    let mut was_playing = false;
    let mut apu: Option<ApuState> = None;

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
                            v.gate_on(note, tr.instruments[idx], vel);
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

            // Engine selection: APU only when a compiled NSF is available.
            let use_apu = tr.engine == Engine::Apu && tr.nsf.is_some();

            // Transitions: on stop, silence hanging voices; on start, reset step timer
            // so step 0 re-gates cleanly on the next tick.
            if tr.playing && !was_playing {
                last_step = usize::MAX;
                sample_in_step = 0;
                tr.step = 0;
                tr.order_pos = 0;
                if tr.song_mode && !tr.order.is_empty() {
                    tr.playing_phrase = tr.order[0];
                }
                if use_apu {
                    let need = apu.as_ref().map_or(true, |a| a.generation != tr.nsf_generation);
                    if need {
                        let nsf = tr.nsf.clone().unwrap();
                        match ApuState::new(&nsf, tr.nsf_generation, sample_rate_u, tr.order.len(), tr.loop_pos) {
                            Ok(a) => { apu = Some(a); tr.engine_error = None; }
                            Err(e) => { apu = None; tr.engine_error = Some(e.to_string()); }
                        }
                    } else if let Some(a) = apu.as_mut() {
                        // restart from the top
                        let _ = a.player.init(0);
                        a.frames = 0;
                        a.buf.clear();
                        a.buf_pos = 0;
                    }
                }
            } else if !tr.playing && was_playing {
                for v in &mut voices {
                    v.kill();
                }
            }
            was_playing = tr.playing;

            let apu_active = tr.playing && use_apu && apu.is_some();
            if let Some(a) = apu.as_mut() {
                let mut mask = 0u8;
                for ch in 0..CHANNELS {
                    if !tr.muted[ch] { mask |= 1 << ch; }
                }
                a.player.set_mask(mask);
            }

            let base_spb = (sample_rate * 60.0 / tr.bpm.max(1) as f32 / 4.0).max(1.0) as u32;
            let groove = tr.groove;
            let channel_length = tr.channel_length;
            for frame in data.chunks_mut(out_channels) {
                let mut mix = 0.0f32;
                if apu_active {
                    let a = apu.as_mut().unwrap();
                    mix += a.next_sample() * 2.5;
                    // live-monitor voices still sound on top
                    for v in &mut voices {
                        mix += v.tick(sample_rate, &bank) * 0.2;
                    }
                } else {
                    if tr.playing && tr.step != last_step {
                        last_step = tr.step;
                        let muted = tr.muted;
                        let instruments = tr.instruments;
                        let step = tr.step;
                        if tr.song_mode && !tr.order.is_empty() {
                            let pi = tr.playing_phrase.min(tr.phrases.len().saturating_sub(1));
                            if let Some(p) = tr.phrases.get(pi) {
                                gate_step(&mut voices, p, step, &instruments, &muted, &channel_length);
                            }
                        } else {
                            gate_step(&mut voices, &tr.phrase, step, &instruments, &muted, &channel_length);
                        }
                    }
                    for v in &mut voices {
                        mix += v.tick(sample_rate, &bank);
                    }
                    mix *= 0.2;
                }
                let s = mix.clamp(-1.0, 1.0);
                let out = T::from_sample(s);
                for o in frame.iter_mut() {
                    *o = out;
                }
                if tr.playing && !apu_active {
                    sample_in_step += 1;
                    if sample_in_step >= step_samples(base_spb, &groove, tr.step) {
                        sample_in_step = 0;
                        tr.step = (tr.step + 1) % STEPS_PER_PHRASE;
                        if tr.step == 0 && tr.song_mode && !tr.order.is_empty() {
                            let mut next = tr.order_pos + 1;
                            if next >= tr.order.len() {
                                next = tr.loop_pos.min(tr.order.len() - 1);
                            }
                            tr.order_pos = next;
                            tr.playing_phrase = tr.order[next];
                        }
                    }
                }
            }
            if apu_active {
                let a = apu.as_ref().unwrap();
                let (step, opos) = a.position(tr.frames_per_row);
                tr.step = step;
                if !tr.order.is_empty() {
                    tr.order_pos = opos.min(tr.order.len() - 1);
                    tr.playing_phrase = tr.order[tr.order_pos];
                }
            }
            // Stage 9: publish the latest state so the UI visualizer has
            // something to read on its next tick. One write per callback
            // (≈hundreds of Hz at 512-frame buffers) is plenty for 60Hz UI.
            let mut voices_out = [VoiceFrame::default(); CHANNELS];
            if apu_active {
                voices_out = apu.as_ref().unwrap().viz_voices();
            }
            for (i, v) in voices.iter().enumerate() {
                if apu_active && !v.held() {
                    continue;
                }
                voices_out[i] = VoiceFrame {
                    // `gate` reports "note held" — Attack/Decay/Sustain only.
                    // Release counts as note-off even though the envelope is
                    // still audible, so modulation bindings on `ch.gate` fall
                    // cleanly at note-off instead of latching on through the
                    // release tail.
                    gate: v.held(),
                    env_level: v.level,
                    freq: v.freq,
                    vel: v.vel,
                };
            }
            let step_phase = if apu_active {
                let a = apu.as_ref().unwrap();
                ((a.frames as f64 % tr.frames_per_row.max(1.0)) / tr.frames_per_row.max(1.0)) as f32
            } else {
                (sample_in_step as f32 / step_samples(base_spb, &groove, tr.step) as f32).min(1.0)
            };
            let (arr_slot, chain_pos) = tr.arrangement_map.get(tr.order_pos).copied().unwrap_or((0, 0));
            tr.frame = VizFrame {
                playing: tr.playing,
                step: tr.step,
                step_phase,
                voices: voices_out,
                order_pos: tr.order_pos,
                arr_slot,
                chain_pos,
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
        // DPCM kick on the downbeats.
        for s in (0..STEPS_PER_PHRASE).step_by(8) {
            p.cells[s][4] = Cell { note: Some(60), instr: 0, vol: 0, fx: None };
        }
        p
    }

    #[test]
    fn bounce_writes_valid_wav_header_and_nonzero_audio() {
        let path = std::env::temp_dir().join("viper_bounce_test.wav");
        let _ = std::fs::remove_file(&path);
        let instr = [Instrument::default(); INSTRUMENTS];
        let phrase = demo_phrase();
        let frames = bounce_to_wav(&path, &[phrase], &instr, 140, 1, 44_100, &[0; 16], &[16; CHANNELS])
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
        let err = bounce_to_wav(&path, &[Phrase::default()], &instr, 140, 0, 44_100, &[0; 16], &[16; CHANNELS])
            .expect_err("zero loops should fail");
        assert!(err.to_string().contains("loops"));
    }

    #[test]
    fn groove_shifts_steps_but_keeps_the_bar_length() {
        let swing = crate::vip::swing_groove(300);
        assert_eq!(step_samples(10_000, &swing, 0), 9_700);
        assert_eq!(step_samples(10_000, &swing, 1), 10_300);
        let bar: u32 = (0..16).map(|s| step_samples(10_000, &swing, s)).sum();
        assert_eq!(bar, 160_000);
        // A huge negative offset never stalls the clock.
        assert_eq!(step_samples(10, &[-500; 16], 3), 1);
    }

    #[test]
    fn polymeter_wraps_a_short_channel_inside_the_phrase() {
        let mut p = Phrase::default();
        p.cells[0][0] = Cell { note: Some(60), instr: 0, vol: 0, fx: None };
        let instr = [Instrument::default(); INSTRUMENTS];
        let mut len = [STEPS_PER_PHRASE as u8; CHANNELS];
        len[0] = 4;
        let mut voices = new_voices();
        gate_step(&mut voices, &p, 4, &instr, &[false; CHANNELS], &len);
        assert!(voices[0].held(), "PU1 with length 4 retriggers on step 4");
        gate_step(&mut voices, &p, 5, &instr, &[false; CHANNELS], &len);
        assert!(!voices[0].held());
        // Full-length channel: step 4 is empty.
        let mut voices = new_voices();
        gate_step(&mut voices, &p, 4, &instr, &[false; CHANNELS], &[STEPS_PER_PHRASE as u8; CHANNELS]);
        assert!(!voices[0].held());
    }

    #[test]
    fn apu_position_wraps_to_loop_point() {
        // 4 order entries, loop at 1, 4.5 frames per row: rows 0..64 then
        // wrap into rows 16..64 forever.
        let mut st = ApuState {
            generation: 0,
            player: {
                let nsf = viper_apu::Nsf::parse(&{
                    let mut v = viper_nsf_test_header();
                    v.extend_from_slice(&[0x60; 16]); // RTS at $8000
                    v
                }).unwrap();
                viper_apu::Player::new(nsf, 44_100)
            },
            buf: Vec::new(), buf_pos: 0, frames: 0, total_rows: 64, loop_row: 16,
        };
        st.frames = 1;
        assert_eq!(st.position(4.5), (0, 0));
        st.frames = 64 * 45 / 10 + 1; // row 64 -> wraps to row 16
        assert_eq!(st.position(4.5), (0, 1));
        st.frames = 520; // row 115.3 -> one more full loop + 3 rows
        assert_eq!(st.position(4.5), (3, 1));
    }

    fn viper_nsf_test_header() -> Vec<u8> {
        let mut h = vec![0u8; 128];
        h[0..5].copy_from_slice(b"NESM\x1A");
        h[5] = 1; h[6] = 1; h[7] = 1;
        h[8..10].copy_from_slice(&0x8000u16.to_le_bytes());
        h[10..12].copy_from_slice(&0x8000u16.to_le_bytes());
        h[12..14].copy_from_slice(&0x8000u16.to_le_bytes());
        h[0x6E..0x70].copy_from_slice(&0x411Au16.to_le_bytes());
        h
    }

    #[test]
    fn dpcm_voice_is_one_shot() {
        let bank = dpcm::default_bank();
        let mut v = Voice::new(4);
        v.gate_on(60, Instrument::default(), 1.0);
        v.gate_off(); // must not cut the sample
        let mut nonzero = 0;
        for _ in 0..4000 {
            if v.tick(44_100.0, &bank).abs() > 0.0 { nonzero += 1; }
        }
        assert!(nonzero > 100);
        // eventually ends on its own
        for _ in 0..200_000 { v.tick(44_100.0, &bank); }
        assert!(v.sample.is_none());
    }
}
