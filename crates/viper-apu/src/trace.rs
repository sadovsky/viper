//! The frame table: what each channel was *doing*, once per frame.
//!
//! A register log says `4000 79`. Music says "pulse 1, volume 9, 25% duty".
//! This module is the step between them, and everything that reads a song
//! back out of an NSF reads this rather than raw registers — so the register
//! semantics live in exactly one place.
//!
//! Two things make this more than a `match` on the address.
//!
//! **Volume is not the low nibble.** `$4000` bit 4 is the constant-volume
//! flag. When it is clear, the low nibble is the hardware envelope's *divider
//! reload*, not a level, and the audible volume is a decay counter the chip
//! walks down on its own. viper's own driver sets that bit on every write, so
//! reading the nibble as a volume passes every test in this repo and then
//! produces nonsense on the first commercial NSF. When a live `Apu` is
//! available its `levels()` already resolves this exactly; from a bare log we
//! simulate the envelope and say so.
//!
//! **A key-on is not a note.** A write to `$4003`/`$4007`/`$400B`/`$400F`
//! reloads the length counter, which is the closest thing the chip has to a
//! note-on. But a driver may key a channel whose volume is zero — viper's does
//! — and reading those as notes invents music that was never audible. The
//! `keyed` flag here is the raw event; deciding which ones are notes is the
//! caller's job, with `level` in hand to do it.

use crate::host::RegWrite;

/// Channel order, matching the tracker's columns: PU1, PU2, TRI, NOI, DPCM.
pub const CHANNELS: usize = 5;
pub const PU1: usize = 0;
pub const PU2: usize = 1;
pub const TRI: usize = 2;
pub const NOI: usize = 3;
pub const DMC: usize = 4;

/// Length counter reloads, per channel. These are the note-on registers.
const KEY_ON: [u16; 4] = [0x4003, 0x4007, 0x400B, 0x400F];

const LENGTH_TABLE: [u8; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 14, 12, 26, 14,
    12, 16, 24, 18, 48, 20, 96, 22, 192, 24, 72, 26, 16, 28, 32, 30,
];

/// One channel at one frame boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelFrame {
    /// 11-bit timer for the pulses and triangle; the 4-bit `$400E` index for
    /// noise, which is already the value a tracker wants. Meaningless on DMC.
    pub period: u16,
    /// Audible level, 0..=15. Triangle is 0 or 15 because the hardware has no
    /// volume control for it; DMC is 0 or 15 for the frame a sample starts.
    pub level: u8,
    /// `$4000`/`$4004` bits 6-7. Noise reuses it for the mode bit.
    pub duty: u8,
    /// The `$4015` enable bit.
    pub enabled: bool,
    /// A length-counter reload landed this frame.
    pub keyed: bool,
    /// False means the level was produced by the hardware envelope, so from a
    /// bare log it is simulated rather than read.
    pub constant_vol: bool,
    /// An *active* `$4001`/`$4005` sweep, which viper has no way to express.
    /// Recorded so a caller can report it instead of silently dropping it.
    ///
    /// Only a sweep that would actually move the pitch counts: bit 7 set and
    /// a non-zero shift. Drivers routinely write `$08` here to park the unit
    /// — viper's own does, at INIT — and calling that an unsupported feature
    /// would cry wolf on almost every NSF in existence.
    pub sweep: u8,
}

/// Every channel at one frame boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameTrace {
    pub frame: u32,
    pub ch: [ChannelFrame; CHANNELS],
    /// The `$4012` value of a DPCM sample that started this frame.
    pub dpcm: Option<u8>,
}

/// A hardware envelope generator, as the 2A03 runs it.
#[derive(Clone, Copy, Default)]
struct Envelope {
    start: bool,
    loop_: bool,
    constant: bool,
    volume: u8,
    divider: u8,
    decay: u8,
}

