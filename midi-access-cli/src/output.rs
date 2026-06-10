//! Human-facing status output, promoted from the ml10x CLI as the kit standard.
//!
//! [`Out`] centralises status/progress messages with a [`Verbosity`] gate.
//! *Canonical payloads* — the JSON/YAML a command exists to produce (`schema`,
//! `catalog`, `resolve`, `show`) — are written straight to stdout by the command,
//! never through [`Out`], so that output is byte-for-byte stable regardless of
//! verbosity.

use std::io::Write;

/// How chatty status output is. Does not affect canonical payloads.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum Verbosity {
    /// Errors only.
    Quiet,
    /// Errors, warnings, and normal status lines.
    #[default]
    Normal,
    /// Everything, including per-step detail.
    Verbose,
}

/// Status/progress sink. Status lines go to stdout; warnings/errors to stderr.
#[derive(Debug, Default)]
pub struct Out {
    pub verbosity: Verbosity,
}

impl Out {
    pub fn new(verbosity: Verbosity) -> Self {
        Self { verbosity }
    }

    pub fn is_quiet(&self) -> bool {
        self.verbosity == Verbosity::Quiet
    }

    pub fn is_verbose(&self) -> bool {
        self.verbosity == Verbosity::Verbose
    }

    /// Normal-priority status line (stdout). Suppressed when quiet.
    pub fn info(&self, message: impl AsRef<str>) {
        if !self.is_quiet() {
            println!("{}", message.as_ref());
        }
    }

    /// Verbose-only detail (stdout). Suppressed unless verbose.
    pub fn detail(&self, message: impl AsRef<str>) {
        if self.is_verbose() {
            println!("{}", message.as_ref());
        }
    }

    /// Warning (stderr). Always shown.
    pub fn warn(&self, message: impl AsRef<str>) {
        let _ = writeln!(std::io::stderr(), "warning: {}", message.as_ref());
    }

    /// Error (stderr). Always shown.
    pub fn error(&self, message: impl AsRef<str>) {
        let _ = writeln!(std::io::stderr(), "error: {}", message.as_ref());
    }
}
