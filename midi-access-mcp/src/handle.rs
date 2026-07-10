//! [`DeviceHandle`] — a concrete, `Send + Sync` snapshot of a [`Device`].
//!
//! The MCP server must be a single non-generic type (rmcp's `#[tool_router]`
//! generates inherent methods and a router keyed on `Self`), and it must be
//! `Clone + Send + Sync + 'static`. Rather than push those bounds onto the kit's
//! `Device`/`Catalogs` traits, this erases `D` at construction:
//!
//! - static reference data (areas, params) is copied as `&'static` slices;
//! - expensive-but-fixed data (catalogs, schemas, defaults) is computed once;
//! - the behaviour that needs `D` (`accepts`, `encode`, name resolution) is kept
//!   as plain `fn` pointers, which are `Send + Sync` for free.
//!
//! Everything here is pure — no MIDI port is ever opened.

use serde_json::{json, Value as Json};
use serde_yaml::Value as Yaml;

use midi_access_core::{Area, Device, DeviceError, Params};

/// A device's reference data plus the few behaviours the MCP tools need.
#[derive(Clone)]
pub struct DeviceHandle {
    name: &'static str,
    areas: &'static [Area],
    params: Params,
    catalog_names: Vec<String>,
    /// `Catalogs::as_value()`, converted to JSON once.
    catalogs: Json,
    /// `(area, factory-default document as YAML)`.
    defaults: Vec<(&'static str, String)>,
    /// `(area, JSON Schema)`.
    schemas: Vec<(&'static str, String)>,

    resolve: fn(&mut Yaml),
    accepts: fn(&str, &Yaml) -> bool,
    encode: fn(&str, &Yaml, u8) -> Result<Vec<u8>, DeviceError>,
}

impl DeviceHandle {
    /// Snapshot `D`. Computes the catalogs, schemas, and defaults once.
    pub fn of<D: Device>() -> Self {
        let areas = D::areas();
        let catalogs = serde_json::to_value(D::catalogs().as_value()).unwrap_or(Json::Null);
        let catalog_names = D::catalogs()
            .names()
            .iter()
            .map(|s| s.to_string())
            .collect();

        let defaults = areas
            .iter()
            .filter_map(|a| {
                let doc = D::defaults(a.name)?;
                let yaml = serde_yaml::to_string(&doc).ok()?;
                Some((a.name, yaml))
            })
            .collect();
        let schemas = areas
            .iter()
            .filter_map(|a| D::schema(a.name).map(|s| (a.name, s)))
            .collect();

        Self {
            name: D::NAME,
            areas,
            params: D::params(),
            catalog_names,
            catalogs,
            defaults,
            schemas,
            // Non-capturing closure: coerces to a plain fn pointer.
            resolve: |v| midi_access_core::resolve_names(v, D::params(), D::catalogs()),
            accepts: D::accepts,
            encode: D::encode,
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn areas(&self) -> &'static [Area] {
        self.areas
    }

    fn area_names(&self) -> Vec<&'static str> {
        self.areas.iter().map(|a| a.name).collect()
    }

    /// Canonical area name for a (possibly loosely-spelled) token.
    fn canon(&self, area: &str) -> Option<&'static str> {
        self.areas.iter().find(|a| a.matches(area)).map(|a| a.name)
    }

    pub fn schema(&self, area: &str) -> Option<&str> {
        let area = self.canon(area)?;
        self.schemas
            .iter()
            .find(|(a, _)| *a == area)
            .map(|(_, s)| s.as_str())
    }

    pub fn defaults(&self, area: &str) -> Option<&str> {
        let area = self.canon(area)?;
        self.defaults
            .iter()
            .find(|(a, _)| *a == area)
            .map(|(_, s)| s.as_str())
    }

    /// The orienting payload: what this device is and what can be asked of it.
    pub fn describe(&self) -> Json {
        let mut groups: Vec<&'static str> = Vec::new();
        for m in self.params.as_slice() {
            if !groups.contains(&m.group) {
                groups.push(m.group);
            }
        }
        let catalogs: Vec<Json> = self
            .catalog_names
            .iter()
            .map(|n| json!({ "name": n, "entries": self.catalog_len(n) }))
            .collect();

        json!({
            "device": self.name,
            "areas": self.areas.iter().map(|a| json!({
                "name": a.name,
                "label": a.label,
                "about": a.about,
                "has_schema": self.schema(a.name).is_some(),
                "has_defaults": self.defaults(a.name).is_some(),
            })).collect::<Vec<_>>(),
            "param_count": self.params.as_slice().len(),
            "param_groups": groups,
            "catalogs": catalogs,
            "resources": self.areas.iter().flat_map(|a| {
                let mut uris = Vec::new();
                if self.schema(a.name).is_some() { uris.push(format!("schema://{}", a.name)); }
                if self.defaults(a.name).is_some() { uris.push(format!("defaults://{}", a.name)); }
                uris
            }).collect::<Vec<_>>(),
        })
    }

    fn catalog_len(&self, name: &str) -> usize {
        self.catalogs
            .get(name)
            .and_then(Json::as_array)
            .map(Vec::len)
            .unwrap_or(0)
    }

    /// Full-text search over the parameter metadata.
    pub fn search_params(&self, query: Option<&str>, group: Option<&str>, limit: usize) -> Json {
        let q = query.map(str::to_lowercase);
        let matched: Vec<&midi_access_core::ParamMeta> = self
            .params
            .as_slice()
            .iter()
            .filter(|m| group.is_none_or(|g| m.group.eq_ignore_ascii_case(g)))
            .filter(|m| {
                q.as_deref().is_none_or(|q| {
                    let hay = format!("{} {} {} {}", m.path, m.label, m.group, m.help);
                    hay.to_lowercase().contains(q)
                })
            })
            .collect();

        let total = matched.len();
        let entries: Vec<Json> = matched
            .into_iter()
            .take(limit)
            .map(|m| serde_json::to_value(m).unwrap_or(Json::Null))
            .collect();

        json!({
            "total": total,
            "returned": entries.len(),
            "truncated": total > entries.len(),
            "params": entries,
        })
    }

    /// One catalog's entries, filtered and paged.
    pub fn get_catalog(
        &self,
        name: &str,
        filter: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Json, String> {
        let entries = self
            .catalogs
            .get(name)
            .and_then(Json::as_array)
            .ok_or_else(|| {
                format!(
                    "unknown catalog {name:?} (available: {})",
                    self.catalog_names.join(", ")
                )
            })?;

        let needle = filter.map(str::to_lowercase);
        let matched: Vec<&Json> = entries
            .iter()
            .filter(|e| {
                needle
                    .as_deref()
                    .is_none_or(|n| e.to_string().to_lowercase().contains(n))
            })
            .collect();

        let total = matched.len();
        let page: Vec<&Json> = matched.into_iter().skip(offset).take(limit).collect();
        Ok(json!({
            "catalog": name,
            "total": total,
            "offset": offset,
            "returned": page.len(),
            "truncated": offset + page.len() < total,
            "entries": page,
        }))
    }

    /// Parse a document and encode it through the device's codec.
    pub fn validate(&self, area: &str, document: &str) -> Json {
        let Some(area) = self.canon(area) else {
            return json!({
                "ok": false,
                "error": format!("unknown area (valid: {})", self.area_names().join(", ")),
            });
        };
        let doc: Yaml = match serde_yaml::from_str(document) {
            Ok(v) => v,
            Err(e) => return json!({ "ok": false, "area": area, "error": format!("YAML: {e}") }),
        };
        // `accepts` is a parse-level check; `encode` also validates byte ranges.
        // Reporting both tells the caller whether the shape or a value is wrong.
        let parses = (self.accepts)(area, &doc);
        match (self.encode)(area, &doc, 0) {
            Ok(bytes) => json!({ "ok": true, "area": area, "encoded_bytes": bytes.len() }),
            Err(e) => json!({
                "ok": false,
                "area": area,
                "parses_as_area": parses,
                "error": e.to_string(),
            }),
        }
    }

    /// Rewrite catalog-backed value names into numbers, returning YAML.
    pub fn resolve_names(&self, document: &str) -> Result<String, String> {
        let mut doc: Yaml = serde_yaml::from_str(document).map_err(|e| format!("YAML: {e}"))?;
        (self.resolve)(&mut doc);
        serde_yaml::to_string(&doc).map_err(|e| format!("YAML: {e}"))
    }

    /// Resolve a `schema://{area}` or `defaults://{area}` URI to its text.
    pub fn read_resource(&self, uri: &str) -> Option<String> {
        let (scheme, area) = uri.split_once("://")?;
        match scheme {
            "schema" => self.schema(area).map(str::to_string),
            "defaults" => self.defaults(area).map(str::to_string),
            _ => None,
        }
    }
}

impl std::fmt::Debug for DeviceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceHandle")
            .field("name", &self.name)
            .field("areas", &self.area_names())
            .field("params", &self.params.as_slice().len())
            .field("catalogs", &self.catalog_names)
            .finish()
    }
}
