//! Generic CLI engine for MIDI device access crates.
//!
//! Implement [`midi_access_core::Device`] for your device, then your `main` is:
//!
//! ```ignore
//! fn main() -> std::process::ExitCode { midi_access_cli::run::<MyDevice>() }
//! ```
//!
//! [`run`] builds a clap command with the standard subcommands — `dump`, `sync`,
//! `show`, `lint`, `diff`, `schema <area>`, `catalog`, `resolve`, `identity`,
//! `ports` — and dispatches them through your `Device`. Status output goes through
//! [`output::Out`]; exit codes are in [`exit_codes`]; MIDI plumbing is in [`midi`].

mod engine;
pub mod exit_codes;
pub mod midi;
pub mod output;
pub mod yaml_io;

pub use engine::run;
