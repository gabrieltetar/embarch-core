/// Every route a live lease gates: a request without the token is refused
/// `409`. Decision 93 has the reason for each edge case. Core pairs it
/// with its own `LEASE_OPEN`, and a test there requires every route to be in
/// exactly one of the two, so a new route cannot be added without deciding.
pub const LEASE_GATED: &[(&str, &str)] = &[
    ("POST", "/flash"),
    ("POST", "/reset"),
    ("POST", "/bootload"),
    ("PUT", "/bootload/ports"),
    ("DELETE", "/bootload/ports"),
    ("GET", "/serial-log"),
    ("GET", "/dev-bench/hello"),
    ("POST", "/probes/enroll"),
    ("DELETE", "/probes/enrolled/{role}"),
    ("PUT", "/probes/enrolled/{role}/board"),
    ("DELETE", "/probes/by-serial/{serial}"),
    ("POST", "/dev-bench/link"),
    ("POST", "/signals"),
    ("DELETE", "/signals/{name}"),
    ("POST", "/signals/{name}/exchange"),
    ("POST", "/signals/{name}/capture"),
    ("DELETE", "/signals/{name}/capture"),
    ("PUT", "/instruments"),
    ("DELETE", "/instruments/{name}"),
    ("POST", "/instruments/{name}/exchange"),
    ("POST", "/live/mem-read"),
    ("POST", "/live/core-state"),
    ("POST", "/validate"),
    ("POST", "/study"),
    ("POST", "/host-studies"),
    ("POST", "/study/{study_id}/cancel"),
];

#[cfg(test)]
mod tests {
    use super::LEASE_GATED;

    #[test]
    fn the_gated_table_is_pinned_in_order() {
        let joined: Vec<String> = LEASE_GATED.iter().map(|(m, p)| format!("{m} {p}")).collect();
        assert_eq!(
            joined,
            [
                "POST /flash",
                "POST /reset",
                "POST /bootload",
                "PUT /bootload/ports",
                "DELETE /bootload/ports",
                "GET /serial-log",
                "GET /dev-bench/hello",
                "POST /probes/enroll",
                "DELETE /probes/enrolled/{role}",
                "PUT /probes/enrolled/{role}/board",
                "DELETE /probes/by-serial/{serial}",
                "POST /dev-bench/link",
                "POST /signals",
                "DELETE /signals/{name}",
                "POST /signals/{name}/exchange",
                "POST /signals/{name}/capture",
                "DELETE /signals/{name}/capture",
                "PUT /instruments",
                "DELETE /instruments/{name}",
                "POST /instruments/{name}/exchange",
                "POST /live/mem-read",
                "POST /live/core-state",
                "POST /validate",
                "POST /study",
                "POST /host-studies",
                "POST /study/{study_id}/cancel",
            ]
        );
    }
}
