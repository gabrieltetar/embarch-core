use serde::{Deserialize, Serialize};

/// `GET /dev-bench/hello`'s body: `HelloAck`'s fields, decoupled from the wire
/// type's capacity-bounded strings. Also what `embarch-umbrella`'s doctor check
/// 13 reads (`embarch-dev-bench` decision 25).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAckInfo {
    pub schema_version: u32,
    pub compatible: bool,
    /// The **bench's** own build, the value a `Study`'s
    /// `requires.dev_bench_version` is compared against. Served under this name
    /// since `embarch-core` decision 108: `firmware_version` on this route
    /// collided with `requires.firmware_version`, which is the DUT's. The wire
    /// `HelloAck` keeps its own spelling.
    pub dev_bench_version: String,
    /// The bench's own chip ID, empty when its build cannot answer. Named
    /// `self_reported_hardware_id` since decision 47, so it is not mistaken for
    /// the JTAG-read `hardware_id` of `/probes/enrolled`.
    pub self_reported_hardware_id: String,
    /// How that compares to the JTAG-read identity: `match`, `mismatch`,
    /// `not-reported` or `undeclared`.
    pub link_identity: String,
    /// The JTAG-read identity `link_identity` was compared against.
    pub probe_hardware_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINNED: &str = concat!(
        r#"{"schema_version":10,"compatible":true,"dev_bench_version":"g1a2b3c","#,
        r#""self_reported_hardware_id":"AAAABBBB","link_identity":"undeclared","#,
        r#""probe_hardware_id":"BBBBAAAA"}"#
    );

    #[test]
    fn the_hello_answer_is_the_pinned_json() {
        let info: HelloAckInfo = serde_json::from_str(PINNED).unwrap();
        assert_eq!(info.dev_bench_version, "g1a2b3c");
        assert_eq!(serde_json::to_string(&info).unwrap(), PINNED);
    }
}
