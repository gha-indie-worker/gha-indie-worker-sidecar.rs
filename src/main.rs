#![forbid(unsafe_code)]

#[path = "../generated/rust/env.rs"]
mod env;
#[allow(clippy::match_like_matches_macro)]
#[path = "../generated/rust/runtime.rs"]
mod env_runtime;
mod pipe;

use ores_otel_sidecar::{runtime, SidecarConfig, SidecarIdentity};

fn main() {
    match pipe::run_if_requested() {
        Ok(true) => return,
        Ok(false) => {}
        Err(_) => {
            eprintln!("ghaiw-sidecar: live log pipe protocol failed");
            std::process::exit(2);
        }
    }

    let values = env_runtime::load_from_os();
    let Ok(cfg) = SidecarConfig::from_bind(
        SidecarIdentity::new(env::SERVICE, env::BIND),
        &values.bind,
        false,
    ) else {
        eprintln!("ghaiw-sidecar: invalid GHA_INDIE_WORKER_SIDECAR_BIND");
        std::process::exit(2);
    };
    runtime::run(&cfg);
}
