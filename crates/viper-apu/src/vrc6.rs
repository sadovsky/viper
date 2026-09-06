//! The VRC6 expansion sound chip: two pulses and a sawtooth.
//!
//! A sibling of [`crate::apu::Apu`], not a member of it, because that is the
//! hardware. The VRC6 lives on the cartridge, has its own linear DAC, and is
//! summed onto the audio pin *externally* — it does not go through the 2A03's
//! non-linear mixing tables. Modelling it inside `Apu` would hide that sum
//! inside a struct whose whole contract is those tables.
//!
//! Registers are at `$9000-$9003` (pulse 1), `$A000-$A002` (pulse 2) and
//! `$B000-$B002` (sawtooth). They are write-only and shadow no memory, so ROM
//! reads through that window still work — which matters, because a VRC6
//! driver's own code sits there.

/// Stem masks. The 2A03 uses bits 0-4 (`CH_ALL = 0x1F`), so the three VRC6
/// channels fit in the top three bits of the same `u8` with no type change
/// anywhere in the render path.
pub const CH_VP1: u8 = 1 << 5;
pub const CH_VP2: u8 = 1 << 6;
pub const CH_SAW: u8 = 1 << 7;
pub const CH_VRC6_ALL: u8 = CH_VP1 | CH_VP2 | CH_SAW;

/// One VRC6 unit of output, in the same scale as `Apu::clock`'s return value.
///
/// The chip's DAC is linear, so its digital sum (0..=61) scales by a single
/// constant. nesdev's measurement is that a VRC6 pulse at full volume is about
/// as loud as a 2A03 pulse at full volume, which fixes the scale: one VRC6
/// unit equals one full-scale 2A03 pulse divided by 15.
///
/// ```text
/// pulse_table[15] = 95.52 / (8128/15 + 100) = 0.148_814
/// ```
///
/// Full-scale VRC6 is then 61 × this ≈ 0.605 — roughly four times a single
/// 2A03 pulse, which is the "VRC6 is louder" everyone hears, falling out of
/// the model rather than being tuned by ear.
pub const VRC6_UNIT: f32 = (95.52 / (8128.0 / 15.0 + 100.0)) / 15.0;

/// The VRC6's output is 180° out of phase with the 2A03 on real hardware.
/// Every reference emulator ignores this, and matching them is worth more
/// than matching the cartridge, because verification here means diffing
/// against NSFPlay and Mesen.
const INVERT: bool = false;

/// The ten audio registers, as an exact set rather than a range: `$9000-$B002`
/// spans 8 KB of the ROM window and a mapper write anywhere else in there must
/// stay a ROM write.
pub fn is_vrc6_reg(addr: u16) -> bool {
    matches!(addr, 0x9000..=0x9003 | 0xA000..=0xA002 | 0xB000..=0xB002)
}

#[derive(Clone, Copy, Debug, Default)]
struct Pulse {
    period: u16,
    divider: u16,
    /// Duty generator, counting 15 down to 0 and wrapping.
    step: u8,
    duty: u8,
    volume: u8,
    /// Mode bit: ignore the duty generator and output `volume` constantly.
    mode: bool,
    enabled: bool,
}

impl Pulse {
    /// Clocked every CPU cycle — unlike the 2A03 pulse, which clocks every
    /// *other* cycle. Getting this wrong costs exactly one octave, so it is
    /// worth stating rather than inferring from the period arithmetic.
    fn clock(&mut self, shift: u8) {
        if self.divider == 0 {
            self.divider = self.period >> shift;
            // 16 steps, counting down. `step <= duty` is high, so duty 0 gives
            // 1/16 high and duty 7 gives 8/16 — the VRC6 pulse never exceeds
            // 50%. (Duties are sixteenths, not eighths.)
            self.step = if self.step == 0 { 15 } else { self.step - 1 };
        } else {
            self.divider -= 1;
        }
    }

