//! Hardware for plugins on the device (ADR 0001, decisions 3a and 3b).
//!
//! A [`Driver`] is a Rust crate behind one trait: an id, the capabilities it
//! offers (`printer.print`), and one `call`. Drivers are compiled in behind
//! Cargo features and registered in a [`Registry`] at start.
//!
//! Plugin PHP reaches them through the [`bridge`]: a loopback HTTP server
//! that gives the PHP host one opaque handle per loaded plugin, bound to the
//! capabilities that plugin declared. A call is refused when the handle is
//! unknown, when the plugin did not declare the capability, or when the
//! device's [`Policy`] has it Off. Nothing here runs a shell.

pub mod bridge;
pub mod driver;
pub mod policy;

#[cfg(feature = "driver-printer")]
pub mod printer;

pub use bridge::{Bridge, BridgeHandle};
pub use driver::{Driver, DriverError, Registry};
pub use policy::{Decision, DefaultPolicy, Policy};

/// Every driver this build ships, registered.
pub fn default_registry() -> Registry {
    #[allow(unused_mut)]
    let mut registry = Registry::new();
    #[cfg(feature = "driver-printer")]
    registry.add(Box::new(printer::PrinterDriver));
    registry
}
