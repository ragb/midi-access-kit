//! The generic CLI engine: a concrete subcommand set dispatched through a
//! `Device` implementation. A device repo's `main.rs` is one line:
//!
//! ```ignore
//! fn main() -> std::process::ExitCode { midi_access_cli::run::<MyDevice>() }
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_yaml::Value;

use midi_access_core::codec::split_sysex;
use midi_access_core::{resolve_names, Bundle, Device, DeviceError, Inbound};

use crate::exit_codes::*;
use crate::midi::MidiSession;
use crate::output::{Out, Verbosity};
use crate::{midi, yaml_io};

const SETTLE: Duration = Duration::from_millis(600);
const OVERALL: Duration = Duration::from_millis(2500);
const IDENTITY_SETTLE: Duration = Duration::from_millis(300);

/// Global options, shared by every subcommand.
#[derive(Parser, Debug)]
#[command(
    version,
    about = "MIDI device access toolkit (dump / sync / edit over SysEx)."
)]
struct Cli {
    /// MIDI port name substring for both directions.
    #[arg(long, global = true)]
    port: Option<String>,
    /// MIDI input port substring (overrides --port).
    #[arg(long, global = true)]
    input_port: Option<String>,
    /// MIDI output port substring (overrides --port).
    #[arg(long, global = true)]
    output_port: Option<String>,
    /// Device / channel number (0..=15).
    #[arg(long, global = true, default_value_t = 0)]
    device: u8,
    /// Show extra detail.
    #[arg(long, short = 'v', global = true)]
    verbose: bool,
    /// Suppress everything but errors.
    #[arg(long, short = 'q', global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List available MIDI input and output ports.
    Ports,
    /// Probe the device with a Universal Identity Request.
    Identity,
    /// Read an area off the device into a YAML file.
    Dump {
        /// Which area (e.g. `system`, `live-set`).
        area: String,
        #[arg(short = 'o', long)]
        output: PathBuf,
    },
    /// Send a YAML file's settings to the device.
    Sync {
        area: String,
        #[arg(short = 'i', long)]
        input: PathBuf,
        /// After writing, read back and confirm it matches.
        #[arg(long)]
        verify: bool,
    },
    /// Pretty-print a YAML file, identifying its kind (no device needed).
    Show { path: PathBuf },
    /// Validate a YAML file by decoding it through the typed model.
    Lint { path: PathBuf },
    /// Compare two YAML files of the same kind, field by field.
    Diff { left: PathBuf, right: PathBuf },
    /// Print the JSON Schema for an area's document.
    Schema { area: String },
    /// Print the full metadata catalog (params + value lists + defaults) as JSON.
    Catalog,
    /// Normalize a preset's value names to numbers, printing codec-ready YAML.
    Resolve {
        path: PathBuf,
        /// Optional area hint (resolution is area-independent; accepted for parity).
        #[arg(long)]
        area: Option<String>,
    },
}

/// Build the clap command, parse, and dispatch through `D`. The entry point a
/// device repo's `main` calls.
pub fn run<D: Device>() -> ExitCode {
    match try_run::<D>(std::env::args_os()) {
        Ok(code) => ExitCode::from(code as u8),
        // A clap parse/help/version "error": print it and use clap's exit code.
        Err(e) => e.exit(),
    }
}

/// Parse `args` and dispatch, returning the command's exit code. Clap parse
/// failures (and `--help`/`--version`) surface as `Err`. Used by `run` and tests.
fn try_run<D: Device>(
    args: impl IntoIterator<Item = impl Into<std::ffi::OsString> + Clone>,
) -> Result<i32, clap::Error> {
    let cmd = Cli::command().name(D::NAME);
    let matches = cmd.try_get_matches_from(args)?;
    let cli = Cli::from_arg_matches(&matches)?;
    Ok(dispatch::<D>(cli))
}

