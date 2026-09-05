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
