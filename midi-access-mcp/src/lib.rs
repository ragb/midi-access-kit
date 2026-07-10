#![forbid(unsafe_code)]

//! Generic MCP server for MIDI device access crates.
//!
//! Implement [`midi_access_core::Device`], then your `main` is one line:
//!
//! ```ignore
//! fn main() -> anyhow::Result<()> { midi_access_mcp::serve::<MyDevice>() }
//! ```
//!
//! This is the MCP analogue of `midi_access_cli::run::<D>()`. It speaks the Model
//! Context Protocol over stdio (via the official `rmcp` SDK), so an assistant can
//! author presets for the device *by name*, grounded in the device's own metadata.
//!
//! ## One server instance per device
//!
//! The device is fixed at compile time by `D`, so a client configures one instance
//! per device and never has to disambiguate which one a call meant:
//!
//! ```json
//! { "mcpServers": {
//!     "ck":    { "command": "ck-mcp" },
//!     "re202": { "command": "re202-mcp" }
//! }}
//! ```
//!
//! ## Surface
//!
//! A device's catalog bundle is large — the CK's is ~110 KB — so it is exposed as
//! something to *query*, never something to dump into a context window.
//!
//! **Tools**
//! - `describe_device` — name, areas, param groups, catalog names. The orienting call.
//! - `search_params` — full-text over path/label/group/help; the retrieval index.
//! - `get_catalog` — one catalog's entries, filtered and paged.
//! - `validate` — parse + byte-encode a document, returning the error if any. This
//!   closes the authoring loop so the model self-corrects instead of guessing.
//! - `resolve_names` — value names → numbers, yielding codec-ready YAML.
//!
//! **Resources**
//! - `schema://{area}` — the area's JSON Schema.
//! - `defaults://{area}` — the area's factory-default document.
//!
//! ## Device I/O
//!
//! Given a [`MidiConfig`] with a port, three more tools reach the hardware:
//! `list_ports`, `dump` (read an area — the entry point for "edit the patch I have
//! loaded right now"), and `sync` (write it back, optionally `verify`ing the
//! read-back).
//!
//! `sync` writes the device's **working memory** by default: audible immediately,
//! discarded on a power cycle. Committing to a saved slot is a separate, explicit
//! `store` argument, because it overwrites that slot for good. Start the server
//! with `read_only` to refuse writes entirely.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, InitializeResult, ListResourcesResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities,
};
use rmcp::service::RequestContext;
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData as McpError, RoleServer};
use rmcp::{ServerHandler, ServiceExt};
use serde::Deserialize;

use midi_access_core::Device;

mod handle;
pub use handle::{DeviceHandle, MidiConfig};

/// Run the MCP server for `D` on stdio with no MIDI port configured — the
/// offline authoring tools only. Blocks until the client disconnects.
pub fn serve<D: Device>() -> anyhow::Result<()> {
    serve_with::<D>(MidiConfig::default())
}

/// Run the MCP server for `D` on stdio, reaching the hardware through `midi`.
///
/// Builds its own tokio runtime, so a device crate's `main` needs no async setup.
pub fn serve_with<D: Device>(midi: MidiConfig) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve_async::<D>(midi))
}

/// Run the MCP server for `D` on stdio inside an existing tokio runtime.
pub async fn serve_async<D: Device>(midi: MidiConfig) -> anyhow::Result<()> {
    let server = MidiAccessServer::new(DeviceHandle::of::<D>().with_midi(midi));
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Run a blocking MIDI operation off the async runtime, mapping either failure
/// into a tool-level error the caller can read.
async fn blocking<T, F>(f: F) -> Result<CallToolResult, McpError>
where
    T: serde::Serialize + Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(CallToolResult::structured(
            serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
        )),
        Ok(Err(e)) => Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
        Err(join) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "MIDI task panicked: {join}"
        ))])),
    }
}

