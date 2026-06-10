//! Value catalogs (name↔number lookup) and the combined metadata [`Bundle`].
//!
//! A device exposes its lookup tables — voices, effect algorithms, EQ
//! frequencies, controller targets, … — through the [`Catalogs`] trait. Each
//! table has a name (`"voices"`, `"eq_freq"`, …) that a [`ParamMeta`]'s
//! `catalog` hint refers to, wiring the tables to name resolution.
//!
//! [`Bundle`] is the one JSON object a tool or LLM needs to author a preset *by
//! name* over a sensible baseline: the params, every catalog's data, and the
//! per-area factory defaults.
//!
//! [`ParamMeta`]: crate::meta::ParamMeta

use serde::Serialize;
use serde_yaml::Value;

use crate::meta::Params;

/// A device's value catalogs: name↔number lookup plus full data for the bundle.
pub trait Catalogs {
    /// Resolve a value *name* within catalog `cat` to its numeric value
    /// (case-insensitive, by convention). `None` if `cat` or `name` is unknown.
    fn resolve(&self, cat: &str, name: &str) -> Option<i64>;

    /// Label a numeric `value` within catalog `cat` (the inverse of
    /// [`resolve`](Catalogs::resolve)). `None` if out of range / unknown.
    fn label(&self, cat: &str, value: i64) -> Option<String>;

    /// The names of every catalog this device exposes.
    fn names(&self) -> &[&str];

    /// All catalog data as one serializable value (the bundle's `catalogs`
    /// field). Conventionally a mapping of catalog-name → entries.
    fn as_value(&self) -> Value;
}

/// The combined metadata bundle — one device's complete reference data.
///
/// Built by the CLI's `catalog` subcommand from a [`Device`](crate::Device):
/// `device` = its name, `params` = its [`Params`], `catalogs` = its
/// [`Catalogs::as_value`], `defaults` = a mapping of area-name → factory default
/// document.
#[derive(Debug, Clone, Serialize)]
pub struct Bundle {
    pub device: &'static str,
    pub params: Params,
    pub catalogs: Value,
    pub defaults: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::ParamMeta;

    struct Fake;
    impl Catalogs for Fake {
        fn resolve(&self, cat: &str, name: &str) -> Option<i64> {
            (cat == "n" && name.eq_ignore_ascii_case("one")).then_some(1)
        }
        fn label(&self, cat: &str, value: i64) -> Option<String> {
            (cat == "n" && value == 1).then(|| "one".to_string())
        }
        fn names(&self) -> &[&str] {
            &["n"]
        }
        fn as_value(&self) -> Value {
            serde_yaml::from_str("n:\n- one").unwrap()
        }
    }

    static P: &[ParamMeta] = &[];

    #[test]
    fn bundle_serializes() {
        let f = Fake;
        let b = Bundle {
            device: "fake",
            params: Params(P),
            catalogs: f.as_value(),
            defaults: serde_yaml::from_str("area: {}").unwrap(),
        };
        let y = serde_yaml::to_string(&b).unwrap();
        assert!(y.contains("device: fake"));
        assert!(y.contains("catalogs:"));
        assert_eq!(f.resolve("n", "ONE"), Some(1));
        assert_eq!(f.label("n", 1).as_deref(), Some("one"));
    }
}
