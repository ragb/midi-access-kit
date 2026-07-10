//! MIDI plumbing: enumerate, select, and open ports; send SysEx; collect dumps.
//!
//! Promoted from the ml10x / ck CLIs as the kit standard. The input callback
//! reassembles `F0…F7` frames (Windows WinMM can split a SysEx across buffers)
//! and forwards each complete frame to a channel. [`MidiSession::request_collect`]
//! sends a (possibly multi-frame) request and collects every inbound frame until
//! the device goes idle — uniform for single- and multi-block dumps.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use thiserror::Error;

use midi_access_core::codec::split_sysex;

const SYSEX_START: u8 = 0xF0;
const SYSEX_END: u8 = 0xF7;

/// midir's backend errors are stringified at the boundary so ours stays
/// `Send + Sync + 'static`.
#[derive(Debug, Error)]
pub enum MidiError {
    #[error("failed to open MIDI port: {0}")]
    Open(String),
    #[error("no MIDI {kind} port matches {needle:?}")]
    PortNotFound { kind: &'static str, needle: String },
    #[error("MIDI send failed: {0}")]
    Send(String),
    #[error("timed out after {0:?} waiting for a reply from the device")]
    Timeout(Duration),
}

fn open_err(e: impl std::fmt::Display) -> MidiError {
    MidiError::Open(e.to_string())
}

/// Enumerate the available `(inputs, outputs)` port names.
///
/// Returns them rather than printing: an MCP server speaks JSON-RPC on stdout, so
/// anything written there would corrupt the protocol stream.
pub fn port_names() -> Result<(Vec<String>, Vec<String>), MidiError> {
    let mi = MidiInput::new("midi-access-list-in").map_err(open_err)?;
    let mo = MidiOutput::new("midi-access-list-out").map_err(open_err)?;
    let name_of =
        |n: Result<String, midir::PortInfoError>| n.unwrap_or_else(|_| "<unknown>".into());
    let inputs = mi
        .ports()
        .iter()
        .map(|p| name_of(mi.port_name(p)))
        .collect();
    let outputs = mo
        .ports()
        .iter()
        .map(|p| name_of(mo.port_name(p)))
        .collect();
    Ok((inputs, outputs))
}

/// Print the available input and output ports to stdout (the CLI's `ports`).
pub fn list_ports() -> Result<(), MidiError> {
    let (inputs, outputs) = port_names()?;
    println!("Input ports:");
    for p in inputs {
        println!("  - {p}");
    }
    println!("\nOutput ports:");
    for p in outputs {
        println!("  - {p}");
    }
    Ok(())
}

/// An open connection to one device: output port + a channel of reassembled
/// inbound SysEx frames, plus the device/channel number.
pub struct MidiSession {
    output: MidiOutputConnection,
    rx: mpsc::Receiver<Vec<u8>>,
    _input: MidiInputConnection<()>,
    pub device: u8,
}

impl MidiSession {
    /// Open the input + output ports whose names contain the given substrings.
    pub fn open_with(
        input_substring: &str,
        output_substring: &str,
        device: u8,
    ) -> Result<Self, MidiError> {
        let mi = MidiInput::new("midi-access-in").map_err(open_err)?;
        let mo = MidiOutput::new("midi-access-out").map_err(open_err)?;

        let in_port = find_port(mi.ports(), |p| mi.port_name(p), input_substring, "input")?;
        let out_port = find_port(mo.ports(), |p| mo.port_name(p), output_substring, "output")?;

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        // Reassemble F0..F7 frames across callback invocations.
        let acc: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let _input = mi
            .connect(
                &in_port,
                "midi-access",
                move |_t, msg, _| reassemble(&acc, &tx, msg),
                (),
            )
            .map_err(open_err)?;
        let output = mo.connect(&out_port, "midi-access").map_err(open_err)?;

        Ok(Self {
            output,
            rx,
            _input,
            device,
        })
    }

    fn drain(&self) {
        while self.rx.try_recv().is_ok() {}
    }

    fn send_frame(&mut self, bytes: &[u8]) -> Result<(), MidiError> {
        self.output
            .send(bytes)
            .map_err(|e| MidiError::Send(e.to_string()))
    }

    /// Send one or more complete SysEx frames packed in `bytes` (each `F0…F7`),
    /// transmitting each frame individually.
    pub fn send_sysex(&mut self, bytes: &[u8]) -> Result<(), MidiError> {
        for frame in split_sysex(bytes) {
            self.send_frame(&frame)?;
        }
        Ok(())
    }

    /// Send `request` (one or more frames) and collect every inbound frame into
    /// one buffer, stopping once no frame arrives for `settle` (after at least one
    /// arrives) or `overall` elapses with nothing at all.
    pub fn request_collect(
        &mut self,
        request: &[u8],
        settle: Duration,
        overall: Duration,
    ) -> Result<Vec<u8>, MidiError> {
        self.drain();
        self.send_sysex(request)?;
        let mut buf = Vec::new();
        let mut deadline = Instant::now() + overall;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(frame) => {
                    buf.extend_from_slice(&frame);
                    deadline = Instant::now() + settle;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(buf)
    }

    /// Send a Universal Identity Request and collect inbound frames for `settle`.
    pub fn identity(&mut self, settle: Duration, overall: Duration) -> Result<Vec<u8>, MidiError> {
        const IDENTITY_REQUEST: [u8; 6] = [0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7];
        self.request_collect(&IDENTITY_REQUEST, settle, overall)
    }
}

/// Push complete `F0…F7` frames from a (possibly partial) callback buffer.
/// Bytes outside a frame (clock, active-sensing, channel messages) are dropped —
/// this engine only acts on SysEx dumps and identity replies.
fn reassemble(acc: &Arc<Mutex<Vec<u8>>>, tx: &mpsc::Sender<Vec<u8>>, msg: &[u8]) {
    let mut buf = acc.lock().unwrap();
    for &b in msg {
        if b == SYSEX_START {
            buf.clear();
            buf.push(b);
        } else if !buf.is_empty() {
            buf.push(b);
            if b == SYSEX_END {
                let _ = tx.send(std::mem::take(&mut *buf));
            }
        }
    }
}

fn find_port<P>(
    ports: Vec<P>,
    name_of: impl Fn(&P) -> Result<String, midir::PortInfoError>,
    needle: &str,
    kind: &'static str,
) -> Result<P, MidiError> {
    ports
        .into_iter()
        .find(|p| {
            name_of(p)
                .map(|n| n.to_lowercase().contains(&needle.to_lowercase()))
                .unwrap_or(false)
        })
        .ok_or_else(|| MidiError::PortNotFound {
            kind,
            needle: needle.to_string(),
        })
}
