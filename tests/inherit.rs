#![forbid(unsafe_code)]

#[path = "../generated/rust/env.rs"]
mod env;
#[path = "../generated/rust/runtime.rs"]
mod env_runtime;

use ores_otel_sidecar::{health, SidecarIdentity};

#[test]
fn inherits_shared_health() {
    let identity = SidecarIdentity::new(env::SERVICE, env::BIND);
    let payload = health::current(identity, None);
    assert!(payload.ok);
    assert_eq!(payload.service, env::SERVICE);
}

#[test]
fn runtime_falls_back_to_catalog_default() {
    let values = env_runtime::load_from(|_| None);
    assert_eq!(values.bind, env::BIND_DEFAULT);
}
