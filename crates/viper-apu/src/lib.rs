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

// Three lints turned off deliberately, with reasons, rather than worked
// around. `type_complexity` fires on the channel-state tuples this codebase
// passes around — `Vec<(usize, usize, NoteEnv)>` and friends — and naming a
// dozen aliases for them would put a layer of indirection between a call
// site and what it actually carries. `too_many_arguments` fires on the
// renderers, which take a frame, an area and the several pieces of state
// they draw; bundling those into a struct purely to get under a threshold
// makes them harder to read, not easier. And `needless_range_loop` is
// almost always wrong here: a loop index in this codebase is a channel, a
// step or a row — a value with a meaning that gets compared, stored and
// printed — so `for c in 0..CHANNELS` says what it means and
// `for (c, x) in xs.iter().enumerate()` says less.
#![allow(clippy::type_complexity, clippy::too_many_arguments, clippy::needless_range_loop)]

pub mod apu;
pub mod cpu;
pub mod host;
pub mod midi;
pub mod nes;
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
