#![forbid(unsafe_code)]

#[path = "../generated/rust/env.rs"]
mod env;
#[path = "../generated/rust/runtime.rs"]
mod env_runtime;

use ores_otel_sidecar::{runtime, SidecarConfig, SidecarIdentity};

fn main() {
    let values = env_runtime::load_from_os();
    let cfg = match SidecarConfig::from_bind(
        SidecarIdentity::new(env::SERVICE, env::BIND),
        &values.bind,
        false,
    ) {
        Ok(cfg) => cfg,
        Err(_) => SidecarConfig::from_env(SidecarIdentity::new(env::SERVICE, env::BIND)),
    };
    runtime::run(&cfg);
}
