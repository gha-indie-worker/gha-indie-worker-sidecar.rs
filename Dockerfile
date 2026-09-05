# syntax=docker/dockerfile:1
#
# Distroless image for ghaiw-sidecar.
# Prefer linux/arm64:
#   docker buildx build --platform linux/arm64 -t ghaiw-sidecar:dev .
#
# This is the ores-otel loopback probe helper (127.0.0.1:9090 — /healthz
# /readyz /metrics), NOT an OTLP collector. Pair it in the same pod as the
# app. The app exports OTLP in-process to
# dd-otel-collector.observability.svc.cluster.local:4318 (HTTP) or :4317 (gRPC).
#
# No ores-sops in this image: secrets stay on the app container
# (env/enc + sops-entrypoint) or a k8s Secret. Distroless has no shell.
#
# k8s contract (see ores-otel/ores-otel-sidecar.rs/k8s/container.yaml):
#   - bind GHA_INDIE_WORKER_SIDECAR_BIND=127.0.0.1:9090 (loopback only)
#   - livenessProbe exec ["/ghaiw-sidecar", "probe"]
#   - no readinessProbe
#   - do not publish :9090 on a Service
#   - do not EXPOSE 4317/4318

FROM rust:1.90-bookworm AS build
ARG TARGETARCH
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git,sharing=locked \
    --mount=type=cache,target=/src/target,id=ghaiw-sidecar-target-${TARGETARCH},sharing=locked \
    cargo build --release --locked --bin ghaiw-sidecar \
    && strip "target/release/ghaiw-sidecar" \
    && cp "target/release/ghaiw-sidecar" "/usr/local/bin/ghaiw-sidecar"

FROM gcr.io/distroless/cc-debian12:nonroot
# Binary lives at /<bin> so kubelet exec ["/ghaiw-sidecar", "probe"] matches the
# ores-otel sidecar contract (no curl, no /usr/local/bin prefix required).
COPY --from=build --chown=65532:65532 "/usr/local/bin/ghaiw-sidecar" "/ghaiw-sidecar"
ENV GHA_INDIE_WORKER_SIDECAR_BIND=127.0.0.1:9090 \
    ORES_OTEL_SIDECAR_BIND=127.0.0.1:9090 \
    OTEL_SERVICE_NAME=ghaiw-sidecar
USER 65532:65532
ENTRYPOINT ["/ghaiw-sidecar"]
