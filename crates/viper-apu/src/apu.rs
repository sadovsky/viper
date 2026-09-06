//! 2A03 APU: two pulses, triangle, noise, DMC, frame counter, and the
//! non-linear mixer. Clocked per CPU cycle. Output is the mixer level in
//! roughly 0..1 (positive, DC-offset — the host high-passes it).
//!
//! Stem support: `mask` silences channels at the mixer without touching
//! their state, so a solo render is timing-identical to the full mix.
//! `dmc_filter` does the same per DPCM sample address.

pub const CH_PU1: u8 = 1 << 0;
pub const CH_PU2: u8 = 1 << 1;
pub const CH_TRI: u8 = 1 << 2;
pub const CH_NOI: u8 = 1 << 3;
pub const CH_DMC: u8 = 1 << 4;
pub const CH_ALL: u8 = 0x1F;

const LENGTH_TABLE: [u8; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 14, 12, 26, 14,
    12, 16, 24, 18, 48, 20, 96, 22, 192, 24, 72, 26, 16, 28, 32, 30,
];
const DUTY: [[u8; 8]; 4] = [
    [0, 1, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 0, 0, 0, 0, 0],
    [0, 1, 1, 1, 1, 0, 0, 0],
    [1, 0, 0, 1, 1, 1, 1, 1],
];
const NOISE_PERIOD: [u16; 16] = [4, 8, 16, 32, 64, 96, 128, 160, 202, 254, 380, 508, 762, 1016, 2034, 4068];
/// DMC timer periods in CPU cycles per output bit, indexed by the $4010 rate.
pub const DMC_RATE: [u16; 16] = [428, 380, 340, 320, 286, 254, 226, 214, 190, 160, 142, 128, 106, 84, 72, 54];

#[derive(Clone, Default)]
struct Envelope {
    start: bool,
    loop_: bool,
    constant: bool,
    volume: u8,
    divider: u8,
    decay: u8,
}

impl Envelope {
    fn clock(&mut self) {
        if self.start {
            self.start = false;
            self.decay = 15;
            self.divider = self.volume;
        } else if self.divider == 0 {
            self.divider = self.volume;
            if self.decay > 0 {
                self.decay -= 1;
            } else if self.loop_ {
                self.decay = 15;
            }
        } else {
            self.divider -= 1;
        }
    }
    fn output(&self) -> u8 {
        if self.constant { self.volume } else { self.decay }
    }
}

/// The levels this envelope generator produces over `clocks` quarter-frames,
/// restarting at `restart_at` if given.
///
/// Exists so `trace.rs`'s reimplementation can be checked against this one.
/// Comparing observable output rather than internal state keeps both structs
/// private and still catches any divergence that could reach a listener.
#[cfg(test)]
pub(crate) fn envelope_outputs(volume: u8, loop_: bool, constant: bool, restart_at: Option<usize>, clocks: usize) -> Vec<u8> {
    let mut e = Envelope { start: true, loop_, constant, volume, divider: 0, decay: 0 };
    (0..clocks)
        .map(|i| {
            if restart_at == Some(i) {
                e.start = true;
            }
            e.clock();
            e.output()
        })
        .collect()
}

#[derive(Clone, Default)]
struct Pulse {
    enabled: bool,
    duty: u8,
    seq: u8,
    timer: u16,
    counter: u16,
    length: u8,
    halt: bool,
    env: Envelope,
    // sweep
    sweep_enabled: bool,
    sweep_period: u8,
    sweep_negate: bool,
    sweep_shift: u8,
    sweep_reload: bool,
    sweep_divider: u8,
    is_pulse1: bool,
}

impl Pulse {
    fn target_period(&self) -> i32 {
        let change = (self.timer >> self.sweep_shift) as i32;
        if self.sweep_negate {
            self.timer as i32 - change - if self.is_pulse1 { 1 } else { 0 }
        } else {
            self.timer as i32 + change
        }
    }
    fn muted(&self) -> bool {
        self.timer < 8 || self.target_period() > 0x7FF
    }
    fn clock_timer(&mut self) {
        if self.counter == 0 {
            self.counter = self.timer;
            self.seq = (self.seq + 1) & 7;
        } else {
            self.counter -= 1;
        }
    }
    fn clock_sweep(&mut self) {
        if self.sweep_divider == 0 && self.sweep_enabled && self.sweep_shift > 0 && !self.muted() {
            let t = self.target_period();
            if t >= 0 {
                self.timer = t as u16;
            }
        }
        if self.sweep_divider == 0 || self.sweep_reload {
            self.sweep_divider = self.sweep_period;
            self.sweep_reload = false;
        } else {
            self.sweep_divider -= 1;
        }
    }
    fn clock_length(&mut self) {
        if !self.halt && self.length > 0 {
            self.length -= 1;
        }
    }
    fn output(&self) -> u8 {
        if !self.enabled || self.length == 0 || self.muted() || DUTY[self.duty as usize][self.seq as usize] == 0 {
            0
        } else {
            self.env.output()
        }
    }
}

