//! Deep-merge and per-slot padding for partial presets.
//!
//! A device's typed model usually carries factory `Default`s and
//! `#[serde(default)]` containers so a *partial* preset deserializes over a
//! complete baseline. Two helpers make that work:
//!
//! - [`merge_over`] overlays a partial mapping onto a base, recursing into nested
//!   mappings; any non-mapping value (including arrays) replaces wholesale.
//! - [`slots`] is a `deserialize_with` helper for fixed-count arrays (zones,
//!   parts, …): each supplied slot is merged over *that slot's* factory default,
//!   and missing trailing slots are filled from the factory — so a preset need
//!   only name the slots, and the fields within them, that it changes.

use serde::{de, Deserialize, Deserializer, Serialize};
use serde_yaml::Value;

/// Deep-merge `overlay` onto `base`: mapping keys present in `overlay` win
/// (recursing into nested mappings); every other value replaces `base` wholesale
/// (so a supplied array overrides, never element-merges).
pub fn merge_over(base: Value, overlay: Value) -> Value {
    use serde_yaml::Value::Mapping;
    match (base, overlay) {
        (Mapping(mut b), Mapping(o)) => {
            for (k, ov) in o {
                let merged = match b.remove(&k) {
                    Some(bv) => merge_over(bv, ov),
                    None => ov,
                };
                b.insert(k, merged);
            }
            Mapping(b)
        }
        (_, overlay) => overlay,
    }
}

/// Build the canonical-count vector for one area: each slot `i` is the supplied
/// element (if any) merged over `factory(i)`, and missing trailing slots are the
/// bare factory slot. Rejects more elements than `count`.
///
/// Use from a field's `#[serde(deserialize_with = …)]`:
///
/// ```ignore
/// fn deser_parts<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Part>, D::Error> {
///     midi_access_core::merge::slots(d, 3, "parts", factory_part)
/// }
/// ```
pub fn slots<'de, D, T>(
    d: D,
    count: usize,
    label: &str,
    factory: impl Fn(usize) -> T,
) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Serialize + serde::de::DeserializeOwned,
{
    let raw = Vec::<Value>::deserialize(d)?;
    if raw.len() > count {
        return Err(de::Error::custom(format!(
            "at most {count} {label}, got {}",
            raw.len()
        )));
    }
    (0..count)
        .map(|i| {
            let base = serde_yaml::to_value(factory(i)).map_err(de::Error::custom)?;
            let v = match raw.get(i) {
                Some(overlay) => merge_over(base, overlay.clone()),
                None => base,
            };
            serde_yaml::from_value(v).map_err(de::Error::custom)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn merge_recurses_mappings_but_replaces_scalars_and_arrays() {
        let base: Value = serde_yaml::from_str("a: 1\nb:\n  c: 2\n  d: 3\ne: [1, 2]").unwrap();
        let overlay: Value = serde_yaml::from_str("b:\n  c: 9\ne: [7]").unwrap();
        let m = merge_over(base, overlay);
        assert_eq!(m["a"].as_i64(), Some(1)); // untouched
        assert_eq!(m["b"]["c"].as_i64(), Some(9)); // overridden
        assert_eq!(m["b"]["d"].as_i64(), Some(3)); // preserved
        assert_eq!(m["e"].as_sequence().unwrap().len(), 1); // array replaced
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Slot {
        on: bool,
        color: u8,
    }

    fn factory(i: usize) -> Slot {
        Slot {
            on: i == 0,
            color: i as u8,
        }
    }

    fn deser<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Slot>, D::Error> {
        slots(d, 3, "slots", factory)
    }

    #[derive(Deserialize)]
    struct Doc {
        #[serde(deserialize_with = "deser")]
        items: Vec<Slot>,
    }

    #[test]
    fn pads_and_merges_per_slot() {
        let d: Doc = serde_yaml::from_str("items:\n- color: 9\n").unwrap();
        assert_eq!(d.items.len(), 3);
        assert!(d.items[0].on); // factory slot 0 default kept
        assert_eq!(d.items[0].color, 9); // overridden
        assert_eq!(d.items[2].color, 2); // padded from factory
        assert!(!d.items[2].on);
    }

    #[test]
    fn rejects_too_many() {
        let err = serde_yaml::from_str::<Doc>("items: [{on: true, color: 1}, {}, {}, {}]\n");
        assert!(err.is_err());
    }
}
