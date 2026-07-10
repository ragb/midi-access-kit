# midi-access-kit

Shared foundation for a family of MIDI-device **access** crates
(ck / minilogue / re202 / ml10x …) that feed the `midi-ccess` SvelteKit editor.

Implement one trait — [`Device`] — and every device crate gets the same CLI
subcommands, the same catalog JSON shape, the same JSON-Schema generation, and
the same name↔number resolution, for free.

The kit is two crates:

| Crate | What it is | Key deps |
|-------|-----------|----------|
| **`midi-access-core`** | Pure library — the `Device` trait, parameter metadata, catalogs, name resolution, deep-merge, codec primitives, JSON Schema. No clap, no midir. Compiles for `wasm32-unknown-unknown`. | `serde`, `serde_yaml`, `thiserror` (+ optional `schemars`, `tsify-next`) |
| **`midi-access-cli`** | The generic CLI engine: `run::<D>()` builds the standard command set and dispatches through your `Device`. | `clap`, `midir`, `anyhow`, `midi-access-core` |
| **`midi-access-mcp`** | The generic MCP server: `serve::<D>()` exposes the device's metadata to an LLM over the Model Context Protocol, so it can author presets *by name*. Read-only. | `rmcp`, `tokio`, `midi-access-core` |

The document lingua franca between a device and the engine is
`serde_yaml::Value`: a device decodes a raw dump into a `Value` and encodes a
`Value` back to wire bytes, so the engine never touches the typed model.

## How to adopt: implement `Device`, call `run::<D>()`

### 1. Model your device in a pure `*-core` crate

Add metadata, catalogs, and the typed byte codec as usual. The kit gives you the
building blocks:

- `meta::{ParamMeta, Params, Kind, Choice, Level}` — one field-metadata model,
  keyed by serde path (`live_set.part.filter_cutoff`).
- `codec::{signed_center, read_u14, read_u16_nibbles, read_ascii, checksum,
  capture_reserved, split_sysex, byte_enum!, …}` — the recurring SysEx encodings
  (the checksum formula is shared; pass the region your vendor sums — Yamaha sums
  Model-ID+Address+Data, Roland sums Address+Data).
- `merge::{merge_over, slots}` — deep-merge + per-slot padding so a *partial*
  preset deserializes over a complete factory baseline.

### 2. Expose catalogs via the `Catalogs` trait

```rust
impl midi_access_core::Catalogs for MyCatalogs {
    fn resolve(&self, cat: &str, name: &str) -> Option<i64> { /* name → number */ }
    fn label(&self, cat: &str, value: i64) -> Option<String> { /* number → name */ }
    fn names(&self) -> &[&str] { &["voices", "effects", /* … */] }
    fn as_value(&self) -> serde_yaml::Value { /* all tables, for the bundle */ }
}
```

A `ParamMeta`'s `catalog: Some("voices")` hint wires a field to a catalog;
`midi_access_core::resolve_names_str` then turns names into numbers anywhere in a
document, and the inverse `label_names_str` turns numbers back into names.

### 3. Implement `Device`

```rust
use midi_access_core::{Area, Catalogs, Device, DeviceError, Inbound, Params};

pub struct MyDevice;

impl Device for MyDevice {
    const NAME: &'static str = "mydev";
    fn areas() -> &'static [Area] { /* e.g. system, live-set */ }
    fn params() -> Params { /* your ParamMeta table */ }
    fn catalogs() -> &'static dyn Catalogs { &MY_CATALOGS }
    fn defaults(area: &str) -> Option<serde_yaml::Value> { /* factory default doc */ }
    fn schema(area: &str) -> Option<String> { /* schema_json::<T>() */ }
    fn request(area: &str, ch: u8) -> Result<Vec<u8>, DeviceError> { /* request frames */ }
    fn decode(area: &str, dump: &[u8]) -> Result<serde_yaml::Value, DeviceError> { /* … */ }
    fn encode(area: &str, doc: &serde_yaml::Value, ch: u8) -> Result<Vec<u8>, DeviceError> { /* … */ }
    fn classify_inbound(bytes: &[u8]) -> Inbound { /* … */ }
    // Optional: override `accepts` for a deserialize-only kind check (used by
    // show/lint/diff) when a file may parse yet fail to byte-encode.
}
```

`request`/`encode` return **all** the SysEx frames for an operation concatenated
(a multi-block dump is a Bulk Header + content + Footer); the engine sends each
frame and, for a dump, collects every inbound frame until the device goes idle,
then hands the whole buffer to `decode`, which splits it with
`codec::split_sysex`. This keeps single- and multi-block devices uniform.

### 4. Your `main` is one line

```rust
fn main() -> std::process::ExitCode {
    midi_access_cli::run::<mydev_core::MyDevice>()
}
```

You now have these subcommands, identical across every device:

```
mydev dump <area> -o <file>          # read an area off the device into YAML
mydev sync <area> -i <file> [--verify]
mydev show <file>                    # identify + pretty-print (offline)
mydev lint <file>                    # validate through the typed model (offline)
mydev diff <a> <b>                   # field-by-field diff (offline)
mydev schema <area>                  # JSON Schema for an area (offline)
mydev catalog                        # params + catalogs + defaults as one JSON bundle (offline)
mydev resolve <file> [--area <a>]    # value names → numbers, codec-ready YAML (offline)
mydev identity                       # Universal Identity Request probe
mydev ports                          # list MIDI ports
```

Global options: `--port` / `--input-port` / `--output-port`, `--device`,
`-v/--verbose`, `-q/--quiet`.

## The catalog bundle

`mydev catalog` emits one standardized JSON object — everything a tool or LLM
needs to author a preset *by name* over a sensible baseline:

```json
{
  "device": "mydev",
  "params":   [ { "path": "...", "label": "...", "catalog": "voices" }, ... ],
  "catalogs": { "voices": [ ... ], "effects": [ ... ] },
  "defaults": { "system": { ... }, "live-set": { ... } }
}
```

## JSON Schema

Behind the `schema` feature, `midi_access_core::schema::schema_json::<T>()`
renders a typed area model's JSON Schema (draft-07, `additionalProperties:false`
from the model's `#[serde(deny_unknown_fields)]`). Commit the output under
`schemas/<device>-<area>.schema.json` and a `dump`'s YAML header points editors at
it; a CI step can regenerate and diff to catch drift.

## Authoring presets with an LLM (`midi-access-mcp`)

The same `Device` impl also gets you an MCP server, so an assistant can author
presets grounded in the device's own metadata rather than guessing:

```rust
// mydev/src/bin/mcp.rs
fn main() -> anyhow::Result<()> { midi_access_mcp::serve::<MyDevice>() }
```

The device is fixed at compile time, so a client configures **one instance per
device** and never has to disambiguate which one a call meant:

```json
{ "mcpServers": {
    "ck":    { "command": "ck-mcp" },
    "re202": { "command": "re202-mcp" }
}}
```

A catalog bundle is large (the CK's is ~110 KB), so it is exposed as something to
*query*, never to dump into a context window:

| Tool | Purpose |
|------|---------|
| `describe_device` | Name, areas, param groups, catalog names. The orienting call. |
| `search_params` | Full-text over path/label/group/help — the retrieval index. |
| `get_catalog` | One catalog's entries, filtered and paged. |
| `validate` | Parse + byte-encode a document, returning the error. Ground truth. |
| `resolve_names` | Value names → numbers, yielding codec-ready YAML. |

Plus two resources, `schema://{area}` and `defaults://{area}`.

The authoring loop is: `describe_device` → `search_params` → write YAML *by name*
→ `resolve_names` → `validate` → iterate. `validate` is what closes it: it
byte-encodes through the real codec, so it catches out-of-range values a JSON
Schema alone would not.

### Talking to the hardware

Give the server a port and it gains three more tools:

| Tool | Purpose |
|------|---------|
| `list_ports` | Enumerate MIDI ports. |
| `dump` | Read an area off the device — how you edit *the patch loaded right now*. |
| `sync` | Write a document back, optionally `verify`ing the read-back. |

```rust
// mydev/src/bin/mcp.rs
midi_access_mcp::serve_with::<MyDevice>(MidiConfig {
    input_port: Some("CK Series".into()),
    output_port: Some("CK Series".into()),
    device: 0,
    read_only: false,
})
```

**`sync` writes working memory by default** — audible immediately, discarded on a
power cycle. Committing to a saved slot is a separate, explicit `store` argument,
because it overwrites that slot for good; the tool description tells the model to
set it only when the user asked. `read_only: true` refuses writes entirely while
leaving `dump` available.

> An MCP stdio server must never write to stdout — that's the JSON-RPC channel.
> Hence `midi::port_names()` (which returns) alongside the CLI's `list_ports`
> (which prints).

## Status / exit codes

Status output goes through `output::Out` (a `Verbosity` gate); canonical payloads
(`schema`/`catalog`/`resolve`/`show`) are written straight to stdout so they stay
byte-stable. Exit codes live in `exit_codes` (`OK`, `USAGE_ERROR`,
`INPUT_FILE_ERROR`, `DEVICE_UNAVAILABLE`, …), promoted from the ml10x CLI.

## Reference adopter

[`ck-access-rs`](https://github.com/ragb/ck-access-rs) (Yamaha CK61/CK88) is built
on this kit: its `ck-core` implements `Device for Ck`, its CLI is
`run::<Ck>()`, and its wasm crate depends only on `ck-core` + `midi-access-core`
(never the CLI crate).

## Development

```sh
cargo test --workspace --all-features
cargo fmt --all
cargo clippy --workspace --all-features --all-targets
cargo build -p midi-access-core --target wasm32-unknown-unknown --features tsify
```

The generic engine is covered by tests against an in-crate fake `Device`
(`midi-access-cli/src/engine.rs`).

## License

MIT.
