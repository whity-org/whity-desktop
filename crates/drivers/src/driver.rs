//! The one trait every hardware driver implements, and the registry that
//! routes a capability name to the driver that offers it.

use serde_json::Value;
use std::collections::HashMap;

/// A capability is `<driver id>.<operation>`: `printer.print`, `serial.open`.
/// The driver id names the crate; the operation is what `call` receives.
pub trait Driver: Send + Sync {
    /// Stable, lower-case, no dots: `printer`, `serial`, `scanner`, `card-reader`.
    fn id(&self) -> &'static str;
    /// The capabilities this driver offers, each starting with `id().`.
    fn capabilities(&self) -> &'static [&'static str];
    /// Run one operation. `op` is the part after the dot.
    fn call(&self, op: &str, args: Value) -> Result<Value, DriverError>;
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "code", content = "message")]
pub enum DriverError {
    /// No driver offers this capability in this build.
    Unsupported(String),
    /// The arguments are not what the operation takes.
    InvalidArgs(String),
    /// The hardware or the OS refused.
    Failed(String),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::Unsupported(m) => write!(f, "unsupported: {m}"),
            DriverError::InvalidArgs(m) => write!(f, "invalid arguments: {m}"),
            DriverError::Failed(m) => write!(f, "failed: {m}"),
        }
    }
}

/// The drivers of this build, by the capabilities they offer.
#[derive(Default)]
pub struct Registry {
    drivers: Vec<Box<dyn Driver>>,
    by_capability: HashMap<&'static str, usize>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a driver. A capability two drivers both offer, or one that
    /// does not start with the driver's id, is a programming error and panics
    /// at start, where it is seen, rather than at a plugin's call.
    pub fn add(&mut self, driver: Box<dyn Driver>) {
        let index = self.drivers.len();
        for capability in driver.capabilities() {
            let prefix = format!("{}.", driver.id());
            assert!(
                capability.starts_with(&prefix),
                "capability {capability} does not belong to driver {}",
                driver.id()
            );
            let previous = self.by_capability.insert(capability, index);
            assert!(
                previous.is_none(),
                "capability {capability} is offered twice"
            );
        }
        self.drivers.push(driver);
    }

    pub fn capabilities(&self) -> Vec<&'static str> {
        let mut all: Vec<&'static str> = self.by_capability.keys().copied().collect();
        all.sort_unstable();
        all
    }

    pub fn offers(&self, capability: &str) -> bool {
        self.by_capability.contains_key(capability)
    }

    /// Route `capability` to its driver and run it.
    pub fn dispatch(&self, capability: &str, args: Value) -> Result<Value, DriverError> {
        let index = self
            .by_capability
            .get(capability)
            .ok_or_else(|| DriverError::Unsupported(format!("no driver offers {capability}")))?;
        let driver = &self.drivers[*index];
        let op = &capability[driver.id().len() + 1..];
        driver.call(op, args)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A driver for tests: echoes its arguments, fails on request.
    pub struct EchoDriver;

    impl Driver for EchoDriver {
        fn id(&self) -> &'static str {
            "echo"
        }
        fn capabilities(&self) -> &'static [&'static str] {
            &["echo.say", "echo.fail"]
        }
        fn call(&self, op: &str, args: Value) -> Result<Value, DriverError> {
            match op {
                "say" => Ok(json!({ "said": args })),
                "fail" => Err(DriverError::Failed("as asked".into())),
                other => Err(DriverError::Unsupported(other.into())),
            }
        }
    }

    #[test]
    fn a_capability_routes_to_its_driver_and_operation() {
        let mut registry = Registry::new();
        registry.add(Box::new(EchoDriver));
        assert_eq!(registry.capabilities(), vec!["echo.fail", "echo.say"]);
        assert_eq!(
            registry.dispatch("echo.say", json!("hi")).unwrap(),
            json!({ "said": "hi" })
        );
        assert_eq!(
            registry.dispatch("echo.fail", json!(null)),
            Err(DriverError::Failed("as asked".into()))
        );
        assert!(matches!(
            registry.dispatch("scanner.scan", json!(null)),
            Err(DriverError::Unsupported(_))
        ));
    }

    #[test]
    #[should_panic(expected = "does not belong")]
    fn a_capability_outside_the_drivers_id_is_refused_at_start() {
        struct Bad;
        impl Driver for Bad {
            fn id(&self) -> &'static str {
                "bad"
            }
            fn capabilities(&self) -> &'static [&'static str] {
                &["printer.print"]
            }
            fn call(&self, _: &str, _: Value) -> Result<Value, DriverError> {
                unreachable!()
            }
        }
        Registry::new().add(Box::new(Bad));
    }

    #[test]
    fn errors_serialise_with_a_code() {
        let json =
            serde_json::to_value(DriverError::InvalidArgs("text is required".into())).unwrap();
        assert_eq!(
            json,
            json!({ "code": "invalid_args", "message": "text is required" })
        );
    }
}