fn dispatch<D: Device>(cli: Cli) -> i32 {
    let verbosity = if cli.verbose {
        Verbosity::Verbose
    } else if cli.quiet {
        Verbosity::Quiet
    } else {
        Verbosity::Normal
    };
    init_logger(verbosity);
    let out = Out::new(verbosity);

    if cli.verbose && cli.quiet {
        out.error("--verbose and --quiet are mutually exclusive");
        return USAGE_ERROR;
    }

    let session_args = SessionArgs {
        port: cli.port,
        input_port: cli.input_port,
        output_port: cli.output_port,
        device: cli.device,
    };

    let code = match cli.command {
        Command::Ports => cmd_ports(&out),
        Command::Identity => with_session(&out, &session_args, |s| identity::<D>(&out, s)),
        Command::Dump { area, output } => {
            with_session(&out, &session_args, |s| dump::<D>(&out, s, &area, &output))
        }
        Command::Sync {
            area,
            input,
            verify,
        } => with_session(&out, &session_args, |s| {
            sync::<D>(&out, s, &area, &input, verify)
        }),
        Command::Show { path } => show::<D>(&path),
        Command::Lint { path } => lint::<D>(&out, &path),
        Command::Diff { left, right } => diff::<D>(&out, &left, &right),
        Command::Schema { area } => schema::<D>(&out, &area),
        Command::Catalog => catalog::<D>(&out),
        Command::Resolve { path, area } => resolve::<D>(&out, &path, area.as_deref()),
    };
    code
}

fn init_logger(verbosity: Verbosity) {
    use log::LevelFilter;
    let default = match verbosity {
        Verbosity::Quiet => LevelFilter::Error,
        Verbosity::Normal => LevelFilter::Warn,
        Verbosity::Verbose => LevelFilter::Info,
    };
    let mut b = env_logger::Builder::new();
    if let Ok(env) = std::env::var("RUST_LOG") {
        b.parse_filters(&env);
    } else {
        b.filter_level(default);
    }
    let _ = b.try_init();
}

// === session ===

struct SessionArgs {
    port: Option<String>,
    input_port: Option<String>,
    output_port: Option<String>,
    device: u8,
}

fn open_session(out: &Out, args: &SessionArgs) -> Result<MidiSession, i32> {
    let port = args.port.as_deref();
    let input = args.input_port.as_deref().or(port);
    let output = args.output_port.as_deref().or(port);
    let (Some(input), Some(output)) = (input, output) else {
        out.error("--port (or --input-port/--output-port) required; run `ports` to list");
        return Err(USAGE_ERROR);
    };
    if args.device > 15 {
        out.error(format!("--device {} out of range (0..=15)", args.device));
        return Err(USAGE_ERROR);
    }
    MidiSession::open_with(input, output, args.device).map_err(|e| {
        out.error(format!("{e}"));
        DEVICE_UNAVAILABLE
    })
}

/// Open a session and run `f`, mapping a session-open failure to its exit code.
fn with_session(out: &Out, args: &SessionArgs, f: impl FnOnce(&mut MidiSession) -> i32) -> i32 {
    match open_session(out, args) {
        Ok(mut s) => f(&mut s),
        Err(code) => code,
    }
}

/// Resolve a CLI area token to its canonical name, or report a usage error.
fn area_name<D: Device>(out: &Out, name: &str) -> Result<&'static str, i32> {
    D::areas()
        .iter()
        .find(|a| a.matches(name))
        .map(|a| a.name)
        .ok_or_else(|| {
            let valid: Vec<&str> = D::areas().iter().map(|a| a.name).collect();
            out.error(format!(
                "unknown area {name:?} (valid: {})",
                valid.join(", ")
            ));
            USAGE_ERROR
        })
}

fn dev_err_code(e: &DeviceError) -> i32 {
    match e {
        DeviceError::UnknownArea(_) => USAGE_ERROR,
        DeviceError::Encode(_) => ENCODE_ERROR,
        DeviceError::Decode(_) | DeviceError::Other(_) => GENERIC_ERROR,
    }
}

// === commands ===

fn cmd_ports(out: &Out) -> i32 {
    match midi::list_ports() {
        Ok(()) => OK,
        Err(e) => {
            out.error(format!("{e}"));
            DEVICE_UNAVAILABLE
        }
    }
}

fn identity<D: Device>(out: &Out, s: &mut MidiSession) -> i32 {
    let raw = match s.identity(IDENTITY_SETTLE, OVERALL) {
        Ok(r) => r,
        Err(e) => {
            out.error(format!("{e}"));
            return DEVICE_UNAVAILABLE;
        }
    };
    for frame in split_sysex(&raw) {
        if let Inbound::Identity { bytes, model } = D::classify_inbound(&frame) {
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
            out.info(format!("reply: {}", hex.join(" ")));
            match model {
                Some(m) => out.info(format!("  -> {m} detected")),
                None => out.info("  -> not recognised (family bytes didn't match)"),
            }
            return OK;
        }
    }
    out.error("no identity reply received");
    DEVICE_UNAVAILABLE
}

