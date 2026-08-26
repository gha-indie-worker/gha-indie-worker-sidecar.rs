#![forbid(unsafe_code)]

use gha_indie_worker_sidecar::{config::SidecarConfig, runtime};

fn main() {
    let cfg = SidecarConfig::from_env();
    runtime::run(&cfg);
}

