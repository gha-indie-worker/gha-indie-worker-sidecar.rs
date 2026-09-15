# Live build-log receiver

`ghaiw-sidecar` supports two external-process log protocols. New IndieBuild/GHA integrations must use the current **build-log metadata v1** contract. The older `gha-indie-worker.log-sidecar.v1` stdin protocol remains a compatibility path only.

The current contract is owned by `gha-indie-worker/gha-indie-worker-interfaces` as two independent human-maintained authorities:

- TypeSpec: `contracts/build-log.tsp`;
- JSON Schema Draft 2020-12: `contracts/build-log.schema.json`.

`ORESoftware/typespec-json-schema-validator` (TJSV) must fail closed unless those peer authorities converge. Generated Schema B, Contract IR, SARIF, Rust projections, and verification receipts are evidence; they are not a third source of truth. This receiver repository independently re-runs TJSV consumer admission in CI before promotion.

## Current protocol: `gha-indie-worker.build-log-metadata/v1`

The worker selects the current protocol by setting:

```text
INDIEBUILD_LOG_PROTOCOL=gha-indie-worker.build-log-metadata/v1
INDIEBUILD_LOG_METADATA_FD=3
INDIEBUILD_LOG_DATA_FD=4
```

The actual descriptor numbers are communicated through the environment, although Unix producers currently use FD 3 for metadata and FD 4 for raw data by convention.

### FD 3 — metadata/control plane

FD 3 carries one UTF-8 JSON object per line. Each object validates against `BuildLogMetadata`. Core fields are:

- `schemaVersion`;
- `event` (`chunk`, `dropped`, `receiver_closed`, or `stream_closed`);
- `jobId`;
- `stream` (`stdout`, `stderr`, or `worker`);
- `sequence`;
- `byteLength`;
- `timestamp`.

Optional correlation dimensions include repository, GitHub organization, workflow, run/job/step identity, attempt, trace/span IDs, and dropped-chunk/byte accounting. Unknown fields fail closed in the strict Rust projection and authored schema.

Metadata is a control-plane lane. The reference receiver does **not** copy metadata text into stdout/stderr.

### FD 4 — raw data plane

For every metadata event with `event=chunk`, the receiver reads exactly `byteLength` bytes from FD 4 and routes them according to `stream`:

- `stdout` bytes go to receiver stdout;
- `stderr` bytes go to receiver stderr.

The data plane is binary-safe. It must not assume UTF-8, split on lines, parse ANSI escapes, or reinterpret NUL/non-UTF8 bytes. Worker/drop/lifecycle events have `byteLength=0` and consume no FD4 bytes.

The reference receiver rejects oversized chunks, truncated data, truncated metadata lines, mismatched protocol versions, raw data on the `worker` stream, malformed drop receipts, lifecycle events claiming raw bytes, and unknown/credential-like metadata fields.

## Primary-output and failure semantics

The receiver is always an optional export sink. The worker's normal stdout/stderr and bounded local build log remain authoritative.

The producer must preserve these invariants:

- receiver delivery uses a bounded queue and never synchronously backpressures the primary stdout/stderr path;
- queue saturation drops only the receiver copy and records bounded drop accounting;
- receiver EOF, crash, malformed input handling, or write pressure cannot change the build result;
- receiver writes are time-bounded;
- shutdown has **one total hard eight-second budget** covering admission stop, bounded drain, pipe closure/EOF, receiver exit, and forced termination;
- the budget is not multiplied per stream, job, or shutdown phase.

The worker/reference-sidecar release gate includes an exact-SHA real process test with ordinary stdout plus binary/non-UTF8 stderr. Unit tests on the producer and receiver are not sufficient substitutes for that process-boundary test.

## Receiver environment and credentials

The worker launches the receiver with an empty environment and passes only explicitly allowlisted values. The current worker also rejects attempts to inherit:

- producer-owned `INDIEBUILD_LOG_*` descriptor/protocol variables;
- receiver self-configuration variables;
- worker GitHub, AWS, database, Fiducia, webhook/auth, or lambda credentials;
- dynamic-loader/interpreter/compiler injection variables such as `LD_PRELOAD`, DYLD loader controls, `BASH_ENV`, `NODE_OPTIONS`, `PYTHONPATH`, Git config/askpass controls, Rust compiler wrappers/flags, and `SSLKEYLOGFILE`.

Use receiver-specific names for sink credentials/configuration, such as OTLP or a dedicated log-sink credential. Do not pass worker credentials to the receiver and do not put credentials in receiver argv. Receiver argv is an explicit argument array and is never shell-evaluated.

## Backend routing

The FD3/FD4 contract is storage-vendor neutral. A receiver may transform admitted frames into OTLP or another bounded internal representation and forward them to `ores-otel`, an OpenTelemetry Collector, Loki, ClickHouse, Supabase-backed indexing, or another user-selected sink. Storage failure must remain downstream of the worker's primary-output boundary.

For centralized GitHub indexing, preserve at least GitHub organization, repository, worker job ID, stream, sequence, timestamp, and any available workflow/run/job/step identity. Never put credentials or secret values into metadata labels/object keys.

## Legacy compatibility protocol

`gha-indie-worker.log-sidecar.v1` is still accepted for older callers. In that mode:

- stdin / FD 0 carries versioned `GHLG` binary frames;
- FD 3 carries newline-delimited command-lifecycle metadata;
- stdout/stderr are receiver-owned output surfaces.

New implementations must not use this legacy framing as the contract for IndieBuild build-log collection. Migrate to `INDIEBUILD_LOG_PROTOCOL=gha-indie-worker.build-log-metadata/v1` and the separate FD3/FD4 metadata/data lanes.

## Portability

The current FD transport is Unix-specific. Windows support must use an explicitly versioned named-pipe or inherited-handle adapter while preserving the same logical metadata schema, binary data semantics, bounded queue/drop accounting, failure isolation, and one-total-budget eight-second shutdown rule. Until that implementation is certified, the Unix FD mode fails closed on unsupported platforms rather than pretending descriptors are portable.
