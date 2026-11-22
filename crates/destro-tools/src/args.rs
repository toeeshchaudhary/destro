//! Tolerant JSON argument extraction helpers.
//!
//! Every helper reports problems as [`ToolOutput::err`] naming the argument.

use crate::{ToolInvocation, ToolOutput};

/// Extracts a required string argument.
pub(crate) fn require_str<'a>(inv: &'a ToolInvocation, name: &str) -> Result<&'a str, ToolOutput> {
    match inv.arguments.get(name) {
        Some(value) => value
            .as_str()
            .ok_or_else(|| ToolOutput::err(format!("argument \"{name}\" must be a string"))),
        None => Err(ToolOutput::err(format!(
            "missing required argument \"{name}\""
        ))),
    }
}

/// Extracts an optional string argument, ignoring a wrong-typed value.
pub(crate) fn opt_str<'a>(inv: &'a ToolInvocation, name: &str) -> Option<&'a str> {
    inv.arguments.get(name).and_then(serde_json::Value::as_str)
}

/// Extracts an optional boolean argument, ignoring a wrong-typed value.
pub(crate) fn opt_bool(inv: &ToolInvocation, name: &str) -> Option<bool> {
    inv.arguments.get(name).and_then(serde_json::Value::as_bool)
}

/// Extracts an optional unsigned integer argument.
///
/// Numeric strings are accepted as well; a wrong-typed value is ignored.
pub(crate) fn opt_u64(inv: &ToolInvocation, name: &str) -> Option<u64> {
    match inv.arguments.get(name)? {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}
