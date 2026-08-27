#![forbid(unsafe_code)]

#[path = "../generated/rust/env.rs"]
mod env;

use ores_otel_sidecar::{health, SidecarIdentity};

#[test]
fn inherits_shared_health() {
    let identity = SidecarIdentity::new(env::SERVICE, env::BIND);
    let payload = health::current(identity, None);
    assert!(payload.ok);
    assert_eq!(payload.service, env::SERVICE);
}
