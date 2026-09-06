//! viper-apu — a small, deterministic 2A03 host for NSF files.
//!
//! * [`cpu`]  — 6502 core
//! * [`apu`]  — 2A03 APU with mixer, per-channel mute mask, DPCM filter
//! * [`nsf`]  — NSF container + memory map with bankswitching
//! * [`host`] — `Player`: INIT/PLAY at frame rate, register-write log, audio
//! * [`render`] — offline rendering: full mix, per-channel stems, triggers
//! * [`wav`], [`midi`] — tiny writers, no dependencies
//! * [`verify`] — diff a register-write log against another emulator's
//! * [`vrc6`] — the VRC6 expansion chip, summed externally to the 2A03

pub mod apu;
pub mod cpu;
pub mod host;
pub mod midi;
pub mod nsf;
pub mod render;
pub mod trace;
pub mod verify;
pub mod vrc6;
pub mod wav;

pub use host::{Player, RegWrite, Trigger, TriggerKind};
pub use nsf::Nsf;
pub use render::{render, RenderOptions, RenderResult, Stem};
pub use trace::{trace, trace_with_levels, ChannelFrame, FrameTrace};