#[derive(Clone, Default)]
struct Triangle {
    enabled: bool,
    control: bool,
    linear_reload: u8,
    linear: u8,
    reload_flag: bool,
    timer: u16,
    counter: u16,
    length: u8,
    seq: u8,
}

impl Triangle {
    fn clock_timer(&mut self) {
        if self.counter == 0 {
            self.counter = self.timer;
            if self.linear > 0 && self.length > 0 {
                self.seq = (self.seq + 1) & 31;
            }
        } else {
            self.counter -= 1;
        }
    }
    fn clock_linear(&mut self) {
        if self.reload_flag {
            self.linear = self.linear_reload;
        } else if self.linear > 0 {
            self.linear -= 1;
        }
        if !self.control {
            self.reload_flag = false;
        }
    }
    fn clock_length(&mut self) {
        if !self.control && self.length > 0 {
            self.length -= 1;
        }
    }
    fn output(&self) -> u8 {
        // Ultrasonic periods: emulate the averaged 7.5 as 7 to avoid a pop.
        if self.timer < 2 {
            return 7;
        }
        let s = self.seq;
        if s < 16 { 15 - s } else { s - 16 }
    }
}

#[derive(Clone)]
struct Noise {
    enabled: bool,
    mode: bool,
    timer: u16,
    counter: u16,
    shift: u16,
    length: u8,
    halt: bool,
    env: Envelope,
}

impl Default for Noise {
    fn default() -> Self {
        Self { enabled: false, mode: false, timer: 4, counter: 0, shift: 1, length: 0, halt: false, env: Envelope::default() }
    }
}

impl Noise {
    fn clock_timer(&mut self) {
        if self.counter == 0 {
            self.counter = self.timer;
            let fb = (self.shift & 1) ^ if self.mode { (self.shift >> 6) & 1 } else { (self.shift >> 1) & 1 };
            self.shift = (self.shift >> 1) | (fb << 14);
        } else {
            self.counter -= 1;
        }
    }
    fn clock_length(&mut self) {
        if !self.halt && self.length > 0 {
            self.length -= 1;
        }
    }
    fn output(&self) -> u8 {
        if !self.enabled || self.length == 0 || self.shift & 1 != 0 { 0 } else { self.env.output() }
    }
}

#[derive(Clone, Default)]
struct Dmc {
    irq_enable: bool,
    loop_: bool,
    rate: u16,
    counter: u16,
    level: u8,
    sample_addr: u16,
    sample_len: u16,
    addr: u16,
    remaining: u16,
    buffer: Option<u8>,
    shift: u8,
    bits: u8,
    silence: bool,
    /// $4012 value of the sample currently loaded (for stem filtering).
    playing_addr_reg: u8,
    addr_reg: u8,
    /// Sample-level mute for stems: the counter keeps running, the mixer sees 0.
    muted: bool,
}

/// Signals raised by the DMC that the host records for trigger export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmcStart {
    pub addr_reg: u8,
}

#[derive(Clone)]
pub struct Apu {
    pu1: Pulse,
    pu2: Pulse,
    tri: Triangle,
    noi: Noise,
    dmc: Dmc,
    frame_mode5: bool,
    frame_cycle: u32,
    /// Pending $4017 write applied after a few cycles (hardware delay).
    frame_reset_delay: u8,
    cycle_parity: bool,
    pub mask: u8,
    /// When set, only DPCM samples whose $4012 value matches are audible.
    pub dmc_filter: Option<u8>,
    pulse_table: [f32; 31],
    tnd_table: [f32; 203],
    pub last_dmc_start: Option<DmcStart>,
    pub noise_trigger: bool,
}

impl Default for Apu {
    fn default() -> Self {
        Self::new()
    }
}

