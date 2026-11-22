//! The PHP runtime of Whity Desktop: a FrankenPHP child process serving the
//! offline plugin host in `php-host/`, supervised from here (ADR 0001,
//! decision 2a). Nothing in this crate knows about Tauri; the app wires
//! status events to its window and the data directory to its own paths.
//!
//! Why a separate process: a PHP crash cannot take the app down, and a
//! process is the one boundary the operating system can lock down. The
//! lockdown lives in [`sandbox`]: an extra `php.ini` the host loads through
//! `PHP_INI_SCAN_DIR`, with `open_basedir` set to the host's own code and its
//! data directory and nothing else.

pub mod proxy;
pub mod sandbox;
pub mod sidecar;

#[cfg(target_os = "windows")]
mod job_object;

pub use sidecar::{Layout, PhpStatusEvent, Sidecar};