impl Envelope {
    /// One quarter-frame clock. Mirrors `apu::Envelope::clock` — kept
    /// separate rather than shared because this one runs on log timing.
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

#[derive(Clone, Copy, Default)]
struct Chan {
    timer: u16,
    duty: u8,
    halt: bool,
    enabled: bool,
    length: u8,
    sweep: u8,
    env: Envelope,
}

/// Build a frame table from a register log alone.
///
/// The envelope and length counters are simulated at their real rates — four
/// quarter-frame clocks and two half-frame clocks per frame — which is exact
/// in the 4-step mode every music driver uses. Where a live `Apu` is on hand,
/// prefer [`trace_with_levels`]: it takes the chip's own answer instead.
pub fn trace(log: &[RegWrite]) -> Vec<FrameTrace> {
    trace_inner(log, None)
}

/// Build a frame table, taking each frame's audible levels and periods from
/// the emulator rather than simulating them.
///
/// `levels[f]` and `periods[f]` are `Apu::levels()` and `Apu::periods()`
/// sampled at the end of frame `f`. Registers still supply duty, the key-on
/// flags and the noise index, which the accessors do not expose.
pub fn trace_with_levels(log: &[RegWrite], levels: &[[u8; 5]], periods: &[[u16; 4]]) -> Vec<FrameTrace> {
    trace_inner(log, Some((levels, periods)))
}

fn trace_inner(log: &[RegWrite], sampled: Option<(&[[u8; 5]], &[[u16; 4]])>) -> Vec<FrameTrace> {
    let Some(last) = log.iter().map(|w| w.frame).max() else { return Vec::new() };
    let mut ch = [Chan::default(); 4];
    let mut tri_linear_reload = 0u8;
    let mut dmc_addr_reg = 0u8;
    let mut dmc_playing = false;
    let mut out: Vec<FrameTrace> = Vec::with_capacity(last as usize + 1);
    let mut i = 0usize;

    for frame in 0..=last {
        let mut keyed = [false; 4];
        let mut dpcm = None;
        // Every write stamped with this frame, in order.
        while i < log.len() && log[i].frame == frame {
            let (a, v) = (log[i].addr, log[i].value);
            i += 1;
            match a {
                0x4000 | 0x4004 | 0x400C => {
                    let c = &mut ch[match a { 0x4000 => PU1, 0x4004 => PU2, _ => NOI }];
                    // Noise has no duty; bit 7 of $400E is its mode bit, and
                    // $400C's top two bits are unused, so this is harmless.
                    c.duty = v >> 6;
                    c.halt = v & 0x20 != 0;
                    c.env.loop_ = v & 0x20 != 0;
                    c.env.constant = v & 0x10 != 0;
                    c.env.volume = v & 0x0F;
                }
                0x4001 | 0x4005 => {
                    let c = &mut ch[if a == 0x4001 { PU1 } else { PU2 }];
                    c.sweep = if v & 0x80 != 0 && v & 7 != 0 { v } else { 0 };
                }
                0x4002 | 0x4006 | 0x400A => {
                    let c = &mut ch[match a { 0x4002 => PU1, 0x4006 => PU2, _ => TRI }];
                    c.timer = (c.timer & 0x700) | v as u16;
                }
                0x4008 => tri_linear_reload = v & 0x7F,
                0x400E => {
                    // The 4-bit index is what a tracker wants, so it is kept
                    // as-is rather than expanded through NOISE_PERIOD and
                    // inverted again later.
                    ch[NOI].timer = (v & 0x0F) as u16;
                    ch[NOI].duty = v >> 7;
                }
                0x4003 | 0x4007 | 0x400B | 0x400F => {
                    let idx = KEY_ON.iter().position(|&k| k == a).unwrap();
                    let c = &mut ch[idx];
                    if a != 0x400F {
                        c.timer = (c.timer & 0xFF) | ((v as u16 & 7) << 8);
                    }
                    if c.enabled {
                        c.length = LENGTH_TABLE[(v >> 3) as usize];
                    }
                    c.env.start = true;
                    keyed[idx] = true;
                }
                0x4012 => dmc_addr_reg = v,
                0x4015 => {
                    for (b, c) in ch.iter_mut().enumerate() {
                        c.enabled = v & (1 << b) != 0;
                        if !c.enabled {
                            c.length = 0;
                        }
                    }
                    if v & 0x10 != 0 {
                        if !dmc_playing {
                            dmc_playing = true;
                            dpcm = Some(dmc_addr_reg);
                        }
                    } else {
                        dmc_playing = false;
                    }
                }
                _ => {}
            }
        }

        // Advance the frame sequencer past this frame: four quarter clocks
        // drive the envelopes, two half clocks the length counters.
        for q in 0..4 {
            for c in ch.iter_mut() {
                c.env.clock();
                if q % 2 == 1 && !c.halt && c.length > 0 {
                    c.length -= 1;
                }
            }
        }

        let mut ft = FrameTrace { frame, ch: [ChannelFrame::default(); CHANNELS], dpcm };
        for k in 0..4 {
            let c = &ch[k];
            let audible = c.enabled && c.length > 0;
            let level = match k {
                // The triangle has no volume: it is on when its linear
                // counter was reloaded non-zero, and off otherwise. That is
                // also exactly how a driver uses $4008.
                TRI => if audible && tri_linear_reload > 0 { 15 } else { 0 },
                _ => if audible { c.env.output() } else { 0 },
            };
            ft.ch[k] = ChannelFrame {
                period: c.timer,
                level,
                duty: c.duty,
                enabled: c.enabled,
                keyed: keyed[k],
                constant_vol: k == TRI || c.env.constant,
                sweep: c.sweep,
            };
        }
        ft.ch[DMC] = ChannelFrame {
            level: if dpcm.is_some() { 15 } else { 0 },
            enabled: dmc_playing,
            keyed: dpcm.is_some(),
            constant_vol: true,
            ..ChannelFrame::default()
        };

        // The chip's own answer wins where we have it: it resolves hardware
        // envelopes and sweep muting exactly, which a log cannot.
        if let Some((levels, periods)) = sampled {
            let f = frame as usize;
            if let Some(l) = levels.get(f) {
                for k in 0..CHANNELS {
                    ft.ch[k].level = l[k];
                }
                // DMC's own gate is more informative than our one-frame pulse.
                ft.ch[DMC].keyed = dpcm.is_some();
            }
            if let Some(p) = periods.get(f) {
                for k in [PU1, PU2, TRI] {
                    ft.ch[k].period = p[k];
                }
            }
        }
        out.push(ft);
    }
    out
}

/// Run an NSF and trace it, taking every level and period from the chip
/// rather than simulating them.
///
/// `frames` defaults to one pass through the detected loop plus its intro.
/// A song whose driver state never repeats has no detectable length, so the
/// caller must say how long to run; that is reported rather than guessed.
pub fn trace_nsf(nsf: &crate::nsf::Nsf, song: u8, frames: Option<u32>) -> anyhow::Result<(Vec<FrameTrace>, Option<(u32, u32)>)> {
    let looped = crate::render::find_loop(nsf, song, 300.0)?;
    let n = match (frames, looped) {
        (Some(n), _) => n,
        // `start + len` covers the intro plus one whole loop whatever the
        // start really is. RAM hashing finds the first frame whose driver
        // state repeats, which can sit later than the musical loop point, so
        // this can include a whole extra pass. That is the safe direction to
        // be wrong in: transcribing a repeat costs nothing once identical
        // phrases collapse, whereas stopping short loses music.
        (None, Some((start, len))) => start + len,
        (None, None) => anyhow::bail!(
            "no loop found: this song's driver state never repeats within 300s, \
             so its length cannot be detected — pass a frame count"
        ),
    };
    // Audio is not wanted here, but the APU still has to run: the levels
    // being sampled are its state, and DPCM fetches read through memory.
    let mut p = crate::host::Player::new(nsf.clone(), 44_100);
    p.init(song)?;
    // `Player::frame` counts up before it calls PLAY, so INIT's writes are
    // logged as frame 0 and the first PLAY is frame 1. The snapshot vectors
    // are indexed by log frame, so INIT gets the slot it wrote in.
    let mut levels = vec![p.apu.levels()];
    let mut periods = vec![p.apu.periods()];
    for _ in 0..n {
        p.frame()?;
        levels.push(p.apu.levels());
        periods.push(p.apu.periods());
        p.samples.clear();
    }
    Ok((trace_with_levels(&p.log, &levels, &periods), looped))
}

/// Render a frame table as TSV, one row per frame per sounding channel.
/// This is what `viper rip --trace` writes: the point is that a human can
/// read it and check the decode by eye.
pub fn format_trace(t: &[FrameTrace]) -> String {
    let mut s = String::from("frame\tch\tperiod\tlevel\tduty\tkey\n");
    let names = ["PU1", "PU2", "TRI", "NOI", "DMC"];
    for f in t {
        for (k, c) in f.ch.iter().enumerate() {
            if c.level == 0 && !c.keyed {
                continue;
            }
            s.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                f.frame, names[k], c.period, c.level, c.duty, if c.keyed { "*" } else { "" }
            ));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a log from `(frame, addr, value)` triples.
    fn log(w: &[(u32, u16, u8)]) -> Vec<RegWrite> {
        w.iter().map(|&(frame, addr, value)| RegWrite { frame, addr, value }).collect()
    }

    /// Enable every channel, which every driver does once at INIT.
    const ON: (u32, u16, u8) = (0, 0x4015, 0x0F);

    #[test]
    fn a_period_is_assembled_from_its_low_and_high_writes() {
        let t = trace(&log(&[ON, (0, 0x4002, 0x42), (0, 0x4003, 0x09)]));
        // High byte holds only the low 3 bits: 0x09 & 7 = 1 -> 0x142.
        assert_eq!(t[0].ch[PU1].period, 0x142);
    }

    #[test]
    fn the_length_reload_registers_are_the_key_ons() {
        let t = trace(&log(&[
            ON,
            (0, 0x4000, 0xBF), (0, 0x4003, 0x08),
            (1, 0x4002, 0x50),
            (2, 0x4007, 0x08), (2, 0x4004, 0xBF),
        ]));
        assert!(t[0].ch[PU1].keyed, "$4003 is a key-on");
        assert!(!t[1].ch[PU1].keyed, "a period write alone is not");
        assert!(t[2].ch[PU2].keyed, "$4007 keys pulse 2");
        assert!(!t[2].ch[PU1].keyed, "and not pulse 1");
    }

    #[test]
    fn a_constant_volume_channel_reports_its_nibble() {
        let t = trace(&log(&[ON, (0, 0x4000, 0xB9), (0, 0x4003, 0x08)]));
        // 0xB9: duty 2, halt/loop set, constant set, volume 9.
        assert_eq!(t[0].ch[PU1].level, 9);
        assert_eq!(t[0].ch[PU1].duty, 2);
        assert!(t[0].ch[PU1].constant_vol);
    }

    #[test]
    fn a_hardware_envelope_decays_instead_of_holding_its_nibble() {
        // 0x80: duty 2, halt and loop clear, constant clear, volume 0. The
        // nibble is a divider reload, not a level, so the decay counter steps
        // once per quarter frame and the channel walks down from 15 — where
        // reading the nibble as a volume would call it silent from the start.
        let t = trace(&log(&[ON, (0, 0x4000, 0x80), (0, 0x4003, 0x08), (5, 0x4002, 1)]));
        assert!(!t[0].ch[PU1].constant_vol, "this level was not read, it was simulated");
        assert!(t[0].ch[PU1].level > 0, "read as a plain nibble it would be silent");
        let levels: Vec<u8> = (0..4).map(|f| t[f].ch[PU1].level).collect();
        assert!(levels.windows(2).all(|w| w[1] < w[0]), "monotone decay, got {:?}", levels);
        assert_eq!(t[5].ch[PU1].level, 0, "and it stays down, because loop is clear");
    }

    #[test]
    fn a_disabled_channel_is_silent_however_loud_its_registers_look() {
        let t = trace(&log(&[
            (0, 0x4015, 0x0F), (0, 0x4000, 0xBF), (0, 0x4003, 0x08),
            (1, 0x4015, 0x0E),
        ]));
        assert_eq!(t[0].ch[PU1].level, 15);
        assert_eq!(t[1].ch[PU1].level, 0, "$4015 cleared its enable bit");
        assert!(!t[1].ch[PU1].enabled);
    }

    #[test]
    fn a_key_on_with_no_volume_is_recorded_but_makes_no_sound() {
        // viper's own driver does exactly this, and reading these as notes
        // invents music that was never audible.
        let t = trace(&log(&[ON, (0, 0x4000, 0xB0), (0, 0x4003, 0x08)]));
        assert!(t[0].ch[PU1].keyed);
        assert_eq!(t[0].ch[PU1].level, 0);
    }

    #[test]
    fn the_triangle_is_a_gate_not_a_volume() {
        let t = trace(&log(&[ON, (0, 0x4008, 0xFF), (0, 0x400B, 0x08), (1, 0x4008, 0x00), (1, 0x400B, 0x08)]));
        assert_eq!(t[0].ch[TRI].level, 15, "linear counter reloaded non-zero");
        assert_eq!(t[1].ch[TRI].level, 0, "reloaded to zero: silent");
    }

    #[test]
    fn noise_keeps_its_four_bit_index_rather_than_an_expanded_period() {
        let t = trace(&log(&[ON, (0, 0x400C, 0xB8), (0, 0x400E, 0x8A), (0, 0x400F, 0x08)]));
        assert_eq!(t[0].ch[NOI].period, 0x0A, "the $400E index, not NOISE_PERIOD[10]");
        assert_eq!(t[0].ch[NOI].duty, 1, "bit 7 is the mode bit");
        assert_eq!(t[0].ch[NOI].level, 8);
    }

    #[test]
    fn a_dpcm_start_is_reported_once_with_its_sample() {
        let t = trace(&log(&[
            (0, 0x4015, 0x0F),
            (1, 0x4012, 0x40), (1, 0x4015, 0x1F),
            (2, 0x4015, 0x1F),
            (3, 0x4015, 0x0F),
            (4, 0x4012, 0x50), (4, 0x4015, 0x1F),
        ]));
        assert_eq!(t[1].dpcm, Some(0x40));
        assert_eq!(t[2].dpcm, None, "still playing is not a new trigger");
        assert_eq!(t[4].dpcm, Some(0x50));
        assert!(t[1].ch[DMC].keyed && !t[2].ch[DMC].keyed);
    }

    #[test]
    fn an_active_sweep_is_recorded_because_viper_cannot_express_one() {
        let t = trace(&log(&[ON, (0, 0x4001, 0x8B), (0, 0x4003, 0x08)]));
        assert_eq!(t[0].ch[PU1].sweep, 0x8B);
        assert_eq!(t[0].ch[PU2].sweep, 0, "and it does not leak across channels");
    }

    #[test]
    fn parking_the_sweep_unit_is_not_an_unsupported_feature() {
        // $08 with bit 7 clear is the idiom for "no sweep, please", and
        // viper's own driver writes it to both pulses at INIT. Reporting it
        // would cry wolf on almost every NSF there is.
        let t = trace(&log(&[ON, (0, 0x4001, 0x08), (0, 0x4005, 0x08), (0, 0x4003, 0x08)]));
        assert_eq!(t[0].ch[PU1].sweep, 0);
        assert_eq!(t[0].ch[PU2].sweep, 0);
        // A shift of zero moves nothing either, even with the enable set.
        let t = trace(&log(&[ON, (0, 0x4001, 0x88), (0, 0x4003, 0x08)]));
        assert_eq!(t[0].ch[PU1].sweep, 0);
    }

    #[test]
    fn the_length_counter_runs_out_when_the_halt_bit_is_clear() {
        // 0x1F: halt clear, constant set, volume 15; length index 3 -> 2,
        // and two half-clocks land in the same frame.
        let t = trace(&log(&[ON, (0, 0x4000, 0x1F), (0, 0x4003, 0x18), (9, 0x4002, 1)]));
        assert_eq!(t[0].ch[PU1].level, 0, "the counter ran out inside frame 0");
        assert_eq!(t[9].ch[PU1].level, 0);
    }

    #[test]
    fn sampled_levels_override_the_simulation() {
        let l = log(&[ON, (0, 0x4000, 0xA0), (0, 0x4003, 0x08)]);
        let sim = trace(&l);
        let levels = vec![[3u8, 0, 0, 0, 0]];
        let periods = vec![[0x111u16, 0, 0, 0]];
        let t = trace_with_levels(&l, &levels, &periods);
        assert_ne!(sim[0].ch[PU1].level, 3);
        assert_eq!(t[0].ch[PU1].level, 3, "the chip's answer wins");
        assert_eq!(t[0].ch[PU1].period, 0x111);
        assert!(t[0].ch[PU1].keyed, "and the register-derived flags survive");
    }

    #[test]
    fn an_empty_log_traces_to_nothing() {
        assert!(trace(&[]).is_empty());
    }
}