impl Apu {
    pub fn new() -> Self {
        let mut pulse_table = [0f32; 31];
        for (i, v) in pulse_table.iter_mut().enumerate().skip(1) {
            *v = 95.52 / (8128.0 / i as f32 + 100.0);
        }
        let mut tnd_table = [0f32; 203];
        for (i, v) in tnd_table.iter_mut().enumerate().skip(1) {
            *v = 163.67 / (24329.0 / i as f32 + 100.0);
        }
        let mut pu1 = Pulse::default();
        pu1.is_pulse1 = true;
        Self {
            pu1,
            pu2: Pulse::default(),
            tri: Triangle::default(),
            noi: Noise::default(),
            dmc: Dmc { rate: DMC_RATE[0], bits: 8, silence: true, ..Default::default() },
            frame_mode5: false,
            frame_cycle: 0,
            frame_reset_delay: 0,
            cycle_parity: false,
            mask: CH_ALL,
            dmc_filter: None,
            pulse_table,
            tnd_table,
            last_dmc_start: None,
            noise_trigger: false,
        }
    }

    pub fn write(&mut self, addr: u16, v: u8) {
        match addr {
            0x4000 | 0x4004 => {
                let p = if addr == 0x4000 { &mut self.pu1 } else { &mut self.pu2 };
                p.duty = v >> 6;
                p.halt = v & 0x20 != 0;
                p.env.loop_ = v & 0x20 != 0;
                p.env.constant = v & 0x10 != 0;
                p.env.volume = v & 0x0F;
            }
            0x4001 | 0x4005 => {
                let p = if addr == 0x4001 { &mut self.pu1 } else { &mut self.pu2 };
                p.sweep_enabled = v & 0x80 != 0;
                p.sweep_period = (v >> 4) & 7;
                p.sweep_negate = v & 0x08 != 0;
                p.sweep_shift = v & 7;
                p.sweep_reload = true;
            }
            0x4002 | 0x4006 => {
                let p = if addr == 0x4002 { &mut self.pu1 } else { &mut self.pu2 };
                p.timer = (p.timer & 0x700) | v as u16;
            }
            0x4003 | 0x4007 => {
                let p = if addr == 0x4003 { &mut self.pu1 } else { &mut self.pu2 };
                p.timer = (p.timer & 0xFF) | ((v as u16 & 7) << 8);
                if p.enabled {
                    p.length = LENGTH_TABLE[(v >> 3) as usize];
                }
                p.seq = 0;
                p.env.start = true;
            }
            0x4008 => {
                self.tri.control = v & 0x80 != 0;
                self.tri.linear_reload = v & 0x7F;
            }
            0x400A => self.tri.timer = (self.tri.timer & 0x700) | v as u16,
            0x400B => {
                self.tri.timer = (self.tri.timer & 0xFF) | ((v as u16 & 7) << 8);
                if self.tri.enabled {
                    self.tri.length = LENGTH_TABLE[(v >> 3) as usize];
                }
                self.tri.reload_flag = true;
            }
            0x400C => {
                self.noi.halt = v & 0x20 != 0;
                self.noi.env.loop_ = v & 0x20 != 0;
                self.noi.env.constant = v & 0x10 != 0;
                self.noi.env.volume = v & 0x0F;
            }
            0x400E => {
                self.noi.mode = v & 0x80 != 0;
                self.noi.timer = NOISE_PERIOD[(v & 0x0F) as usize] - 1;
            }
            0x400F => {
                if self.noi.enabled {
                    self.noi.length = LENGTH_TABLE[(v >> 3) as usize];
                }
                self.noi.env.start = true;
                self.noise_trigger = true;
            }
            0x4010 => {
                self.dmc.irq_enable = v & 0x80 != 0;
                self.dmc.loop_ = v & 0x40 != 0;
                self.dmc.rate = DMC_RATE[(v & 0x0F) as usize];
            }
            0x4011 => self.dmc.level = v & 0x7F,
            0x4012 => {
                self.dmc.addr_reg = v;
                self.dmc.sample_addr = 0xC000 | ((v as u16) << 6);
            }
            0x4013 => self.dmc.sample_len = ((v as u16) << 4) | 1,
            0x4015 => {
                self.pu1.enabled = v & 1 != 0;
                self.pu2.enabled = v & 2 != 0;
                self.tri.enabled = v & 4 != 0;
                self.noi.enabled = v & 8 != 0;
                if !self.pu1.enabled { self.pu1.length = 0; }
                if !self.pu2.enabled { self.pu2.length = 0; }
                if !self.tri.enabled { self.tri.length = 0; }
                if !self.noi.enabled { self.noi.length = 0; }
                if v & 0x10 != 0 {
                    if self.dmc.remaining == 0 {
                        self.dmc.addr = self.dmc.sample_addr;
                        self.dmc.remaining = self.dmc.sample_len;
                        self.dmc.playing_addr_reg = self.dmc.addr_reg;
                        self.dmc.muted = matches!(self.dmc_filter, Some(f) if f != self.dmc.addr_reg);
                        self.last_dmc_start = Some(DmcStart { addr_reg: self.dmc.addr_reg });
                    }
                } else {
                    self.dmc.remaining = 0;
                }
            }
            0x4017 => {
                self.frame_mode5 = v & 0x80 != 0;
                // Reset takes effect 3-4 cycles later; in 5-step mode the
                // quarter+half clocks fire immediately.
                self.frame_reset_delay = if self.cycle_parity { 3 } else { 4 };
                if self.frame_mode5 {
                    self.clock_quarter();
                    self.clock_half();
                }
            }
            _ => {}
        }
    }