fn dump<D: Device>(out: &Out, s: &mut MidiSession, area: &str, output: &Path) -> i32 {
    let area = match area_name::<D>(out, area) {
        Ok(a) => a,
        Err(c) => return c,
    };
    let req = match D::request(area, s.device) {
        Ok(r) => r,
        Err(e) => {
            out.error(format!("{e}"));
            return dev_err_code(&e);
        }
    };
    let raw = match s.request_collect(&req, SETTLE, OVERALL) {
        Ok(r) => r,
        Err(e) => {
            out.error(format!("{e}"));
            return DEVICE_UNAVAILABLE;
        }
    };
    if raw.is_empty() {
        out.error("no dump received (check --device and MIDI In/Out on the device)");
        return DEVICE_UNAVAILABLE;
    }
    let value = match D::decode(area, &raw) {
        Ok(v) => v,
        Err(e) => {
            out.error(format!("{e}"));
            return dev_err_code(&e);
        }
    };
    match yaml_io::write_value(output, D::NAME, area, &value) {
        Ok(()) => {
            out.info(format!("wrote {}", output.display()));
            OK
        }
        Err(e) => {
            out.error(format!("{e:#}"));
            GENERIC_ERROR
        }
    }
}

fn sync<D: Device>(out: &Out, s: &mut MidiSession, area: &str, input: &Path, verify: bool) -> i32 {
    let area = match area_name::<D>(out, area) {
        Ok(a) => a,
        Err(c) => return c,
    };
    let mut value = match yaml_io::read_value(input) {
        Ok(v) => v,
        Err(e) => {
            out.error(format!("{e:#}"));
            return INPUT_FILE_ERROR;
        }
    };
    // Names are an input-only convenience: resolve before encoding.
    resolve_names(&mut value, D::params(), D::catalogs());
    let bytes = match D::encode(area, &value, s.device) {
        Ok(b) => b,
        Err(e) => {
            out.error(format!("{e}"));
            return dev_err_code(&e);
        }
    };
    if let Err(e) = s.send_sysex(&bytes) {
        out.error(format!("{e}"));
        return DEVICE_UNAVAILABLE;
    }
    out.info(format!("sent {area} from {}", input.display()));

    if verify {
        std::thread::sleep(Duration::from_millis(80));
        let want = match D::decode(area, &bytes) {
            Ok(v) => v,
            Err(e) => {
                out.error(format!("{e}"));
                return dev_err_code(&e);
            }
        };
        let req = match D::request(area, s.device) {
            Ok(r) => r,
            Err(e) => {
                out.error(format!("{e}"));
                return dev_err_code(&e);
            }
        };
        let raw = match s.request_collect(&req, SETTLE, OVERALL) {
            Ok(r) => r,
            Err(e) => {
                out.error(format!("{e}"));
                return SYNC_NO_ACK;
            }
        };
        match D::decode(area, &raw) {
            Ok(got) if got == want => out.info("  verified: read-back matches"),
            Ok(_) => {
                out.error("verify FAILED: read-back differs from what we sent");
                return DEVICE_REJECTED;
            }
            Err(e) => {
                out.error(format!("verify decode failed: {e}"));
                return DEVICE_REJECTED;
            }
        }
    }
    OK
}

fn show<D: Device>(path: &Path) -> i32 {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: reading {}: {e}", path.display());
            return INPUT_FILE_ERROR;
        }
    };
    let kind = serde_yaml::from_str::<Value>(&text).ok().and_then(|v| {
        D::areas()
            .iter()
            .find(|a| D::accepts(a.name, &v))
            .map(|a| a.label)
    });
    println!(
        "# {} ({})",
        path.display(),
        kind.unwrap_or("unrecognized — printing raw")
    );
    print!("{text}");
    OK
}

fn lint<D: Device>(out: &Out, path: &Path) -> i32 {
    let value = match yaml_io::read_value(path) {
        Ok(v) => v,
        Err(e) => {
            out.error(format!("{e:#}"));
            return INPUT_FILE_ERROR;
        }
    };
    for a in D::areas() {
        if D::accepts(a.name, &value) {
            return match D::encode(a.name, &value, 0) {
                Ok(_) => {
                    out.info(format!("OK: {} is a valid {}", path.display(), a.label));
                    OK
                }
                Err(e) => {
                    out.error(format!("{e}"));
                    INPUT_FILE_ERROR
                }
            };
        }
    }
    let valid: Vec<&str> = D::areas().iter().map(|a| a.label).collect();
    out.error(format!(
        "{} parses as none of: {}",
        path.display(),
        valid.join(", ")
    ));
    INPUT_FILE_ERROR
}