// === tool arguments ===
//
// Derived with rmcp's re-exported schemars (1.x). The kit itself pins schemars 0.8
// for `schema_json`, and the two majors must never be crossed.

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParamsArgs {
    /// Case-insensitive text matched against a parameter's path, label, group, and
    /// help. Omit to list everything (subject to `limit`).
    pub query: Option<String>,
    /// Restrict to one display group, e.g. `"Filter and EG"`.
    pub group: Option<String>,
    /// Maximum entries to return (default 50).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetCatalogArgs {
    /// Catalog name, e.g. `"voices"`. `describe_device` lists them.
    pub name: String,
    /// Case-insensitive substring an entry must contain to be returned.
    pub filter: Option<String>,
    /// Maximum entries to return (default 50).
    pub limit: Option<usize>,
    /// Entries to skip before applying `limit` (default 0).
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ValidateArgs {
    /// Which area the document belongs to, e.g. `"live-set"`.
    pub area: String,
    /// The document, as YAML (JSON is accepted — YAML is a superset).
    pub document: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResolveNamesArgs {
    /// A document (possibly partial) using value *names*, as YAML or JSON.
    pub document: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DumpArgs {
    /// Which area to read off the device, e.g. `"live-set"`.
    pub area: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SyncArgs {
    /// Which area to write, e.g. `"live-set"`.
    pub area: String,
    /// The document, as YAML or JSON. May be partial (it is merged over the
    /// factory default) and may use value names (they are resolved for you).
    pub document: String,
    /// Read the area back afterwards and confirm it matches. Recommended.
    pub verify: Option<bool>,
    /// Also commit the write to persistent storage at this destination, e.g. the
    /// CK's `"20-8"` (page-sound). DESTRUCTIVE: overwrites that saved slot for
    /// good. Omit to leave the device's working memory only, which a power cycle
    /// discards. Only set this when the user explicitly asked to save to a slot.
    pub store: Option<String>,
}

// === server ===

/// The MCP server for one device, erased from `D` into a [`DeviceHandle`].
#[derive(Clone)]
pub struct MidiAccessServer {
    device: DeviceHandle,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl MidiAccessServer {
    pub fn new(device: DeviceHandle) -> Self {
        Self {
            device,
            tool_router: Self::tool_router(),
        }
    }

    /// The device this server speaks for.
    pub fn device(&self) -> &DeviceHandle {
        &self.device
    }

    #[tool(description = "\
Describe this MIDI device: its name, editable areas (each dumpable/syncable as one \
document), the parameter display groups, and the names of its value catalogs. Call \
this first to orient yourself before searching parameters or authoring a preset.")]
    pub async fn describe_device(&self) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::structured(self.device.describe()))
    }

    #[tool(description = "\
Search this device's editable parameters. Matches a case-insensitive query against \
each parameter's path, label, group, and help text, and returns their full metadata \
(path, label, group, help, value kind, and which value catalog the field draws on, \
if any). Use this instead of trying to load the whole catalog.")]
    pub async fn search_params(
        &self,
        Parameters(args): Parameters<SearchParamsArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::structured(self.device.search_params(
            args.query.as_deref(),
            args.group.as_deref(),
            args.limit.unwrap_or(50),
        )))
    }

    #[tool(description = "\
List the entries of one value catalog (e.g. `voices`, `part_effects`), optionally \
filtered by a case-insensitive substring and paged. A parameter's `catalog` field \
(from search_params) names the catalog its value is drawn from. Catalogs can be \
large — filter rather than paging through everything.")]
    pub async fn get_catalog(
        &self,
        Parameters(args): Parameters<GetCatalogArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.device.get_catalog(
            &args.name,
            args.filter.as_deref(),
            args.limit.unwrap_or(50),
            args.offset.unwrap_or(0),
        ) {
            Ok(v) => Ok(CallToolResult::structured(v)),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
        }
    }

    #[tool(description = "\
Validate a document for an area by parsing it into the device's typed model and \
encoding it to wire bytes. Returns ok=true, or the error explaining what is wrong. \
Run this after authoring or editing a preset — it is the ground truth, and it \
catches out-of-range values that the JSON Schema alone will not.")]
    pub async fn validate(
        &self,
        Parameters(args): Parameters<ValidateArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::structured(
            self.device.validate(&args.area, &args.document),
        ))
    }

    #[tool(description = "\
Rewrite a document's catalog-backed value *names* into the numbers the codec wants \
(e.g. effect_1_type: \"Hall Reverb\" becomes 27), returning codec-ready YAML. \
Numbers and unrecognised names pass through unchanged. Author presets by name, then \
call this, then validate.")]
    pub async fn resolve_names(
        &self,
        Parameters(args): Parameters<ResolveNamesArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.device.resolve_names(&args.document) {
            Ok(yaml) => Ok(CallToolResult::success(vec![ContentBlock::text(yaml)])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
        }
    }

    // === device I/O ===

    #[tool(description = "\
List the MIDI input and output ports on this machine. Use it to find the device's \
port name if `dump`/`sync` report that no port is configured.")]
    pub async fn list_ports(&self) -> Result<CallToolResult, McpError> {
        blocking(|| {
            midi_access_cli::midi::port_names()
                .map(
                    |(inputs, outputs)| serde_json::json!({ "inputs": inputs, "outputs": outputs }),
                )
                .map_err(|e| e.to_string())
        })
        .await
    }

    #[tool(description = "\
Read an area off the connected device and return it as YAML — the device's current \
state. This is how you edit the patch that is loaded right now: dump it, change what \
the user asked for, then sync it back. Read-only with respect to the device.")]
    pub async fn dump(
        &self,
        Parameters(args): Parameters<DumpArgs>,
    ) -> Result<CallToolResult, McpError> {
        let d = self.device.clone();
        blocking(move || d.dump(&args.area)).await
    }

    #[tool(description = "\
Write a document to the connected device. The document may be partial (it is merged \
over the factory default) and may use value names (resolved automatically). \
\n\nBy default this writes only the device's working memory: audible immediately, \
discarded on a power cycle. Pass `verify` to read it back and confirm. \
\n\nPass `store` ONLY when the user explicitly asked to save to a slot — it \
permanently overwrites that saved slot. Prefer syncing without `store` so the user \
can audition the sound first.")]
    pub async fn sync(
        &self,
        Parameters(args): Parameters<SyncArgs>,
    ) -> Result<CallToolResult, McpError> {
        let d = self.device.clone();
        blocking(move || {
            d.sync(
                &args.area,
                &args.document,
                args.verify.unwrap_or(false),
                args.store.as_deref(),
            )
        })
        .await
    }
}

// `router = self.tool_router` reuses the router built in `new`; the macro's
// default (`Self::tool_router()`) would rebuild it on every request.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for MidiAccessServer {
    fn get_info(&self) -> InitializeResult {
        let d = &self.device;
        let areas: Vec<&str> = d.areas().iter().map(|a| a.name).collect();
        let io_hint = if d.midi().read_only {
            "This server is read-only: `dump` works, but it will not write to the device."
        } else if d.midi().input_port.is_none() {
            "No MIDI port is configured, so `dump`/`sync` are unavailable; run \
             `list_ports` and restart the server with --port."
        } else {
            "`dump` reads the device's current state and `sync` writes it back — that is \
             how you edit the patch the user has loaded right now. `sync` touches working \
             memory only, which a power cycle discards; pass `store` solely when the user \
             asks to save to a slot, since that overwrites it permanently."
        };
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        InitializeResult::new(capabilities)
            .with_server_info(Implementation::new(
                format!("{}-mcp", d.name()),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "Authoring tools for the {name} MIDI device. Editable areas: {areas}.\n\n\
                 Start with `describe_device`. To build a preset: read the \
                 `defaults://<area>` resource for a baseline, use `search_params` and \
                 `get_catalog` to choose values *by name*, call `resolve_names` to turn \
                 those names into numbers, then `validate` to confirm it encodes. \
                 `schema://<area>` gives the JSON Schema.\n\n\
                 {io_hint}",
                name = d.name(),
                areas = areas.join(", "),
            ))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let mut resources = Vec::new();
        for area in self.device.areas() {
            if self.device.schema(area.name).is_some() {
                resources.push(resource(
                    format!("schema://{}", area.name),
                    format!("{} JSON Schema", area.label),
                    format!("JSON Schema for the `{}` document.", area.name),
                    "application/json",
                ));
            }
            if self.device.defaults(area.name).is_some() {
                resources.push(resource(
                    format!("defaults://{}", area.name),
                    format!("{} defaults", area.label),
                    format!("Factory-default `{}` document, as YAML.", area.name),
                    "application/yaml",
                ));
            }
        }
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = request.uri;
        let text = self
            .device
            .read_resource(&uri)
            .ok_or_else(|| McpError::resource_not_found(uri.clone(), None))?;
        Ok(ReadResourceResult::new(vec![
            ResourceContents::TextResourceContents {
                uri,
                mime_type: None,
                text,
                meta: None,
            },
        ]))
    }
}

fn resource(uri: String, name: String, description: String, mime: &str) -> Resource {
    let mut r = Resource::new(uri, name);
    r.description = Some(description);
    r.mime_type = Some(mime.to_string());
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi_access_core::meta::{Level, ParamMeta};
    use midi_access_core::{Area, Catalogs, DeviceError, Inbound, Params};
    use serde_yaml::Value as Yaml;

    // A fake device: one area, `system`, whose document is `{ n: 0..=255 }`
    // encoded as the single frame `F0 <n> F7`. `n` is catalog-backed, so name
    // resolution is exercised.

    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FakeDoc {
        n: u8,
    }

    struct FakeCats;
    impl Catalogs for FakeCats {
        fn resolve(&self, cat: &str, name: &str) -> Option<i64> {
            (cat == "nums" && name.eq_ignore_ascii_case("seven")).then_some(7)
        }
        fn label(&self, cat: &str, value: i64) -> Option<String> {
            (cat == "nums" && value == 7).then(|| "seven".to_string())
        }
        fn names(&self) -> &[&str] {
            &["nums"]
        }
        fn as_value(&self) -> Yaml {
            serde_yaml::from_str("nums:\n- {value: 7, label: seven}\n- {value: 8, label: eight}")
                .unwrap()
        }
    }

    static PARAMS: &[ParamMeta] = &[
        ParamMeta {
            path: "system.n",
            label: "The N",
            group: "Main",
            help: "how much n",
            level: Level::Plain,
            kind: None,
            catalog: Some("nums"),
        },
        ParamMeta {
            path: "system.cutoff",
            label: "Cutoff",
            group: "Filter",
            help: "brightness",
            level: Level::Magnitude,
            kind: None,
            catalog: None,
        },
    ];
    static AREAS: &[Area] = &[Area {
        name: "system",
        label: "System",
        about: "the system area",
    }];
    static CATS: FakeCats = FakeCats;

    struct Fake;
    impl Device for Fake {
        const NAME: &'static str = "fake";
        fn areas() -> &'static [Area] {
            AREAS
        }
        fn params() -> Params {
            Params(PARAMS)
        }
        fn catalogs() -> &'static dyn Catalogs {
            &CATS
        }
        fn defaults(area: &str) -> Option<Yaml> {
            (area == "system").then(|| serde_yaml::from_str("n: 0").unwrap())
        }
        fn schema(area: &str) -> Option<String> {
            (area == "system").then(|| r#"{"title":"FakeDoc"}"#.to_string())
        }
        fn request(_: &str, _: u8) -> Result<Vec<u8>, DeviceError> {
            Ok(vec![])
        }
        fn decode(_: &str, _: &[u8]) -> Result<Yaml, DeviceError> {
            Err(DeviceError::Decode("n/a".into()))
        }
        fn encode(area: &str, doc: &Yaml, _: u8) -> Result<Vec<u8>, DeviceError> {
            if area != "system" {
                return Err(DeviceError::UnknownArea(area.into()));
            }
            let d: FakeDoc = serde_yaml::from_value(doc.clone())
                .map_err(|e| DeviceError::Encode(e.to_string()))?;
            Ok(vec![0xF0, d.n, 0xF7])
        }
        fn classify_inbound(b: &[u8]) -> Inbound {
            Inbound::Other(b.to_vec())
        }
    }

    fn handle() -> DeviceHandle {
        DeviceHandle::of::<Fake>()
    }

    #[test]
    fn router_exposes_the_authoring_and_device_io_tools() {
        let tools = MidiAccessServer::tool_router().list_all();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "describe_device",
                "dump",
                "get_catalog",
                "list_ports",
                "resolve_names",
                "search_params",
                "sync",
                "validate",
            ]
        );
        // Every tool carries a description and an input schema for the client.
        for t in &tools {
            assert!(t.description.as_ref().is_some_and(|d| !d.is_empty()));
        }
    }

    #[test]
    fn describe_orients_the_caller() {
        let d = handle().describe();
        assert_eq!(d["device"], "fake");
        assert_eq!(d["param_count"], 2);
        assert_eq!(d["areas"][0]["name"], "system");
        assert_eq!(d["areas"][0]["has_schema"], true);
        assert_eq!(d["areas"][0]["has_defaults"], true);
        // Groups are de-duplicated in table order.
        assert_eq!(d["param_groups"], serde_json::json!(["Main", "Filter"]));
        // Catalog sizes are advertised so the caller knows to filter.
        assert_eq!(d["catalogs"][0]["name"], "nums");
        assert_eq!(d["catalogs"][0]["entries"], 2);
        let uris = d["resources"].as_array().unwrap();
        assert!(uris.iter().any(|u| u == "schema://system"));
        assert!(uris.iter().any(|u| u == "defaults://system"));
    }

    #[test]
    fn search_params_matches_help_and_group_and_reports_truncation() {
        let h = handle();
        // Matches on help text, not just label.
        let r = h.search_params(Some("brightness"), None, 50);
        assert_eq!(r["total"], 1);
        assert_eq!(r["params"][0]["path"], "system.cutoff");
        // `level` is serialized as the historical bool.
        assert_eq!(r["params"][0]["level"], true);

        // Group filter.
        assert_eq!(h.search_params(None, Some("Main"), 50)["total"], 1);
        // Limit surfaces truncation rather than silently dropping.
        let r = h.search_params(None, None, 1);
        assert_eq!(r["total"], 2);
        assert_eq!(r["returned"], 1);
        assert_eq!(r["truncated"], true);
    }

    #[test]
    fn get_catalog_filters_pages_and_rejects_unknown_names() {
        let h = handle();
        let all = h.get_catalog("nums", None, 50, 0).unwrap();
        assert_eq!(all["total"], 2);

        let filtered = h.get_catalog("nums", Some("SEVEN"), 50, 0).unwrap();
        assert_eq!(filtered["total"], 1);
        assert_eq!(filtered["entries"][0]["label"], "seven");

        let paged = h.get_catalog("nums", None, 1, 1).unwrap();
        assert_eq!(paged["returned"], 1);
        assert_eq!(paged["truncated"], false);
        assert_eq!(paged["entries"][0]["label"], "eight");

        let err = h.get_catalog("nope", None, 50, 0).unwrap_err();
        assert!(err.contains("unknown catalog"), "{err}");
        assert!(err.contains("nums"), "should list what is available: {err}");
    }

    #[test]
    fn validate_is_the_ground_truth() {
        let h = handle();
        let ok = h.validate("system", "n: 42");
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["encoded_bytes"], 3);

        // Out of range for u8: parses as YAML, fails to encode.
        let bad = h.validate("system", "n: 300");
        assert_eq!(bad["ok"], false);
        assert!(bad["error"].as_str().unwrap().contains("300"));

        // Malformed YAML.
        assert_eq!(h.validate("system", "n: [unclosed")["ok"], false);

        // Unknown area names the valid ones.
        let unknown = h.validate("bogus", "n: 1");
        assert_eq!(unknown["ok"], false);
        assert!(unknown["error"].as_str().unwrap().contains("system"));
    }

    #[test]
    fn resolve_names_rewrites_names_and_passes_numbers_through() {
        let h = handle();
        assert_eq!(h.resolve_names("n: seven").unwrap().trim(), "n: 7");
        assert_eq!(h.resolve_names("n: 9").unwrap().trim(), "n: 9");
        // Unknown names are left alone rather than erroring.
        assert_eq!(h.resolve_names("n: nope").unwrap().trim(), "n: nope");
        assert!(h.resolve_names("n: [unclosed").is_err());
    }

    #[test]
    fn resources_resolve_by_uri() {
        let h = handle();
        assert!(h
            .read_resource("schema://system")
            .unwrap()
            .contains("FakeDoc"));
        assert!(h
            .read_resource("defaults://system")
            .unwrap()
            .contains("n: 0"));
        // Area matching is lenient, like the CLI's.
        assert!(h.read_resource("schema://SYSTEM").is_some());
        assert!(h.read_resource("schema://bogus").is_none());
        assert!(h.read_resource("nonsense").is_none());
    }

    #[test]
    fn get_info_advertises_tools_resources_and_the_device() {
        let info = MidiAccessServer::new(handle()).get_info();
        assert_eq!(info.server_info.name, "fake-mcp");
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.resources.is_some());
        let instr = info.instructions.unwrap();
        assert!(instr.contains("fake"));
        // With no port configured, the handshake says so up front.
        assert!(instr.contains("No MIDI port is configured"));
    }

    /// End-to-end over the real protocol: a client on one end of an in-memory
    /// duplex, the server on the other. Proves the wiring (initialize, tools/list,
    /// tools/call, resources/read), not just that the handlers compile.
    #[tokio::test]
    async fn speaks_mcp_over_a_duplex_transport() {
        use rmcp::model::CallToolRequestParams;
        use rmcp::serve_client;

        let (client_io, server_io) = tokio::io::duplex(8192);
        let server = tokio::spawn(async move {
            let s = MidiAccessServer::new(DeviceHandle::of::<Fake>());
            s.serve(server_io).await.unwrap().waiting().await
        });

        let client = serve_client((), client_io).await.expect("initialize");

        // The handshake carried our device-specific server info + instructions.
        let info = client.peer_info().expect("server info");
        assert_eq!(info.server_info.name, "fake-mcp");
        assert!(info
            .instructions
            .as_ref()
            .unwrap()
            .contains("No MIDI port is configured"));

        // tools/list
        let tools = client.list_tools(Default::default()).await.unwrap();
        assert_eq!(tools.tools.len(), 8);

        // tools/call — resolve a value name through the real dispatch path.
        let args = serde_json::json!({ "document": "n: seven" })
            .as_object()
            .cloned()
            .unwrap();
        let out = client
            .call_tool(CallToolRequestParams::new("resolve_names").with_arguments(args))
            .await
            .unwrap();
        assert_ne!(out.is_error, Some(true));

        // resources/list + resources/read
        let resources = client.list_resources(Default::default()).await.unwrap();
        assert_eq!(resources.resources.len(), 2);
        let read = client
            .read_resource(rmcp::model::ReadResourceRequestParams::new(
                "defaults://system",
            ))
            .await
            .unwrap();
        assert_eq!(read.contents.len(), 1);

        client.cancel().await.unwrap();
        let _ = server.await;
    }

    #[tokio::test]
    async fn tool_wrappers_report_failure_as_tool_errors_not_protocol_errors() {
        let s = MidiAccessServer::new(handle());
        // A bad catalog name is the caller's problem: surface it in the result.
        let r = s
            .get_catalog(Parameters(GetCatalogArgs {
                name: "nope".into(),
                filter: None,
                limit: None,
                offset: None,
            }))
            .await
            .expect("no protocol error");
        assert_eq!(r.is_error, Some(true));

        let ok = s.describe_device().await.unwrap();
        assert_ne!(ok.is_error, Some(true));
    }
    #[test]
    fn device_io_without_a_port_reports_how_to_fix_it() {
        // No MidiConfig: `dump`/`sync` must explain themselves rather than hang or
        // open some arbitrary port.
        let h = handle();
        let e = h.dump("system").unwrap_err();
        assert!(e.contains("no MIDI port configured"), "{e}");
        assert!(e.contains("list_ports"), "should point at the fix: {e}");

        let e = h.sync("system", "n: 1", false, None).unwrap_err();
        assert!(e.contains("no MIDI port configured"), "{e}");
    }

    #[test]
    fn read_only_refuses_sync_before_touching_midi() {
        let h = handle().with_midi(MidiConfig {
            input_port: Some("nonexistent".into()),
            output_port: Some("nonexistent".into()),
            device: 0,
            read_only: true,
        });
        let e = h.sync("system", "n: 1", false, None).unwrap_err();
        assert!(e.contains("read-only"), "{e}");
        // Reads are still allowed (this one fails on the bogus port, not on policy).
        assert!(!h.dump("system").unwrap_err().contains("read-only"));
    }

    #[test]
    fn sync_validates_the_area_and_document_before_opening_a_port() {
        let h = handle().with_midi(MidiConfig {
            input_port: Some("nonexistent".into()),
            output_port: Some("nonexistent".into()),
            ..MidiConfig::default()
        });
        // Bad area and bad YAML must be caught without any MIDI traffic.
        assert!(h
            .sync("bogus", "n: 1", false, None)
            .unwrap_err()
            .contains("unknown area"));
        assert!(h
            .sync("system", "n: [unclosed", false, None)
            .unwrap_err()
            .contains("YAML"));
    }

    #[test]
    fn get_info_describes_the_io_posture() {
        let offline = MidiAccessServer::new(handle()).get_info();
        assert!(offline
            .instructions
            .unwrap()
            .contains("No MIDI port is configured"));

        let ro = MidiAccessServer::new(handle().with_midi(MidiConfig {
            read_only: true,
            ..MidiConfig::default()
        }))
        .get_info();
        assert!(ro.instructions.unwrap().contains("read-only"));

        let live = MidiAccessServer::new(handle().with_midi(MidiConfig {
            input_port: Some("CK".into()),
            output_port: Some("CK".into()),
            ..MidiConfig::default()
        }))
        .get_info();
        let i = live.instructions.unwrap();
        assert!(i.contains("`dump` reads"));
        assert!(i.contains("overwrites it permanently"));
    }
}