    fn output(&self) -> u8 {
        if !self.enabled {
            return 0;
        }
        if self.mode || self.step <= self.duty {
            self.volume
        } else {
            0
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Saw {
    period: u16,
    divider: u16,
    /// Position in the 14-clock cycle.
    step: u8,
    /// 6-bit accumulation rate.
    rate: u8,
    accum: u8,
    enabled: bool,
}

impl Saw {
    /// Fourteen divider clocks per cycle. The accumulator is added to on the
    /// *even* clocks only, so `rate` lands six times before the cycle resets
    /// it — not seven. For rate $08 the published sequence is
    /// `00 00 08 08 10 10 18 18 20 20 28 28 30 30` then back to `00`.
    fn clock(&mut self, shift: u8) {
        if self.divider == 0 {
            self.divider = self.period >> shift;
            // Act on the current step, then advance — so the very first clock
            // is observed as step 0 (the reset) rather than skipping past it.
            if self.step == 0 {
                self.accum = 0;
            } else if self.step % 2 == 0 {
                // Wrapping is deliberate: six adds of a rate above 42 overflow
                // the 8-bit accumulator on real hardware, and that buzz is a
                // technique people use, not a bug to clamp away.
                self.accum = self.accum.wrapping_add(self.rate);
            }
            self.step = (self.step + 1) % 14;
        } else {
            self.divider -= 1;
        }
    }

    fn output(&self) -> u8 {
        if self.enabled {
            self.accum >> 3
        } else {
            0
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Vrc6 {
    p1: Pulse,
    p2: Pulse,
    saw: Saw,
    /// `$9003`: freezes all three oscillators where they stand.
    halt: bool,
    /// `$9003`: right-shift applied to each channel's period reload.
    shift: u8,
    /// Stem mask, using `CH_VP1`/`CH_VP2`/`CH_SAW`.
    pub mask: u8,
}

impl Vrc6 {
    pub fn new() -> Self {
        Self { mask: CH_VRC6_ALL, ..Default::default() }
    }

    pub fn write(&mut self, addr: u16, v: u8) {
        match addr {
            0x9000 => {
                self.p1.volume = v & 0x0F;
                self.p1.duty = (v >> 4) & 0x07;
                self.p1.mode = v & 0x80 != 0;
            }
            0x9001 => self.p1.period = (self.p1.period & 0x0F00) | v as u16,
            0x9002 => {
                self.p1.period = (self.p1.period & 0x00FF) | ((v as u16 & 0x0F) << 8);
                self.p1.enabled = v & 0x80 != 0;
                if !self.p1.enabled {
                    // Documented: clearing E resets and halts the duty
                    // generator. Whether the frequency divider also stops is
                    // not stated anywhere; we let it free-run, matching the
                    // saw. It can only affect the phase of the first cycle
                    // after a key-on, but it is a choice, not an accident.
                    self.p1.step = 15;
                }
            }
            0x9003 => {
                self.halt = v & 0x01 != 0;
                self.shift = if v & 0x04 != 0 { 8 } else if v & 0x02 != 0 { 4 } else { 0 };
            }
            0xA000 => {
                self.p2.volume = v & 0x0F;
                self.p2.duty = (v >> 4) & 0x07;
                self.p2.mode = v & 0x80 != 0;
            }
            0xA001 => self.p2.period = (self.p2.period & 0x0F00) | v as u16,
            0xA002 => {
                self.p2.period = (self.p2.period & 0x00FF) | ((v as u16 & 0x0F) << 8);
                self.p2.enabled = v & 0x80 != 0;
                if !self.p2.enabled {
                    self.p2.step = 15;
                }
            }
            0xB000 => self.saw.rate = v & 0x3F,
            0xB001 => self.saw.period = (self.saw.period & 0x0F00) | v as u16,
            0xB002 => {
                self.saw.period = (self.saw.period & 0x00FF) | ((v as u16 & 0x0F) << 8);
                self.saw.enabled = v & 0x80 != 0;
                if !self.saw.enabled {
                    // Documented: the accumulator is forced to zero, but the
                    // frequency divider is *not* reset.
                    self.saw.accum = 0;
                }
            }
            _ => {}
        }
    }

    /// Advance one CPU cycle and return this chip's contribution to the mix.
    pub fn clock(&mut self) -> f32 {
        if !self.halt {
            self.p1.clock(self.shift);
            self.p2.clock(self.shift);
            self.saw.clock(self.shift);
        }
        let m = self.mask;
        let a = if m & CH_VP1 != 0 { self.p1.output() } else { 0 } as u32;
        let b = if m & CH_VP2 != 0 { self.p2.output() } else { 0 } as u32;
        let c = if m & CH_SAW != 0 { self.saw.output() } else { 0 } as u32;
        let sum = (a + b + c) as f32 * VRC6_UNIT;
        if INVERT { -sum } else { sum }
    }

    /// Per-channel levels for the visualizer: (vp1, vp2, saw), 0..15/0..31.
    pub fn levels(&self) -> [u8; 3] {
        [self.p1.output(), self.p2.output(), self.saw.output()]
    }

    /// Raw period registers, for deriving displayed frequency.
    pub fn periods(&self) -> [u16; 3] {
        [self.p1.period, self.p2.period, self.saw.period]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saw_at(rate: u8, cycles: usize) -> Vec<u8> {
        let mut v = Vrc6::new();
        v.write(0xB000, rate);
        v.write(0xB001, 0);
        v.write(0xB002, 0x80); // enable, period hi = 0
        (0..cycles).map(|_| { v.clock(); v.levels()[2] }).collect()
    }

    #[test]
    fn the_saw_climbs_six_steps_then_resets() {
        // The nesdev table for rate $08, verbatim: the accumulator is added to
        // on even clocks only, six times, and the seventh accumulator clock
        // resets it. Output is the top five bits.
        let out = saw_at(0x08, 28);
        let expect: Vec<u8> = vec![0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6];
        assert_eq!(out[..14], expect[..], "first cycle");
        assert_eq!(out[14..28], expect[..], "and it repeats");
    }

    #[test]
    fn the_saw_overflows_above_rate_42_instead_of_clamping() {
        // Six adds of 42 reach 252, the highest clean peak.
        assert_eq!(*saw_at(42, 14).iter().max().unwrap(), 31);
        // 43 wraps the 8-bit accumulator, so the staircase falls partway
        // through the cycle. That buzz is a technique, not a bug.
        let out = saw_at(43, 14);
        assert!(
            out.windows(2).any(|w| w[1] < w[0]),
            "the accumulator must wrap, got {:?}",
            out
        );
    }

    #[test]
    fn pulse_duties_are_sixteenths_never_more_than_half() {
        // Duty D is high for D+1 of the 16 steps, so the widest is 8/16.
        for duty in 0..8u8 {
            let mut v = Vrc6::new();
            v.write(0x9000, 0x0F | (duty << 4));
            v.write(0x9001, 0);
            v.write(0x9002, 0x80);
            let high = (0..16).filter(|_| { v.clock(); v.levels()[0] > 0 }).count();
            assert_eq!(high, duty as usize + 1, "duty {}", duty);
        }
    }

    #[test]
    fn the_mode_bit_ignores_the_duty_generator() {
        let mut v = Vrc6::new();
        v.write(0x9000, 0x80 | 0x0F); // mode set, duty 0, volume 15
        v.write(0x9001, 0);
        v.write(0x9002, 0x80);
        for _ in 0..16 {
            v.clock();
            assert_eq!(v.levels()[0], 15, "mode holds the output high");
        }
    }

    #[test]
    fn a_pulse_period_matches_the_2a03_formula() {
        // f = CPU / (16 * (t + 1)) — the same as a 2A03 pulse, which is why
        // VP1/VP2 need no period table of their own. Count cycles between
        // successive wraps of the 16-step duty generator.
        let t = 7u16;
        let mut v = Vrc6::new();
        v.write(0x9000, 0x7F); // duty 7, volume 15
        v.write(0x9001, (t & 0xFF) as u8);
        v.write(0x9002, 0x80 | ((t >> 8) as u8));
        let mut edges = Vec::new();
        let mut prev = v.levels()[0] > 0;
        for i in 0..4000 {
            v.clock();
            let now = v.levels()[0] > 0;
            if now && !prev {
                edges.push(i);
            }
            prev = now;
        }
        let period: Vec<usize> = edges.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(period.iter().all(|&p| p == 16 * (t as usize + 1)), "{:?}", period);
    }

    #[test]
    fn the_saw_period_is_fourteen_not_sixteen() {
        let t = 3u16;
        let mut v = Vrc6::new();
        v.write(0xB000, 8);
        v.write(0xB001, (t & 0xFF) as u8);
        v.write(0xB002, 0x80 | ((t >> 8) as u8));
        // The accumulator resets once per 14 divider clocks.
        let mut resets = Vec::new();
        let mut prev = v.levels()[2];
        for i in 0..2000 {
            v.clock();
            let now = v.levels()[2];
            if now == 0 && prev != 0 {
                resets.push(i);
            }
            prev = now;
        }
        let gaps: Vec<usize> = resets.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps.iter().all(|&g| g == 14 * (t as usize + 1)), "{:?}", gaps);
    }

    #[test]
    fn halt_freezes_everything_and_shift_divides_the_period() {
        let mut v = Vrc6::new();
        v.write(0xB000, 8);
        v.write(0xB001, 0);
        v.write(0xB002, 0x80);
        for _ in 0..6 { v.clock(); }
        let frozen = v.levels()[2];
        v.write(0x9003, 0x01); // halt
        for _ in 0..100 { v.clock(); }
        assert_eq!(v.levels()[2], frozen, "halt stops the oscillators where they are");

        // Halt overrides the shift bits, so set shift alone to see it.
        let mut fast = Vrc6::new();
        fast.write(0x9003, 0x04); // 256x: shift by 8
        fast.write(0xB000, 8);
        fast.write(0xB001, 0xFF);
        fast.write(0xB002, 0x80 | 0x0F); // period $FFF
        let mut plain = Vrc6::new();
        plain.write(0xB000, 8);
        plain.write(0xB001, 0xFF);
        plain.write(0xB002, 0x80 | 0x0F);
        for _ in 0..64 { fast.clock(); plain.clock(); }
        assert!(fast.levels()[2] > plain.levels()[2], "a shifted period runs faster");
    }

    #[test]
    fn disabling_a_channel_silences_it() {
        let mut v = Vrc6::new();
        v.write(0x9000, 0x7F);
        v.write(0x9001, 0);
        v.write(0x9002, 0x80);
        for _ in 0..8 { v.clock(); }
        v.write(0x9002, 0x00); // clear enable
        v.clock();
        assert_eq!(v.levels()[0], 0);

        v.write(0xB000, 8);
        v.write(0xB002, 0x80);
        for _ in 0..8 { v.clock(); }
        v.write(0xB002, 0x00);
        assert_eq!(v.levels()[2], 0, "clearing E zeroes the accumulator");
    }

    #[test]
    fn the_mix_is_calibrated_against_a_full_scale_2a03_pulse() {
        // The identity behind VRC6_UNIT, asserted so nobody can quietly retune
        // the constant: one VRC6 pulse at full volume equals one 2A03 pulse at
        // full volume, which is pulse_table[15].
        let mut v = Vrc6::new();
        v.write(0x9000, 0x80 | 0x0F); // mode on, volume 15 — constant output
        v.write(0x9001, 0);
        v.write(0x9002, 0x80);
        let out = v.clock();
        let apu_full = 95.52 / (8128.0 / 15.0 + 100.0);
        assert!((out - apu_full).abs() < 1e-7, "{} vs {}", out, apu_full);
    }

    #[test]
    fn the_stem_mask_silences_one_channel_at_a_time() {
        let mut v = Vrc6::new();
        v.write(0x9000, 0x80 | 0x0F);
        v.write(0x9002, 0x80);
        v.write(0xA000, 0x80 | 0x0F);
        v.write(0xA002, 0x80);
        let both = v.clock();
        v.mask = CH_VP1;
        let one = v.clock();
        assert!((both - 2.0 * one).abs() < 1e-6, "{} vs {}", both, one);
        v.mask = 0;
        assert_eq!(v.clock(), 0.0);
    }

    #[test]
    fn the_ten_registers_are_an_exact_set_not_a_range() {
        for a in [0x9000, 0x9003, 0xA000, 0xA002, 0xB000, 0xB002] {
            assert!(is_vrc6_reg(a), "${:04X}", a);
        }
        // Everything else in the window is ROM and must stay ROM.
        for a in [0x8FFF, 0x9004, 0x9FFF, 0xA003, 0xAFFF, 0xB003, 0xC000] {
            assert!(!is_vrc6_reg(a), "${:04X}", a);
        }
    }
}