fn diff<D: Device>(out: &Out, left: &Path, right: &Path) -> i32 {
    let a = match yaml_io::read_value(left) {
        Ok(v) => v,
        Err(e) => {
            out.error(format!("{e:#}"));
            return INPUT_FILE_ERROR;
        }
    };
    let b = match yaml_io::read_value(right) {
        Ok(v) => v,
        Err(e) => {
            out.error(format!("{e:#}"));
            return INPUT_FILE_ERROR;
        }
    };
    let area = D::areas()
        .iter()
        .find(|ar| D::accepts(ar.name, &a) && D::accepts(ar.name, &b));
    let Some(area) = area else {
        out.error("both files must be the same recognised kind");
        return USAGE_ERROR;
    };
    print_diff(
        &normalize::<D>(area.name, &a),
        &normalize::<D>(area.name, &b),
    );
    OK
}

/// Canonicalize a document by round-tripping it through the device's typed model
/// (encode→decode), falling back to a plain re-serialize if that fails.
fn normalize<D: Device>(area: &str, v: &Value) -> String {
    if let Ok(bytes) = D::encode(area, v, 0) {
        if let Ok(canon) = D::decode(area, &bytes) {
            if let Ok(s) = serde_yaml::to_string(&canon) {
                return s;
            }
        }
    }
    serde_yaml::to_string(v).unwrap_or_default()
}

fn print_diff(left: &str, right: &str) {
    let la: Vec<&str> = left.lines().collect();
    let lb: Vec<&str> = right.lines().collect();
    let mut any = false;
    for i in 0..la.len().max(lb.len()) {
        let a = la.get(i).copied().unwrap_or("");
        let b = lb.get(i).copied().unwrap_or("");
        if a == b {
            continue;
        }
        any = true;
        let ka = a.split_once(':').map(|(k, _)| k.trim());
        let kb = b.split_once(':').map(|(k, _)| k.trim());
        match (ka, kb) {
            (Some(ka), Some(kb)) if ka == kb => {
                let va = a.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
                let vb = b.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
                println!("  {ka}: {va}  →  {vb}");
            }
            _ => {
                println!("- {a}");
                println!("+ {b}");
            }
        }
    }
    if !any {
        println!("(no differences)");
    }
}

fn schema<D: Device>(out: &Out, area: &str) -> i32 {
    let area = match area_name::<D>(out, area) {
        Ok(a) => a,
        Err(c) => return c,
    };
    match D::schema(area) {
        Some(s) => {
            println!("{s}");
            OK
        }
        None => {
            out.error(format!("no schema for area {area:?}"));
            USAGE_ERROR
        }
    }
}

/// Build the catalog [`Bundle`] for `D` and render it as pretty JSON.
fn catalog_json<D: Device>() -> Result<String, serde_json::Error> {
    let mut defaults = serde_yaml::Mapping::new();
    for a in D::areas() {
        if let Some(d) = D::defaults(a.name) {
            defaults.insert(Value::String(a.name.to_string()), d);
        }
    }
    let bundle = Bundle {
        device: D::NAME,
        params: D::params(),
        catalogs: D::catalogs().as_value(),
        defaults: Value::Mapping(defaults),
    };
    serde_json::to_string_pretty(&bundle)
}

fn catalog<D: Device>(out: &Out) -> i32 {
    match catalog_json::<D>() {
        Ok(json) => {
            println!("{json}");
            OK
        }
        Err(e) => {
            out.error(format!("serialize catalog: {e}"));
            GENERIC_ERROR
        }
    }
}

