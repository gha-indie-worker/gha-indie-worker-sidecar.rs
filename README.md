# gha-indie-worker-sidecar.rs

Sidecar for GHA Indie Worker.

Inherits [`ores-otel-sidecar`](https://github.com/ores-otel/ores-otel-sidecar.rs).
Bind with `GHA_INDIE_WORKER_SIDECAR_BIND` (default `127.0.0.1:9090`).

```sh
cargo run --bin gha-indie-worker-sidecar
```
