#![forbid(unsafe_code)]

#[path = "../generated/rust/env.rs"]
mod env;

use ores_otel_sidecar::{runtime, SidecarConfig, SidecarIdentity};

fn main() {
    let cfg = SidecarConfig::from_env(SidecarIdentity::new(env::SERVICE, env::BIND));
    runtime::run(&cfg);
}