fn resolve<D: Device>(out: &Out, path: &Path, _area: Option<&str>) -> i32 {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            out.error(format!("reading {}: {e}", path.display()));
            return INPUT_FILE_ERROR;
        }
    };
    match midi_access_core::resolve_names_str(&text, D::params(), D::catalogs()) {
        Ok(s) => {
            print!("{s}");
            OK
        }
        Err(e) => {
            out.error(format!("resolve names: {e}"));
            INPUT_FILE_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi_access_core::meta::{Level, ParamMeta};
    use midi_access_core::schema::schema_json;
    use midi_access_core::{Area, Catalogs, Params};

    // A tiny fake device: one area, `system`, whose document is `{ n: <0..=127> }`
    // encoded as the single SysEx frame `F0 <n> F7`. The `n` field is catalog-
    // backed so name resolution is exercised too.

    #[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct FakeDoc {
        /// the value
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
        fn as_value(&self) -> Value {
            serde_yaml::from_str("nums:\n- seven").unwrap()
        }
    }

    static PARAMS: &[ParamMeta] = &[ParamMeta {
        path: "system.n",
        label: "N",
        group: "Main",
        help: "the value",
        level: Level::Plain,
        kind: None,
        catalog: Some("nums"),
    }];
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
        fn defaults(area: &str) -> Option<Value> {
            (area == "system").then(|| serde_yaml::from_str("n: 0").unwrap())
        }
        fn schema(area: &str) -> Option<String> {
            (area == "system").then(schema_json::<FakeDoc>)
        }
        fn request(_area: &str, _ch: u8) -> Result<Vec<u8>, DeviceError> {
            Ok(vec![0xF0, 0x7E, 0xF7])
        }
        fn decode(area: &str, dump: &[u8]) -> Result<Value, DeviceError> {
            if area != "system" {
                return Err(DeviceError::UnknownArea(area.into()));
            }
            let frame = split_sysex(dump)
                .into_iter()
                .next()
                .ok_or_else(|| DeviceError::Decode("no frame".into()))?;
            let n = *frame
                .get(1)
                .ok_or_else(|| DeviceError::Decode("short".into()))?;
            serde_yaml::to_value(FakeDoc { n }).map_err(|e| DeviceError::Decode(e.to_string()))
        }
        fn encode(area: &str, doc: &Value, _ch: u8) -> Result<Vec<u8>, DeviceError> {
            if area != "system" {
                return Err(DeviceError::UnknownArea(area.into()));
            }
            let d: FakeDoc = serde_yaml::from_value(doc.clone())
                .map_err(|e| DeviceError::Encode(e.to_string()))?;
            Ok(vec![0xF0, d.n, 0xF7])
        }
        fn classify_inbound(bytes: &[u8]) -> Inbound {
            Inbound::Other(bytes.to_vec())
        }
    }

    #[test]
    fn catalog_bundle_has_standard_shape() {
        let json = catalog_json::<Fake>().unwrap();
        assert!(json.contains("\"device\": \"fake\""));
        assert!(json.contains("\"params\""));
        assert!(json.contains("\"catalogs\""));
        assert!(json.contains("\"defaults\""));
        assert!(json.contains("\"system.n\"")); // a param path
        assert!(json.contains("seven")); // catalog data
    }

    #[test]
    fn area_resolution_is_lenient_and_validating() {
        let out = Out::new(Verbosity::Quiet);
        assert_eq!(area_name::<Fake>(&out, "SYSTEM").unwrap(), "system");
        assert!(area_name::<Fake>(&out, "nope").is_err());
    }

    #[test]
    fn normalize_round_trips_through_codec() {
        let v: Value = serde_yaml::from_str("n: 42").unwrap();
        let s = normalize::<Fake>("system", &v);
        assert_eq!(s.trim(), "n: 42");
    }

    #[test]
    fn schema_command_and_unknown_area() {
        let out = Out::new(Verbosity::Quiet);
        assert_eq!(schema::<Fake>(&out, "system"), OK);
        assert_eq!(schema::<Fake>(&out, "bogus"), USAGE_ERROR);
    }

    #[test]
    fn dump_without_port_is_a_usage_error() {
        // `dump` needs a session; with no --port the engine reports a usage error
        // before ever touching MIDI.
        let code = try_run::<Fake>(["fake", "dump", "system", "-o", "out.yaml"]).unwrap();
        assert_eq!(code, USAGE_ERROR);
    }

    #[test]
    fn unknown_subcommand_is_clap_error() {
        assert!(try_run::<Fake>(["fake", "frobnicate"]).is_err());
    }

    #[test]
    fn catalog_via_try_run_succeeds() {
        assert_eq!(try_run::<Fake>(["fake", "catalog"]).unwrap(), OK);
    }

    #[test]
    fn resolve_and_show_on_temp_files() {
        let dir = std::env::temp_dir();
        let p = dir.join("mak_fake_resolve.yaml");
        std::fs::write(&p, "n: seven\n").unwrap();
        // resolve turns the name into its number via the catalog.
        let resolved =
            midi_access_core::resolve_names_str("n: seven\n", Fake::params(), Fake::catalogs())
                .unwrap();
        assert_eq!(resolved.trim(), "n: 7");
        // show identifies the kind for a numeric (encodable) file.
        std::fs::write(&p, "n: 7\n").unwrap();
        assert_eq!(show::<Fake>(&p), OK);
        let _ = std::fs::remove_file(&p);
    }
}
