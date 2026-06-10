//! Generic name ↔ value resolution for catalog-backed fields.
//!
//! Editor-/LLM-authored presets are friendlier in names ("Hall Reverb", "78Rd",
//! "2.0 kHz") than raw indices. The canonical typed model stays numeric (so the
//! editor and byte codec are unaffected); this is a translation layer that turns
//! names into numbers on the way in (and back, for display).
//!
//! Which fields are catalog-backed — and which catalog — is declared by each
//! [`ParamMeta`]'s `catalog` hint, so resolution stays in sync with the field
//! metadata. The walk is structure-agnostic: it descends a [`Value`] and, at any
//! leaf whose key matches a catalog-hinted parameter, rewrites the value via the
//! device's [`Catalogs`]. Numbers and unknown names pass through unchanged.
//!
//! [`ParamMeta`]: crate::meta::ParamMeta

use serde_yaml::Value;

use crate::catalog::Catalogs;
use crate::meta::Params;

/// The last `.`-separated segment of a param path (`"live_set.part.x"` → `"x"`).
fn last_segment(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
}

/// The catalog a leaf `key` maps to, determined by matching the last segment of
/// a catalog-hinted param path. `None` if no hinted param ends with `key`, or if
/// several do with *different* catalogs (ambiguous — left untouched).
fn catalog_for_key(params: Params, key: &str) -> Option<&'static str> {
    let mut found: Option<&'static str> = None;
    for m in params.0 {
        if let Some(cat) = m.catalog {
            if last_segment(m.path) == key {
                match found {
                    None => found = Some(cat),
                    Some(prev) if prev == cat => {}
                    Some(_) => return None, // ambiguous across catalogs
                }
            }
        }
    }
    found
}

/// Replace a string (or each string in a sequence) with its resolved number.
fn resolve_leaf(v: &mut Value, cat: &str, catalogs: &dyn Catalogs) {
    match v {
        Value::String(s) => {
            if let Some(n) = catalogs.resolve(cat, s) {
                *v = Value::Number(n.into());
            }
        }
        Value::Sequence(seq) => {
            for e in seq.iter_mut() {
                resolve_leaf(e, cat, catalogs);
            }
        }
        _ => {}
    }
}

/// Replace a number (or each number in a sequence) with its catalog label.
fn label_leaf(v: &mut Value, cat: &str, catalogs: &dyn Catalogs) {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if let Some(s) = catalogs.label(cat, i) {
                    *v = Value::String(s);
                }
            }
        }
        Value::Sequence(seq) => {
            for e in seq.iter_mut() {
                label_leaf(e, cat, catalogs);
            }
        }
        _ => {}
    }
}

/// Walk `v`, applying `leaf` at every catalog-hinted key and recursing elsewhere.
fn walk(v: &mut Value, params: Params, catalogs: &dyn Catalogs, leaf: LeafFn) {
    match v {
        Value::Mapping(map) => {
            for (k, val) in map.iter_mut() {
                if let Some(cat) = k.as_str().and_then(|key| catalog_for_key(params, key)) {
                    leaf(val, cat, catalogs);
                } else {
                    walk(val, params, catalogs, leaf);
                }
            }
        }
        Value::Sequence(seq) => {
            for e in seq.iter_mut() {
                walk(e, params, catalogs, leaf);
            }
        }
        _ => {}
    }
}

type LeafFn = fn(&mut Value, &str, &dyn Catalogs);

/// Convert value *names* to numbers throughout `v`, in place.
pub fn resolve_names(v: &mut Value, params: Params, catalogs: &dyn Catalogs) {
    walk(v, params, catalogs, resolve_leaf);
}

/// Convert numeric values to their catalog *labels* throughout `v`, in place
/// (the inverse of [`resolve_names`]).
pub fn label_names(v: &mut Value, params: Params, catalogs: &dyn Catalogs) {
    walk(v, params, catalogs, label_leaf);
}

/// Parse YAML/JSON, [`resolve_names`], and re-serialize to normalized YAML ready
/// for a device's `encode`/`decode`.
pub fn resolve_names_str(
    input: &str,
    params: Params,
    catalogs: &dyn Catalogs,
) -> Result<String, serde_yaml::Error> {
    let mut v: Value = serde_yaml::from_str(input)?;
    resolve_names(&mut v, params, catalogs);
    serde_yaml::to_string(&v)
}

/// Parse YAML/JSON, [`label_names`], and re-serialize to name-bearing YAML.
pub fn label_names_str(
    input: &str,
    params: Params,
    catalogs: &dyn Catalogs,
) -> Result<String, serde_yaml::Error> {
    let mut v: Value = serde_yaml::from_str(input)?;
    label_names(&mut v, params, catalogs);
    serde_yaml::to_string(&v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{Level, ParamMeta};

    struct Cats;
    impl Catalogs for Cats {
        fn resolve(&self, cat: &str, name: &str) -> Option<i64> {
            match (cat, name.to_ascii_lowercase().as_str()) {
                ("fx", "hall reverb") => Some(27),
                ("voices", "cfx stereo") => Some(0),
                ("voices", "78rd") => Some(13),
                _ => None,
            }
        }
        fn label(&self, cat: &str, value: i64) -> Option<String> {
            match (cat, value) {
                ("fx", 27) => Some("Hall Reverb".into()),
                ("voices", 0) => Some("CFX Stereo".into()),
                ("voices", 13) => Some("78Rd".into()),
                _ => None,
            }
        }
        fn names(&self) -> &[&str] {
            &["fx", "voices"]
        }
        fn as_value(&self) -> Value {
            Value::Null
        }
    }

    const fn pc(path: &'static str, cat: &'static str) -> ParamMeta {
        ParamMeta {
            path,
            label: "",
            group: "",
            help: "",
            level: Level::Plain,
            kind: None,
            catalog: Some(cat),
        }
    }

    static PARAMS: &[ParamMeta] = &[
        pc("live_set.part.effect_1_type", "fx"),
        pc("live_set.part.category_voices", "voices"),
    ];

    #[test]
    fn resolves_names_in_nested_partial_doc() {
        let yaml = "parts:\n- effect_1_type: Hall Reverb\n  category_voices: [CFX Stereo, 78Rd]\n";
        let out = resolve_names_str(yaml, Params(PARAMS), &Cats).unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        let part = &v["parts"][0];
        assert_eq!(part["effect_1_type"].as_i64(), Some(27));
        assert_eq!(part["category_voices"][1].as_i64(), Some(13));
    }

    #[test]
    fn numbers_and_unknowns_pass_through() {
        let yaml = "parts:\n- effect_1_type: 99\n  category_voices: [Nope]\n";
        let out = resolve_names_str(yaml, Params(PARAMS), &Cats).unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(v["parts"][0]["effect_1_type"].as_i64(), Some(99));
        assert_eq!(v["parts"][0]["category_voices"][0].as_str(), Some("Nope"));
    }

    #[test]
    fn label_is_inverse() {
        let yaml = "parts:\n- effect_1_type: 27\n";
        let out = label_names_str(yaml, Params(PARAMS), &Cats).unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(v["parts"][0]["effect_1_type"].as_str(), Some("Hall Reverb"));
    }
}
