//! The [`Device`] trait — the contract a device crate implements to drive the
//! generic CLI engine and editor tooling.
//!
//! The document lingua franca is [`serde_yaml::Value`]: the device decodes a raw
//! dump into a `Value` and encodes a `Value` back to wire bytes, so the engine
//! never touches the typed model. Areas (`"system"`, `"live-set"`, …) name the
//! editable documents a device dumps/syncs as one YAML file each.
//!
//! ## Multi-frame dumps
//!
//! [`request`](Device::request) and [`encode`](Device::encode) return *all* the
//! SysEx bytes for an operation — possibly several `F0…F7` frames concatenated
//! (e.g. a bulk header + content blocks + footer). The engine sends each frame
//! and, for a dump, collects every inbound frame until the device goes idle, then
//! hands the whole collected buffer to [`decode`](Device::decode), which splits
//! and reassembles it. This keeps single- and multi-block devices uniform.

use serde_yaml::Value;
use thiserror::Error;

use crate::catalog::Catalogs;
use crate::meta::Params;

/// One editable document a device dumps/syncs (e.g. the global System or a
/// Live Set patch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Area {
    /// CLI token, kebab-case, e.g. `"live-set"`.
    pub name: &'static str,
    /// Human label, e.g. `"Live Set"` (shown by `show`/`lint`).
    pub label: &'static str,
    /// One-line description for help text.
    pub about: &'static str,
}

impl Area {
    /// Case-insensitive match, treating `-` and `_` as equivalent so
    /// `live-set`, `live_set`, and `liveset` all hit a `"live-set"` area.
    pub fn matches(&self, name: &str) -> bool {
        fn norm(s: &str) -> String {
            s.chars()
                .filter(|c| *c != '-' && *c != '_')
                .flat_map(char::to_lowercase)
                .collect()
        }
        norm(self.name) == norm(name)
    }
}

/// A classified inbound MIDI message, as far as the engine needs to act on it
/// (identify the device, recognise dumps). Devices map their richer internal
/// classification onto this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// A block/bulk dump, with the area it targets resolved if known.
    Dump {
        area: Option<String>,
        address: Vec<u8>,
        data: Vec<u8>,
    },
    /// A single-parameter change.
    Parameter { address: Vec<u8>, data: Vec<u8> },
    /// A request the device echoed back.
    Request { address: Vec<u8> },
    /// A Universal Identity Reply, with the model name if recognised.
    Identity {
        bytes: Vec<u8>,
        model: Option<String>,
    },
    /// Anything else (channel messages, undecodable SysEx, …).
    Other(Vec<u8>),
}

/// Errors a [`Device`] operation can return.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum DeviceError {
    #[error("unknown area: {0:?}")]
    UnknownArea(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("encode: {0}")]
    Encode(String),
    #[error("{0}")]
    Other(String),
}

/// The contract one device crate implements to plug into the kit.
pub trait Device {
    /// Short device name, used for the binary name, schema filenames, and the
    /// catalog bundle's `device` field (e.g. `"ck"`).
    const NAME: &'static str;

    /// The editable documents this device dumps/syncs.
    fn areas() -> &'static [Area];

    /// The parameter-metadata table.
    fn params() -> Params;

    /// The value catalogs (name↔number lookup + data).
    fn catalogs() -> &'static dyn Catalogs;

    /// The factory-default document for an area, as a `Value` (or `None`).
    fn defaults(area: &str) -> Option<Value>;

    /// The JSON Schema for an area's document, as a string (or `None`).
    fn schema(area: &str) -> Option<String>;

    /// The SysEx bytes to send to request a dump of `area` from device `ch`.
    fn request(area: &str, ch: u8) -> Result<Vec<u8>, DeviceError>;

    /// Decode a collected dump (one or more concatenated SysEx frames) of `area`
    /// into a document `Value`.
    fn decode(area: &str, dump: &[u8]) -> Result<Value, DeviceError>;

    /// Encode a document `Value` for `area` into the SysEx bytes that write it to
    /// device `ch` (header + blocks + footer as needed).
    fn encode(area: &str, doc: &Value, ch: u8) -> Result<Vec<u8>, DeviceError>;

    /// Classify an inbound MIDI byte sequence.
    fn classify_inbound(bytes: &[u8]) -> Inbound;

    /// Whether `doc` is a well-formed document for `area` — a *parse-level* check
    /// used by `show`/`lint`/`diff` to identify a file's kind, distinct from a
    /// full [`encode`](Device::encode) (which also validates byte ranges).
    ///
    /// The default delegates to `encode`; override for a looser, deserialize-only
    /// check (a file may parse into the typed model yet fail to encode).
    fn accepts(area: &str, doc: &Value) -> bool {
        Self::encode(area, doc, 0).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn area_matching_is_lenient() {
        let a = Area {
            name: "live-set",
            label: "Live Set",
            about: "",
        };
        assert!(a.matches("live-set"));
        assert!(a.matches("live_set"));
        assert!(a.matches("LiveSet"));
        assert!(!a.matches("system"));
    }
}