    pub fn read_status(&self) -> u8 {
        let mut s = 0;
        if self.pu1.length > 0 { s |= 1; }
        if self.pu2.length > 0 { s |= 2; }
        if self.tri.length > 0 { s |= 4; }
        if self.noi.length > 0 { s |= 8; }
        if self.dmc.remaining > 0 { s |= 0x10; }
        s
    }

    fn clock_quarter(&mut self) {
        self.pu1.env.clock();
        self.pu2.env.clock();
        self.noi.env.clock();
        self.tri.clock_linear();
    }

    fn clock_half(&mut self) {
        self.pu1.clock_length();
        self.pu2.clock_length();
        self.tri.clock_length();
        self.noi.clock_length();
        self.pu1.clock_sweep();
        self.pu2.clock_sweep();
    }

    /// Advance one CPU cycle. `fetch` is called when the DMC needs a
    /// sample byte. Returns the mixer output for this cycle.
    pub fn clock<F: FnMut(u16) -> u8>(&mut self, mut fetch: F) -> f32 {
        // frame counter (CPU-cycle resolution; APU cycle = 2 CPU cycles)
        if self.frame_reset_delay > 0 {
            self.frame_reset_delay -= 1;
            if self.frame_reset_delay == 0 {
                self.frame_cycle = 0;
            }
        }
        self.frame_cycle += 1;
        match self.frame_cycle {
            7457 | 22371 => self.clock_quarter(),
            14913 => { self.clock_quarter(); self.clock_half(); }
            29829 => {
                if !self.frame_mode5 {
                    self.clock_quarter();
                    self.clock_half();
                }
            }
            29830 => {
                if !self.frame_mode5 {
                    self.frame_cycle = 0;
                }
            }
            37281 => { self.clock_quarter(); self.clock_half(); }
            37282 => self.frame_cycle = 0,
            _ => {}
        }

        // timers
        self.cycle_parity = !self.cycle_parity;
        if self.cycle_parity {
            self.pu1.clock_timer();
            self.pu2.clock_timer();
            self.noi.clock_timer();
        }
        self.tri.clock_timer();

        // DMC
        if self.dmc.buffer.is_none() && self.dmc.remaining > 0 {
            let b = fetch(self.dmc.addr);
            self.dmc.buffer = Some(b);
            self.dmc.addr = if self.dmc.addr == 0xFFFF { 0x8000 } else { self.dmc.addr + 1 };
            self.dmc.remaining -= 1;
            if self.dmc.remaining == 0 && self.dmc.loop_ {
                self.dmc.addr = self.dmc.sample_addr;
                self.dmc.remaining = self.dmc.sample_len;
            }
        }
        if self.dmc.counter == 0 {
            self.dmc.counter = self.dmc.rate - 1;
            if !self.dmc.silence {
                if self.dmc.shift & 1 != 0 {
                    if self.dmc.level <= 125 { self.dmc.level += 2; }
                } else if self.dmc.level >= 2 {
                    self.dmc.level -= 2;
                }
                self.dmc.shift >>= 1;
            }
            self.dmc.bits -= 1;
            if self.dmc.bits == 0 {
                self.dmc.bits = 8;
                match self.dmc.buffer.take() {
                    Some(b) => { self.dmc.shift = b; self.dmc.silence = false; }
                    None => self.dmc.silence = true,
                }
            }
        } else {
            self.dmc.counter -= 1;
        }

        // mixer
        let m = self.mask;
        let p1 = if m & CH_PU1 != 0 { self.pu1.output() } else { 0 };
        let p2 = if m & CH_PU2 != 0 { self.pu2.output() } else { 0 };
        let t = if m & CH_TRI != 0 { self.tri.output() } else { 0 };
        let n = if m & CH_NOI != 0 { self.noi.output() } else { 0 };
        let d = if m & CH_DMC != 0 && !self.dmc.muted { self.dmc.level } else { 0 };
        self.pulse_table[(p1 + p2) as usize] + self.tnd_table[(3 * t as usize) + (2 * n as usize) + d as usize]
    }

