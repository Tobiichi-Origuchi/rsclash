use rsclash_mihomo::models::{Connections, LogEntry, Memory, Proxies, Traffic, VersionInfo};

#[test]
fn controller_fixtures_remain_compatible() {
    let version: VersionInfo = decode(include_str!("fixtures/version.json"));
    let proxies: Proxies = decode(include_str!("fixtures/proxies.json"));
    let connections: Connections = decode(include_str!("fixtures/connections.json"));
    let traffic: Traffic = decode(include_str!("fixtures/traffic.json"));
    let memory: Memory = decode(include_str!("fixtures/memory.json"));
    let log: LogEntry = decode(include_str!("fixtures/log.json"));

    assert_eq!(version.version, "v1.19.28");
    assert_eq!(proxies.proxies["GLOBAL"].now.as_deref(), Some("Node A"));
    assert_eq!(connections.connections.as_deref().map(<[_]>::len), Some(1));
    assert_eq!((traffic.up, traffic.down), (10, 20));
    assert_eq!(memory.inuse, 1_048_576);
    assert_eq!(log.level, "info");
}

#[allow(clippy::expect_used)]
fn decode<T>(fixture: &str) -> T
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(fixture).expect("fixture should match its controller model")
}
