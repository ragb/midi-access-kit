//! File-level YAML helpers: read a document to a [`Value`], and write one with a
//! `# yaml-language-server: $schema=…` header so editors pick up completion.
//!
//! The schema header points at `./schemas/<device>-<area>.schema.json`, matching
//! where a device repo commits its generated schemas.

use std::path::Path;

use anyhow::{Context, Result};
use serde_yaml::Value;

/// The schema-hint comment line for an area's YAML file.
pub fn schema_header(device: &str, area: &str) -> String {
    format!("# yaml-language-server: $schema=./schemas/{device}-{area}.schema.json")
}

/// Read and parse a YAML/JSON file into a [`Value`].
pub fn read_value(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("parsing YAML from {}", path.display()))
}

/// Serialize `value` to YAML and write it to `path`, prepending the schema
/// header for `device`/`area` and creating parent directories as needed.
pub fn write_value(path: &Path, device: &str, area: &str, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    let body = serde_yaml::to_string(value).context("serialize document")?;
    let mut out = schema_header(device, area);
    out.push('\n');
    out.push_str(&body);
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}
