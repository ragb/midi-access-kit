//! Editor-facing parameter metadata, unified across devices.
//!
//! Generalizes ck's `ParamMeta` and minilogue's `Kind`/`Choice` into one model.
//! Every editable field is described by a [`ParamMeta`] keyed by a fully-qualified
//! serde path (`system.common.master_tune`, `live_set.part.filter_cutoff`, …)
//! that matches the device's typed structs. The editor's `?` help buttons and a
//! device's value pickers read this at runtime, so the help/labels live next to
//! the codec instead of in a hand-written front-end table.
//!
//! All types are `Serialize`-only (this is reference data, never deserialized).

use serde::{Serialize, Serializer};

/// One option of a [`Kind::Choice`] field: the serialized `value` token and its
/// human `label`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Choice {
    pub value: &'static str,
    pub label: &'static str,
}

/// Const constructor for a [`Choice`].
pub const fn choice(value: &'static str, label: &'static str) -> Choice {
    Choice { value, label }
}

/// The shape of a field's value — what UI control and validation it implies.
///
/// Serialized with serde's default (external) tagging, e.g.
/// `{"range": {"min": 0, "max": 127}}`, `{"choice": [{value,label}, …]}`,
/// `"toggle"`, `"text"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A numeric value, inclusive `min..=max`, with an optional engineering unit.
    Range {
        min: i64,
        max: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        unit: Option<&'static str>,
    },
    /// A discrete choice from a fixed list of options.
    Choice(&'static [Choice]),
    /// An on/off boolean.
    Toggle,
    /// Free text (names, file paths).
    Text,
}

/// Whether a field is a `0..N` *magnitude* the editor may show as a percentage
/// (cutoff, depth, volume…), as opposed to a signed/centred/enum/index/bool.
///
/// Serializes as a bare boolean (`true` for [`Level::Magnitude`]) so a device's
/// catalog JSON matches the historical `level: true` shape; [`Level::Plain`] is
/// omitted by [`ParamMeta`]'s `skip_serializing_if`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    /// Signed, centred, enum, index, or boolean — not a magnitude.
    #[default]
    Plain,
    /// A `0..N` magnitude that reads naturally as "x% of full".
    Magnitude,
}

impl Level {
    /// True for [`Level::Plain`] — the serde `skip_serializing_if` predicate.
    pub fn is_plain(&self) -> bool {
        matches!(self, Level::Plain)
    }
}

impl Serialize for Level {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(matches!(self, Level::Magnitude))
    }
}

/// Metadata for one editable field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ParamMeta {
    /// Fully-qualified serde path, e.g. `"live_set.part.filter_cutoff"`.
    pub path: &'static str,
    /// Accessible label, e.g. `"Cutoff"`.
    pub label: &'static str,
    /// Display group / fieldset legend, e.g. `"Filter and EG"`.
    pub group: &'static str,
    /// Tooltip / screen-reader body.
    pub help: &'static str,
    /// Magnitude flag (see [`Level`]); omitted from output when [`Level::Plain`].
    #[serde(skip_serializing_if = "Level::is_plain")]
    pub level: Level,
    /// Optional value-shape hint for the editor; omitted when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    /// Which lookup catalog this field's raw value maps to, if any (drives
    /// name↔number resolution; see [`crate::resolve`]). Omitted when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog: Option<&'static str>,
}

/// A device's full parameter table, in display order. A thin `&'static` wrapper
/// with lookup helpers; serializes transparently as the underlying array.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Params(pub &'static [ParamMeta]);

impl Params {
    /// Full metadata for a field path, or `None` if absent.
    pub fn get(&self, path: &str) -> Option<&'static ParamMeta> {
        self.0.iter().find(|m| m.path == path)
    }

    /// Help text for a field path, or `None` if absent.
    pub fn help(&self, path: &str) -> Option<&'static str> {
        self.get(path).map(|m| m.help)
    }

    /// All fields in a display group, in catalog order.
    pub fn in_group<'a>(&self, group: &'a str) -> impl Iterator<Item = &'static ParamMeta> + 'a {
        let slice = self.0;
        slice.iter().filter(move |m| m.group == group)
    }

    /// The underlying slice.
    pub fn as_slice(&self) -> &'static [ParamMeta] {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static SAMPLE: &[ParamMeta] = &[
        ParamMeta {
            path: "a.cutoff",
            label: "Cutoff",
            group: "Filter",
            help: "h",
            level: Level::Magnitude,
            kind: Some(Kind::Range {
                min: 0,
                max: 127,
                unit: None,
            }),
            catalog: None,
        },
        ParamMeta {
            path: "a.kind",
            label: "Kind",
            group: "Filter",
            help: "h",
            level: Level::Plain,
            kind: None,
            catalog: Some("kinds"),
        },
    ];

    #[test]
    fn lookup_and_group() {
        let p = Params(SAMPLE);
        assert_eq!(p.get("a.cutoff").unwrap().label, "Cutoff");
        assert!(p.get("nope").is_none());
        assert_eq!(p.in_group("Filter").count(), 2);
        assert_eq!(p.help("a.kind"), Some("h"));
    }

    #[test]
    fn level_serializes_as_bool_and_skips_plain() {
        let y = serde_yaml::to_string(&Params(SAMPLE)).unwrap();
        // Magnitude → `level: true`; Plain → omitted entirely.
        assert!(y.contains("level: true"));
        assert_eq!(y.matches("level:").count(), 1);
        // catalog hint present only on the second entry.
        assert!(y.contains("catalog: kinds"));
    }

    #[test]
    fn kind_is_externally_tagged() {
        // serde_yaml renders external tags as `!variant`; serde_json (used by the
        // `catalog` command) renders them as `{"variant": …}`. Either way the
        // variant name and inner fields are present.
        static OPTS: &[Choice] = &[choice("a", "A")];
        let y = serde_yaml::to_string(&Kind::Choice(OPTS)).unwrap();
        assert!(y.contains("choice"));
        assert!(y.contains("value: a"));
        let r = serde_yaml::to_string(&Kind::Range {
            min: 0,
            max: 9,
            unit: Some("dB"),
        })
        .unwrap();
        assert!(r.contains("range"));
        assert!(r.contains("unit: dB"));
    }
}
