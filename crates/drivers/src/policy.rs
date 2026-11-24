//! What the person at this device allows each plugin to do with its
//! hardware, in Whity's three words: Off, Ask first, Allowed.
//!
//! This crate asks a [`Policy`] before every call. The default here is the
//! floor: printing is Allowed, everything else is Off. The settings screen
//! that lets the person change it per plugin and per capability, and the
//! proposal that `Ask first` raises before a call runs, come with the
//! capability-manifest PR; until then `Ask first` is refused like `Off`, with
//! its own code so the PHP side can already tell the two apart.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Off,
    AskFirst,
    Allowed,
}

pub trait Policy: Send + Sync {
    fn decide(&self, plugin: &str, capability: &str) -> Decision;
}

/// Printing is Allowed for every plugin that declares it; everything else is
/// Off until the person turns it on.
#[derive(Default)]
pub struct DefaultPolicy;

impl Policy for DefaultPolicy {
    fn decide(&self, _plugin: &str, capability: &str) -> Decision {
        if capability.starts_with("printer.") {
            Decision::Allowed
        } else {
            Decision::Off
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printing_is_allowed_and_the_rest_is_off() {
        let policy = DefaultPolicy;
        assert_eq!(policy.decide("Any", "printer.print"), Decision::Allowed);
        assert_eq!(policy.decide("Any", "printer.list"), Decision::Allowed);
        assert_eq!(policy.decide("Any", "serial.open"), Decision::Off);
        assert_eq!(policy.decide("Any", "echo.say"), Decision::Off);
    }
}