    /// Per-channel levels for the visualizer: (pu1, pu2, tri, noi, dmc) 0..15/127.
    pub fn levels(&self) -> [u8; 5] {
        [
            if self.pu1.length > 0 && !self.pu1.muted() { self.pu1.env.output() } else { 0 },
            if self.pu2.length > 0 && !self.pu2.muted() { self.pu2.env.output() } else { 0 },
            if self.tri.length > 0 && self.tri.linear > 0 { 15 } else { 0 },
            if self.noi.length > 0 { self.noi.env.output() } else { 0 },
            if self.dmc.remaining > 0 || self.dmc.buffer.is_some() { 15 } else { 0 },
        ]
    }

    /// Timer periods for the visualizer: (pu1, pu2, tri, noi_index).
    pub fn periods(&self) -> [u16; 4] {
        [self.pu1.timer, self.pu2.timer, self.tri.timer, self.noi.timer]
    }
}

/// Play a DPCM sample through the real DMC model and return the 7-bit
/// output level once per output bit (i.e. at the sample's own rate). The
/// sample is placed at $C000; `start_level` is what $4011 held before the
/// hit. Length is taken from the data (16n+1 rule applied by the caller).
pub fn decode_dmc(data: &[u8], rate_idx: u8, start_level: u8) -> Vec<u8> {
    let mut apu = Apu::new();
    let period = DMC_RATE[(rate_idx & 15) as usize] as u32;
    apu.write(0x4010, rate_idx & 15);
    apu.write(0x4011, start_level & 0x7F);
    apu.write(0x4012, 0);
    let len_reg = (data.len().saturating_sub(1) / 16) as u8;
    apu.write(0x4013, len_reg);
    apu.write(0x4015, 0x10);
    let total_bits = data.len() * 8;
    // The output unit starts with an empty shift register: the first
    // byte's bits play after one 8-bit silence cycle. Run that plus every
    // data bit, one sample per timer period, then drop the lead-in.
    const LEAD: usize = 8;
    let mut out = Vec::with_capacity(total_bits + LEAD);
    let mut cycles: u32 = 0;
    let mut next_sample = period;
    while out.len() < total_bits + LEAD {
        apu.clock(|a| data.get((a as usize).wrapping_sub(0xC000)).copied().unwrap_or(0x55));
        cycles += 1;
        if cycles == next_sample {
            next_sample += period;
            out.push(apu.dmc.level);
        }
    }
    out.drain(..LEAD);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An APU with every channel enabled, as a driver leaves it after INIT.
    fn apu() -> Apu {
        let mut a = Apu::new();
        a.write(0x4015, 0x0F);
        a
    }

    /// Run `n` CPU cycles with no DPCM data behind the bus.
    fn run(a: &mut Apu, n: u32) {
        for _ in 0..n {
            a.clock(|_| 0);
        }
    }

    #[test]
    fn the_mixer_reaches_full_scale_without_clipping() {
        let a = Apu::new();
        // Both published constants conspire so that everything at maximum —
        // two pulses at 15, triangle 15, noise 15, DMC 127 — lands one part
        // in fifty thousand *below* unity. That is why the host never clips,
        // and it is a coincidence of 95.52 and 163.67 that no refactor of
        // these tables is allowed to break.
        // Pinned tightly, because these are published hardware figures
        // rather than tuning knobs: 95.52 and 163.67 come from the chip, and
        // a plausible-looking 95.5 would shift every render by a hair that
        // no ear and no loose tolerance would catch.
        assert!((a.pulse_table[30] - 0.257_512_6).abs() < 1e-6, "{}", a.pulse_table[30]);
        assert!((a.tnd_table[202] - 0.742_467_6).abs() < 1e-6, "{}", a.tnd_table[202]);
        let full = a.pulse_table[30] + a.tnd_table[202];
        assert!((full - 0.999_980).abs() < 1e-5, "full scale is {}", full);
        assert_eq!(a.pulse_table[0], 0.0);
        assert_eq!(a.tnd_table[0], 0.0);
        assert!(a.pulse_table.iter().chain(a.tnd_table.iter()).all(|v| v.is_finite()));
        assert!(a.pulse_table.windows(2).all(|w| w[1] > w[0]), "monotone");
    }

    #[test]
    fn the_mixer_is_not_linear() {
        // Two pulses at volume 15 are quieter than twice one of them. This
        // is the whole reason for the lookup tables — a linear sum would be
        // both wrong and louder — so assert the inequality rather than the
        // formula, which would just restate the source.
        let a = Apu::new();
        assert!(a.pulse_table[30] < 2.0 * a.pulse_table[15]);
    }

    #[test]
    fn the_noise_lfsr_has_the_periods_the_hardware_does() {
        // 32767 in long mode, 93 in short: the published figures, and the
        // reason a snare sounds like a snare rather than a buzz. A
        // one-character slip here — `>> 6` for `>> 7`, `<< 14` for `<< 15` —
        // changes the timbre of every drum in every song and is completely
        // invisible to the golden register log, which only records the
        // $400E write that chose the mode.
        for (mode, want) in [(false, 32767u32), (true, 93)] {
            let mut n = Noise { mode, timer: 0, ..Noise::default() };
            let start = n.shift;
            let mut steps = 0u32;
            loop {
                n.clock_timer();
                steps += 1;
                assert_ne!(n.shift, 0, "the register must never reach zero, or it locks up");
                if n.shift == start {
                    break;
                }
                assert!(steps <= 40000, "no cycle found in {} mode", if mode { "short" } else { "long" });
            }
            assert_eq!(steps, want, "{} mode", if mode { "short" } else { "long" });
        }
    }

    #[test]
    fn a_length_counter_expires_on_the_frame_sequencers_tenth_half_clock() {
        // Length index 0 loads 10. In 4-step mode the half-frame clocks fall
        // at CPU cycles 14913 and 29829 of each 29830-cycle period, so the
        // tenth lands at 149,149 — and that one number pins the length
        // table, both sequencer positions and the period reset together.
        let mut a = apu();
        a.write(0x4000, 0x10); // constant volume, halt clear so the counter runs
        a.write(0x4003, 0x00); // length index 0 -> 10
        run(&mut a, 149_148);
        assert_eq!(a.read_status() & 1, 1, "still sounding one cycle before");
        run(&mut a, 1);
        assert_eq!(a.read_status() & 1, 0, "expired at 149,149");
    }

    #[test]
    fn a_pulse_is_muted_by_its_sweep_target_even_when_the_sweep_is_disabled() {
        // The rule people get wrong: `muted()` is consulted by `output()`
        // unconditionally, so a period whose sweep *target* overflows 11 bits
        // silences the channel whether or not the sweep unit is running.
        let mut a = apu();
        a.write(0x4000, 0x1F); // constant volume 15
        a.write(0x4001, 0x01); // sweep DISABLED (bit 7 clear), shift 1
        a.write(0x4002, 0x00);
        a.write(0x4003, 0x0E); // timer 0x600, length index 1
        assert_eq!(a.pu1.timer, 0x600);
        assert!(a.pu1.target_period() > 0x7FF, "target is {}", a.pu1.target_period());
        assert!(a.pu1.muted());
        assert_eq!(a.pu1.output(), 0, "silent despite the sweep being off");
        // And the other half of the rule: a period below 8 mutes too.
        a.write(0x4002, 0x07);
        a.write(0x4003, 0x08);
        assert!(a.pu1.muted());
    }

    #[test]
    fn the_two_pulses_negate_differently() {
        // Pulse 1 subtracts an extra one where pulse 2 does not — ones'
        // complement against two's. It is a real hardware asymmetry, it is
        // why the two channels drift apart under a downward sweep, and it is
        // exactly the kind of off-by-one a rewrite would tidy away.
        let mut a = apu();
        for base in [0x4000u16, 0x4004] {
            a.write(base, 0x1F); // constant volume 15
            a.write(base + 1, 0x89); // sweep on, period 0, negate, shift 1
            a.write(base + 2, 0x00);
            a.write(base + 3, 0x0A); // timer 0x200, length index 1
        }
        assert_eq!(a.pu1.target_period(), 0xFF, "0x200 - 0x100 - 1");
        assert_eq!(a.pu2.target_period(), 0x100, "0x200 - 0x100");
        a.clock_half();
        assert_eq!(a.pu2.timer - a.pu1.timer, 1, "the swept periods stay one apart");
    }

    #[test]
    fn a_sweep_never_writes_back_a_negative_period() {
        // `clock_sweep` guards its write-back with `t >= 0`. That guard is
        // unreachable in practice and it is worth knowing why rather than
        // deleting it: a negative target needs a shift of 0, but a shift of
        // 0 also fails the `sweep_shift > 0` condition on the same line, and
        // every period small enough to go negative at a larger shift is
        // already muted by the `timer < 8` rule.
        let mut a = apu();
        a.write(0x4000, 0x1F);
        a.write(0x4001, 0x88); // negate, shift 0 -> target is -1
        a.write(0x4002, 0x00);
        a.write(0x4003, 0x0A);
        assert_eq!(a.pu1.target_period(), -1);
        let before = a.pu1.timer;
        a.clock_half();
        assert_eq!(a.pu1.timer, before, "shift 0 disables the sweep entirely");
    }

    #[test]
    fn a_pulse_waveform_repeats_every_sixteen_cycles_per_period_unit() {
        // The timer clocks on alternate CPU cycles and advances an 8-step
        // sequence every `timer + 1` of those, so one whole waveform is
        // exactly 16 * (timer + 1) CPU cycles. Exact, no floats: after that
        // many clocks both the sequence position and the divider are back
        // where they started.
        for timer in [8u16, 100, 253] {
            let mut a = apu();
            a.write(0x4000, 0xBF); // duty 2, halt, constant volume 15
            a.write(0x4002, (timer & 0xFF) as u8);
            a.write(0x4003, 0x08 | ((timer >> 8) as u8));
            let (seq0, counter0) = (a.pu1.seq, a.pu1.counter);
            let period = 16 * (timer as u32 + 1);
            let mut high = 0u32;
            for _ in 0..period {
                if a.pu1.output() > 0 {
                    high += 1;
                }
                a.clock(|_| 0);
            }
            assert_eq!((a.pu1.seq, a.pu1.counter), (seq0, counter0), "timer {}", timer);
            // Duty 2 is high for four of eight steps.
            assert_eq!(high, period / 2, "duty 2 is a square wave at timer {}", timer);
        }
    }

    #[test]
    fn duty_three_is_the_inverted_quarter_not_a_three_quarter_wave() {
        // The row of the duty table that gets transcribed wrong: 10011111 is
        // 25% duty inverted, so it reads as six steps of eight high.
        assert_eq!(DUTY[3].iter().filter(|&&v| v == 1).count(), 6);
        assert_eq!(DUTY[2].iter().filter(|&&v| v == 1).count(), 4);
        assert_eq!(DUTY[1].iter().filter(|&&v| v == 1).count(), 2);
        assert_eq!(DUTY[0].iter().filter(|&&v| v == 1).count(), 1);
    }

    #[test]
    fn the_triangle_is_gated_by_its_linear_counter() {
        // Quarter-frame clocks decrement the linear counter; when it reaches
        // zero the sequence freezes, which is how a driver silences the
        // triangle without a volume it does not have.
        let mut a = apu();
        a.write(0x4008, 0x01); // control clear, linear reload 1
        a.write(0x400A, 0x40);
        a.write(0x400B, 0x08);
        a.clock_quarter(); // reload_flag was set by $400B: linear = 1
        assert_eq!(a.tri.linear, 1);
        a.clock_quarter(); // no reload now, so it counts down
        assert_eq!(a.tri.linear, 0);
        let seq = a.tri.seq;
        run(&mut a, 500);
        assert_eq!(a.tri.seq, seq, "a zero linear counter freezes the sequence");
    }

    #[test]
    fn an_ultrasonic_triangle_holds_still_instead_of_popping() {
        // Below period 2 the real chip runs faster than it can be sampled;
        // viper returns the averaged mid-level rather than let the sequence
        // alias into a click. Deliberate, documented, and untested until now.
        let mut t = Triangle { timer: 0, ..Default::default() };
        assert_eq!(t.output(), 7);
        t.timer = 1;
        assert_eq!(t.output(), 7);
        t.timer = 2;
        t.seq = 0;
        assert_eq!(t.output(), 15, "at period 2 the sequence is live again");
    }

    #[test]
    fn dmc_fetches_walk_the_sample_and_wrap_from_ffff_to_8000() {
        // $4012 is the sample address in 64-byte units above $C000 and $4013
        // its length in 16-byte units plus one. The wrap at the top of the
        // address space is either right or an infinite loop, and no register
        // log can see it.
        let mut a = apu();
        a.write(0x4010, 0x0F); // fastest rate, no loop
        a.write(0x4012, 0x01); // $C040
        a.write(0x4013, 0x02); // 33 bytes
        a.write(0x4015, 0x1F); // start
        assert_eq!(a.dmc.sample_addr, 0xC040);
        assert_eq!(a.dmc.sample_len, 33);
        // 33 bytes at the fastest rate is 54 cycles a bit, so about 14,300.
        let mut seen = Vec::new();
        for _ in 0..20_000 {
            a.clock(|addr| {
                seen.push(addr);
                0
            });
        }
        assert_eq!(seen.len(), 33, "one fetch per byte, then it stops");
        assert_eq!(seen[0], 0xC040);
        assert_eq!(*seen.last().unwrap(), 0xC060);
        assert!(seen.windows(2).all(|w| w[1] == w[0] + 1), "consecutive");

        // From the top of the address space it wraps to $8000, not past it.
        let mut a = apu();
        a.write(0x4010, 0x0F);
        a.write(0x4012, 0xFF); // $FFC0
        a.write(0x4013, 0x0F);
        a.write(0x4015, 0x1F);
        // 64 bytes from $FFC0 to the top, at 432 cycles a byte.
        let mut seen = Vec::new();
        for _ in 0..40_000 {
            a.clock(|addr| {
                seen.push(addr);
                0
            });
        }
        let at = seen.iter().position(|&x| x == 0xFFFF).expect("reaches the top");
        assert_eq!(seen[at + 1], 0x8000, "wraps to the start of the image");
    }

    #[test]
    fn a_dmc_sample_only_restarts_once_it_has_finished() {
        // $4015 bit 4 is a start, not a retrigger: writing it again while a
        // sample is playing must not reset it, or a drum stutters.
        let mut a = apu();
        a.write(0x4010, 0x0F);
        a.write(0x4012, 0x01);
        a.write(0x4013, 0x0F);
        a.write(0x4015, 0x1F);
        run(&mut a, 200);
        let left = a.dmc.remaining;
        assert!(left > 0 && left < 241);
        a.write(0x4015, 0x1F);
        assert_eq!(a.dmc.remaining, left, "an already-playing sample is untouched");
    }

    #[test]
    fn the_five_step_sequence_is_longer_and_clocks_immediately() {
        // Writing $4017 with bit 7 clocks quarter and half at once — drivers
        // lean on that to resync — and stretches the period from 29,830 to
        // 37,282 cycles, which is one fewer length tick per unit time.
        let mut a = apu();
        a.write(0x4000, 0x10);
        a.write(0x4003, 0x10); // length index 2 -> 20
        let before = a.pu1.length;
        a.write(0x4017, 0x80);
        assert_eq!(a.pu1.length, before - 1, "the write itself clocked a half frame");

        let halves = |mode5: u8, cycles: u32| {
            let mut a = apu();
            a.write(0x4017, mode5);
            a.write(0x4000, 0x10);
            a.write(0x4003, 0xF8); // length index 31 -> 30, long enough not to expire
            let start = a.pu1.length;
            run(&mut a, cycles);
            start - a.pu1.length
        };
        // Over the same 60,000 cycles the 4-step sequence clocks halves at
        // 14913, 29829, 44743 and 59659; the 5-step one only at 14913, 37281
        // and 52195. The same wall-clock time buys fewer length ticks in the
        // longer sequence, which is the whole audible consequence of the mode
        // bit and the reason a driver has to know which one it set.
        assert_eq!(halves(0x00, 60_000), 4);
        assert_eq!(halves(0x80, 60_000), 3);
    }

    #[test]
    fn decode_dmc_matches_hardware_rules() {
        let up = decode_dmc(&[0xFF; 17], 15, 64);
        assert!(up.len() >= 8);
        // eight 1-bits from 64 reach 80 within the first byte's samples
        assert!(up[..16].contains(&80), "{:?}", &up[..16]);
        let down = decode_dmc(&[0x00; 17], 15, 64);
        assert!(down[..16].contains(&48), "{:?}", &down[..16]);
        let top = decode_dmc(&[0xFF; 17], 15, 126);
        assert!(top.iter().all(|&l| l == 126), "clamps at 126");
        let bottom = decode_dmc(&[0x00; 17], 15, 0);
        assert!(bottom.iter().all(|&l| l == 0), "clamps at 0");
        let toggle = decode_dmc(&[0x55; 17], 15, 64);
        assert_eq!(*toggle.last().unwrap(), 64, "0x55 nets zero");
    }
}
