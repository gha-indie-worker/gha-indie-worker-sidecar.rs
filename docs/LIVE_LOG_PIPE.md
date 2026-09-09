# Live log pipe receiver

`ghaiw-sidecar` implements the executable receiver half of `gha-indie-worker.log-sidecar.v1`. It complements the crate's generic `log_tee` library: `log_tee` defines the bounded, fail-open tee semantics; this adapter defines how an external process receives those events.

When the worker starts this binary with `GHAIW_LOG_SIDECAR_PROTOCOL=gha-indie-worker.log-sidecar.v1`, the process switches from HTTP service mode into one-command pipe mode.

## Channels

On Unix:

- stdin / FD 0 receives versioned binary build-log frames;
- FD 3 receives newline-delimited JSON command-lifecycle metadata;
- stdout and stderr are receiver-owned output/diagnostic surfaces;
- EOF closes the receiver cleanly.

The worker also sets `GHAIW_LOG_SIDECAR_METADATA_FD=3` and, when available, `GHAIW_LOG_SIDECAR_JOB_ID`.

## Data frame

Each stdin frame is:

| Bytes | Field |
| ---: | --- |
| 0..4 | ASCII magic `GHLG` |
| 4 | protocol frame version (`1`) |
| 5 | stream (`1` = stdout, `2` = stderr) |
| 6..10 | payload length as unsigned 32-bit big-endian |
| 10.. | raw payload bytes |

Payloads are binary-safe; receivers must not assume UTF-8. The reference decoder rejects malformed magic/version/stream identifiers, truncated input, and frames larger than 64 KiB.

## Metadata

FD 3 carries one JSON object per line. Current lifecycle events include `command_started`, `command_finished`, `command_timed_out`, and `command_wait_failed`. The worker keeps metadata bounded and excludes command arguments and ambient environment values.

## Failure semantics

The process pipe must preserve the same guarantees as `log_tee::tee_stream_decoupled`:

- normal/native build stdout and stderr remain authoritative;
- receiver delivery is bounded and fail-open;
- queue saturation drops export copies rather than slowing the build;
- receiver failure cannot change the build result;
- receiver drain/termination cannot extend teardown beyond eight seconds.

The worker clears the receiver environment and passes only explicitly allowlisted variables. Receiver argv is supplied as an argv array and is never shell-evaluated.

## Portability

FD 3 is a Unix transport. Windows should eventually use a named-pipe or inherited-handle adapter while retaining the same logical v1 event schema and bounded shutdown/backpressure semantics. Until then, this exact pipe mode fails closed on unsupported platforms rather than pretending file descriptors are portable.
