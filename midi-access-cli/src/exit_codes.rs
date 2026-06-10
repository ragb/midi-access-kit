//! Process exit codes, promoted from the ml10x CLI as the kit standard.
//!
//! Commands return an `i32`; `main` casts it to a [`std::process::ExitCode`].

/// Success.
pub const OK: i32 = 0;
/// Catch-all failure.
pub const GENERIC_ERROR: i32 = 1;
/// Bad arguments / usage (unknown area, out-of-range selector).
pub const USAGE_ERROR: i32 = 2;
/// Could not read/parse/validate an input file.
pub const INPUT_FILE_ERROR: i32 = 3;
/// No device / MIDI port available, or no response.
pub const DEVICE_UNAVAILABLE: i32 = 4;
/// Failed to encode a document to wire bytes.
pub const ENCODE_ERROR: i32 = 5;
/// A write was sent but the device never acknowledged it.
pub const SYNC_NO_ACK: i32 = 6;
/// The device actively rejected a write (e.g. verify mismatch).
pub const DEVICE_REJECTED: i32 = 7;
