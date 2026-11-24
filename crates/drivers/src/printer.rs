//! The printer driver: list the printers the OS knows and print raw bytes to
//! one of them, through the `printers` crate (Windows spooler, macOS and
//! Linux CUPS). Raw means the printer's own defaults: no paper size or
//! orientation here. A plugin that needs exact output renders a PDF or ESC/POS
//! bytes itself and sends those.

use crate::driver::{Driver, DriverError};
use printers::common::base::job::PrinterJobOptions;
use serde_json::{json, Value};

pub struct PrinterDriver;

impl Driver for PrinterDriver {
    fn id(&self) -> &'static str {
        "printer"
    }

    fn capabilities(&self) -> &'static [&'static str] {
        &["printer.list", "printer.print"]
    }

    fn call(&self, op: &str, args: Value) -> Result<Value, DriverError> {
        match op {
            "list" => Ok(list()),
            "print" => print(&args),
            other => Err(DriverError::Unsupported(format!("printer.{other}"))),
        }
    }
}

/// `{ "printers": [{ "name", "isDefault" }], "default": name | null }`.
fn list() -> Value {
    let default = printers::get_default_printer().map(|p| p.name);
    let printers: Vec<Value> = printers::get_printers()
        .into_iter()
        .map(|p| json!({ "name": p.name, "isDefault": Some(&p.name) == default.as_ref() }))
        .collect();
    json!({ "printers": printers, "default": default })
}

/// Args: `{ "text": string, "printer"?: name, "jobName"?: string }`. Answers
/// `{ "printer": name }`.
fn print(args: &Value) -> Result<Value, DriverError> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| DriverError::InvalidArgs("text is required".into()))?;
    let job_name = args
        .get("jobName")
        .and_then(Value::as_str)
        .unwrap_or("Whity Desktop");
    let printer = match args.get("printer").and_then(Value::as_str) {
        Some(name) => printers::get_printer_by_name(name)
            .ok_or_else(|| DriverError::InvalidArgs(format!("no printer called {name}")))?,
        None => printers::get_default_printer()
            .ok_or_else(|| DriverError::Failed("no default printer is set".into()))?,
    };
    let options = PrinterJobOptions {
        name: Some(job_name),
        ..PrinterJobOptions::none()
    };
    printer
        .print(text.as_bytes(), options)
        // PrintersError implements Debug only.
        .map_err(|e| DriverError::Failed(format!("{e:?}")))?;
    Ok(json!({ "printer": printer.name }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_needs_text() {
        assert_eq!(
            PrinterDriver.call("print", json!({})),
            Err(DriverError::InvalidArgs("text is required".into()))
        );
    }

    #[test]
    fn an_unknown_printer_is_an_argument_error() {
        let result = PrinterDriver.call(
            "print",
            json!({ "text": "x", "printer": "no-such-printer-whity-test" }),
        );
        assert!(
            matches!(result, Err(DriverError::InvalidArgs(_))),
            "{result:?}"
        );
    }

    /// Talks to the OS: runs where a spooler exists and never asserts a count.
    #[test]
    fn list_answers_the_shape() {
        let value = PrinterDriver.call("list", json!(null)).unwrap();
        assert!(value["printers"].is_array());
        assert!(value.get("default").is_some());
    }
}
