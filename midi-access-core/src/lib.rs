#![forbid(unsafe_code)]

//! Shared foundation for a family of MIDI-device *access* crates.
//!
//! Each device crate (ck / minilogue / re202 / ml10x …) models its own SysEx
//! codec and parameter set, then implements one trait — [`Device`] — to plug
//! into the generic CLI engine (`midi-access-cli`) and the editor tooling. In
//! return it gets, for free and identically across devices:
//!
//! - a uniform **parameter-metadata** model ([`meta`]) — label / group / help /
//!   level / kind / catalog-hint, keyed by a serde path;
//! - a uniform **catalog** ([`catalog`]) — name↔number lookup tables behind the
//!   [`Catalogs`] trait, bundled with params + defaults into one JSON object
//!   ([`Bundle`]);
//! - generic **name resolution** ([`resolve`]) — turn editor-/LLM-authored value
//!   *names* ("Hall Reverb", "2.0 kHz") into the numeric indices the codec wants;
//! - a **deep-merge** + per-slot padding helper ([`merge`]) for partial presets;
//! - shared **codec** primitives ([`codec`]) — centred-signed, 14-bit split,
//!   nibble-packed, ASCII, checksums, reserved-byte capture, SysEx framing;
//! - one-line **JSON Schema** emission ([`schema`], behind the `schema` feature).
//!
//! The document lingua franca between a device and the engine is
//! [`serde_yaml::Value`]: the device decodes a dump to a `Value` and encodes a
//! `Value` back to wire bytes; the engine never needs to know the typed model.
//!
//! Pure: no MIDI, no file I/O — compiles for `wasm32-unknown-unknown`.

pub mod catalog;
pub mod codec;
pub mod device;
pub mod merge;
pub mod meta;
pub mod resolve;
#[cfg(feature = "schema")]
pub mod schema;

pub use catalog::{Bundle, Catalogs};
pub use codec::{split_sysex, CodecError, RawByte};
pub use device::{Area, Device, DeviceError, Inbound};
pub use meta::{choice, Choice, Kind, Level, ParamMeta, Params};
pub use resolve::{label_names, label_names_str, resolve_names, resolve_names_str};
