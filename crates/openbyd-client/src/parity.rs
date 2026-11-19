//! Entry point for the parity tests of the flows ported here.
//!
//! Works as `openbyd_protocol::parity`: every ported function is reachable
//! under a name such as `"api.gps.fetch_gps_endpoint"`, taking and
//! returning JSON. A flow's call runs it against a script and returns its
//! trace (see [`crate::scripted`]). Nothing else should use this module.

use openbyd_protocol::parity::{Handler, first_of};
use serde_json::Value;

pub use openbyd_protocol::parity::{Outcome, ParityError};

const MODULES: &[Handler] = &[crate::api::parity_call, crate::mqtt::parity_call];

/// Runs the call named `name` with JSON `args`.
pub fn call(name: &str, args: &Value) -> Outcome {
    first_of(MODULES, name, args).unwrap_or_else(|| {
        Err(ParityError::new(
            "UnknownCall",
            format!("no parity call named {name:?}"),
        ))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn unknown_calls_are_reported() {
        assert_eq!(
            call("nothing.here", &json!({})).unwrap_err().kind,
            "UnknownCall"
        );
        assert_eq!(
            call("api.booking.anything", &json!({})).unwrap_err().kind,
            "UnknownCall"
        );
    }
}
