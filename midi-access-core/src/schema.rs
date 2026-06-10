//! JSON Schema emission (behind the `schema` feature).
//!
//! A device's typed area models derive [`schemars::JsonSchema`]; [`schema_json`]
//! renders one to a pretty-printed JSON string for the CLI's `schema` subcommand
//! and the CI drift check. `additionalProperties: false` and per-field
//! descriptions come from the models' own `#[serde(deny_unknown_fields)]` and
//! rustdoc — this just drives the generator with schemars' defaults.

/// Render the JSON Schema for `T` as a pretty-printed string.
pub fn schema_json<T: schemars::JsonSchema>() -> String {
    serde_json::to_string_pretty(&schemars::schema_for!(T)).expect("RootSchema serializes to JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(schemars::JsonSchema)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Demo {
        /// a field
        a: u8,
    }

    #[test]
    fn emits_draft7_with_no_additional_props() {
        let s = schema_json::<Demo>();
        assert!(s.contains("\"$schema\""));
        assert!(s.contains("\"additionalProperties\": false"));
        assert!(s.contains("\"title\": \"Demo\""));
    }
}
