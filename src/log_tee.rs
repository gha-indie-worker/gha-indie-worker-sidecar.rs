use std::io::{self, Read, Write};
use std::sync::mpsc::{self, sync_channel, RecvTimeoutError, TrySendError};
use std::thread;
use std::time::Duration;

/// Keep source reads small enough that a redactor can expand a chunk without making
/// exporter memory unbounded.
pub const MAX_READ_CHUNK_BYTES: usize = 8 * 1024;
/// Hard ceiling for the redacted/export copy. The native GitHub log copy is never
/// truncated by this limit.
pub const MAX_EXPORT_CHUNK_BYTES: usize = 16 * 1024;
/// Default number of prepared log chunks that may wait for a receiver. Once this
/// queue is full, export copies are dropped rather than applying backpressure to
/// GitHub's native stdout/stderr stream.
pub const DEFAULT_EXPORT_QUEUE_CAPACITY: usize = 64;
/// The receiver gets at most eight additional seconds to drain after the native
/// stream reaches EOF. A wedged receiver is detached after this deadline.
pub const MAX_EXPORT_DRAIN: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

impl LogStream {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportChunk<'a> {
    pub stream: LogStream,
    pub sequence: u32,
    pub bytes: &'a [u8],
    pub redacted: bool,
    pub truncated: bool,
}

/// Owned form used by the decoupled receiver thread. Ownership is intentional:
/// the native output path must be able to continue reading while a receiver is
/// still processing an earlier chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedExportChunk {
    pub stream: LogStream,
    pub sequence: u32,
    pub bytes: Vec<u8>,
    pub redacted: bool,
    pub truncated: bool,
}

impl OwnedExportChunk {
    pub fn as_borrowed(&self) -> ExportChunk<'_> {
        ExportChunk {
            stream: self.stream,
            sequence: self.sequence,
            bytes: &self.bytes,
            redacted: self.redacted,
            truncated: self.truncated,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoupledTeeConfig {
    pub queue_capacity: usize,
    pub drain_timeout: Duration,
}

impl Default for DecoupledTeeConfig {
    fn default() -> Self {
        Self {
            queue_capacity: DEFAULT_EXPORT_QUEUE_CAPACITY,
            drain_timeout: MAX_EXPORT_DRAIN,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TeeReport {
    pub mirrored_bytes: u64,
    /// Chunks accepted by the export path (directly for `tee_stream`, or by the
    /// bounded queue for `tee_stream_decoupled`).
    pub export_enqueued: u64,
    /// Receiver calls known to have started. On a drain timeout this is a lower
    /// bound because the detached receiver may still be inside its final call.
    pub export_attempts: u64,
    pub export_failures: u64,
    pub dropped_exports: u64,
    pub truncated_exports: u64,
    pub exporter_disconnected: bool,
    pub exporter_panicked: bool,
    pub shutdown_timed_out: bool,
    /// Once the u32 wire sequence domain is exhausted, export is disabled while
    /// native stdout/stderr mirroring continues.
    pub sequence_exhausted: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ExporterOutcome {
    attempts: u64,
    failures: u64,
}

fn next_sequence(sequence: &mut Option<u32>) -> Option<u32> {
    let current = (*sequence)?;
    *sequence = current.checked_add(1);
    Some(current)
}

fn prepare_chunk<F>(
    input: &[u8],
    stream: LogStream,
    sequence: u32,
    prepared: &mut Vec<u8>,
    redact: &mut F,
) -> (OwnedExportChunk, bool)
where
    F: FnMut(&[u8], &mut Vec<u8>) -> bool,
{
    prepared.clear();
    let redacted = redact(input, prepared);
    let truncated = prepared.len() > MAX_EXPORT_CHUNK_BYTES;
    if truncated {
        prepared.truncate(MAX_EXPORT_CHUNK_BYTES);
    }

    (
        OwnedExportChunk {
            stream,
            sequence,
            bytes: prepared.clone(),
            redacted,
            truncated,
        },
        truncated,
    )
}

/// Mirrors every source byte to the caller-owned GitHub output before attempting export.
///
/// This synchronous primitive is useful for tests and receivers that are already known
/// to be non-blocking. Worker/process integrations should use `tee_stream_decoupled` so a
/// slow or wedged sidecar cannot apply backpressure to GitHub's native log stream.
///
/// The `redact` callback prepares a separate copy for the central-log exporter and returns
/// whether it changed the payload. The exporter never receives the original input buffer.
/// Export failures are counted and fail open so observability cannot fail a build.
/// Read/mirror failures remain fatal because losing GitHub's native log stream is unacceptable.
pub fn tee_stream<R, W, F, E>(
    reader: &mut R,
    mirror: &mut W,
    stream: LogStream,
    mut redact: F,
    mut export: E,
) -> io::Result<TeeReport>
where
    R: Read,
    W: Write,
    F: FnMut(&[u8], &mut Vec<u8>) -> bool,
    E: FnMut(ExportChunk<'_>) -> Result<(), ()>,
{
    let mut report = TeeReport::default();
    let mut buffer = [0_u8; MAX_READ_CHUNK_BYTES];
    let mut prepared = Vec::with_capacity(MAX_READ_CHUNK_BYTES);
    let mut sequence = Some(0_u32);

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            mirror.flush()?;
            return Ok(report);
        }

        let input = &buffer[..read];
        mirror.write_all(input)?;
        mirror.flush()?;
        report.mirrored_bytes = report.mirrored_bytes.saturating_add(read as u64);

        let Some(current_sequence) = next_sequence(&mut sequence) else {
            report.sequence_exhausted = true;
            report.dropped_exports = report.dropped_exports.saturating_add(1);
            continue;
        };

        let (owned, truncated) =
            prepare_chunk(input, stream, current_sequence, &mut prepared, &mut redact);
        if truncated {
            report.truncated_exports = report.truncated_exports.saturating_add(1);
        }

        report.export_enqueued = report.export_enqueued.saturating_add(1);
        report.export_attempts = report.export_attempts.saturating_add(1);
        if export(owned.as_borrowed()).is_err() {
            report.export_failures = report.export_failures.saturating_add(1);
        }
    }
}

/// Mirrors stdout/stderr synchronously while exporting a separate redacted copy through
/// a bounded queue on a receiver thread.
///
/// The native mirror is always written and flushed *before* any redaction or queue work.
/// Queue saturation, receiver failure, receiver panic, sequence exhaustion, and shutdown
/// timeout are observability failures only: they are recorded in `TeeReport` while the
/// native stream continues. Only source read and native mirror write/flush failures are
/// returned as `io::Error`.
///
/// At EOF the queue is closed and the receiver is allowed `config.drain_timeout` to finish.
/// The production default is `MAX_EXPORT_DRAIN` (eight seconds). A receiver that does not
/// finish by then is detached so its shutdown cannot hold up the build indefinitely.
pub fn tee_stream_decoupled<R, W, F, E>(
    reader: &mut R,
    mirror: &mut W,
    stream: LogStream,
    config: DecoupledTeeConfig,
    mut redact: F,
    mut export: E,
) -> io::Result<TeeReport>
where
    R: Read,
    W: Write,
    F: FnMut(&[u8], &mut Vec<u8>) -> bool,
    E: FnMut(OwnedExportChunk) -> Result<(), ()> + Send + 'static,
{
    if config.queue_capacity == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "export queue capacity must be greater than zero",
        ));
    }

    let (queue_tx, queue_rx) = sync_channel::<OwnedExportChunk>(config.queue_capacity);
    let (done_tx, done_rx) = mpsc::channel::<ExporterOutcome>();
    let worker = thread::Builder::new()
        .name("ghaiw-log-export".to_owned())
        .spawn(move || {
            let mut outcome = ExporterOutcome::default();
            while let Ok(chunk) = queue_rx.recv() {
                outcome.attempts = outcome.attempts.saturating_add(1);
                if export(chunk).is_err() {
                    outcome.failures = outcome.failures.saturating_add(1);
                }
            }
            let _ = done_tx.send(outcome);
        })?;

    let mut report = TeeReport::default();
    let mut buffer = [0_u8; MAX_READ_CHUNK_BYTES];
    let mut prepared = Vec::with_capacity(MAX_READ_CHUNK_BYTES);
    let mut sequence = Some(0_u32);
    let mut export_enabled = true;

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            mirror.flush()?;
            break;
        }

        let input = &buffer[..read];
        mirror.write_all(input)?;
        mirror.flush()?;
        report.mirrored_bytes = report.mirrored_bytes.saturating_add(read as u64);

        if !export_enabled {
            report.dropped_exports = report.dropped_exports.saturating_add(1);
            continue;
        }

        let Some(current_sequence) = next_sequence(&mut sequence) else {
            report.sequence_exhausted = true;
            report.dropped_exports = report.dropped_exports.saturating_add(1);
            export_enabled = false;
            continue;
        };

        let (owned, truncated) =
            prepare_chunk(input, stream, current_sequence, &mut prepared, &mut redact);
        if truncated {
            report.truncated_exports = report.truncated_exports.saturating_add(1);
        }

        match queue_tx.try_send(owned) {
            Ok(()) => {
                report.export_enqueued = report.export_enqueued.saturating_add(1);
            }
            Err(TrySendError::Full(_)) => {
                report.dropped_exports = report.dropped_exports.saturating_add(1);
            }
            Err(TrySendError::Disconnected(_)) => {
                report.exporter_disconnected = true;
                report.dropped_exports = report.dropped_exports.saturating_add(1);
                export_enabled = false;
            }
        }
    }

    drop(queue_tx);
    match done_rx.recv_timeout(config.drain_timeout) {
        Ok(outcome) => {
            report.export_attempts = outcome.attempts;
            report.export_failures = outcome.failures;
            if worker.join().is_err() {
                report.exporter_panicked = true;
                report.exporter_disconnected = true;
            }
        }
        Err(RecvTimeoutError::Timeout) => {
            report.shutdown_timed_out = true;
            // Dropping JoinHandle deliberately detaches the receiver. The process may now
            // complete without waiting for a user-defined sidecar/export implementation.
            drop(worker);
        }
        Err(RecvTimeoutError::Disconnected) => {
            report.exporter_disconnected = true;
            if worker.join().is_err() {
                report.exporter_panicked = true;
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    fn copy_redactor(input: &[u8], out: &mut Vec<u8>) -> bool {
        out.extend_from_slice(input);
        false
    }

    #[test]
    fn stream_names_match_cross_runtime_contract() {
        assert_eq!(LogStream::Stdout.as_str(), "stdout");
        assert_eq!(LogStream::Stderr.as_str(), "stderr");
    }

    #[test]
    fn production_drain_deadline_is_eight_seconds() {
        assert_eq!(
            DecoupledTeeConfig::default().drain_timeout,
            Duration::from_secs(8)
        );
    }

    #[test]
    fn preserves_original_bytes_while_exporting_redacted_copy() {
        let input = b"before secret after\n";
        let mut reader = Cursor::new(input);
        let mut mirror = Vec::new();
        let mut exported = Vec::new();

        let report = tee_stream(
            &mut reader,
            &mut mirror,
            LogStream::Stdout,
            |chunk, out| {
                let text = String::from_utf8_lossy(chunk).replace("secret", "[REDACTED]");
                out.extend_from_slice(text.as_bytes());
                text.as_bytes() != chunk
            },
            |chunk| {
                exported.push((
                    chunk.stream,
                    chunk.sequence,
                    chunk.bytes.to_vec(),
                    chunk.redacted,
                    chunk.truncated,
                ));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(mirror, input);
        assert_eq!(
            exported,
            vec![(
                LogStream::Stdout,
                0,
                b"before [REDACTED] after\n".to_vec(),
                true,
                false,
            )]
        );
        assert_eq!(report.mirrored_bytes, input.len() as u64);
        assert_eq!(report.export_attempts, 1);
        assert_eq!(report.export_failures, 0);
    }

    #[test]
    fn exporter_failure_is_fail_open_for_native_log_stream() {
        let input = b"build output stays visible\n";
        let mut reader = Cursor::new(input);
        let mut mirror = Vec::new();

        let report = tee_stream(
            &mut reader,
            &mut mirror,
            LogStream::Stderr,
            copy_redactor,
            |_chunk| Err(()),
        )
        .unwrap();

        assert_eq!(mirror, input);
        assert_eq!(report.export_attempts, 1);
        assert_eq!(report.export_failures, 1);
    }

    #[test]
    fn bounds_export_chunks_and_keeps_monotonic_per_stream_sequence() {
        let input = vec![b'x'; MAX_READ_CHUNK_BYTES * 2 + 3];
        let mut reader = Cursor::new(input.clone());
        let mut mirror = Vec::new();
        let mut seen = Vec::new();

        let report = tee_stream(
            &mut reader,
            &mut mirror,
            LogStream::Stdout,
            copy_redactor,
            |chunk| {
                seen.push((chunk.sequence, chunk.bytes.len(), chunk.truncated));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(mirror, input);
        assert_eq!(
            seen,
            vec![
                (0, MAX_READ_CHUNK_BYTES, false),
                (1, MAX_READ_CHUNK_BYTES, false),
                (2, 3, false),
            ]
        );
        assert_eq!(report.export_attempts, 3);
    }

    #[test]
    fn truncates_redactor_expansion_before_export() {
        let input = b"x";
        let mut reader = Cursor::new(input);
        let mut mirror = Vec::new();
        let mut observed = None;

        let report = tee_stream(
            &mut reader,
            &mut mirror,
            LogStream::Stdout,
            |_chunk, out| {
                out.resize(MAX_EXPORT_CHUNK_BYTES + 50, b'z');
                true
            },
            |chunk| {
                observed = Some((chunk.bytes.len(), chunk.redacted, chunk.truncated));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(mirror, input);
        assert_eq!(observed, Some((MAX_EXPORT_CHUNK_BYTES, true, true)));
        assert_eq!(report.truncated_exports, 1);
    }

    #[test]
    fn decoupled_receiver_preserves_order_when_it_keeps_up() {
        let input = vec![b'q'; MAX_READ_CHUNK_BYTES * 2 + 3];
        let mut reader = Cursor::new(input.clone());
        let mut mirror = Vec::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let receiver_seen = Arc::clone(&seen);

        let report = tee_stream_decoupled(
            &mut reader,
            &mut mirror,
            LogStream::Stdout,
            DecoupledTeeConfig {
                queue_capacity: 8,
                drain_timeout: Duration::from_secs(1),
            },
            copy_redactor,
            move |chunk| {
                receiver_seen.lock().unwrap().push(chunk.sequence);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(mirror, input);
        assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2]);
        assert_eq!(report.export_enqueued, 3);
        assert_eq!(report.export_attempts, 3);
        assert_eq!(report.dropped_exports, 0);
        assert!(!report.shutdown_timed_out);
    }

    #[test]
    fn slow_receiver_cannot_backpressure_native_stream_and_drain_is_bounded() {
        let input = vec![b's'; MAX_READ_CHUNK_BYTES * 5];
        let mut reader = Cursor::new(input.clone());
        let mut mirror = Vec::new();

        let report = tee_stream_decoupled(
            &mut reader,
            &mut mirror,
            LogStream::Stderr,
            DecoupledTeeConfig {
                queue_capacity: 1,
                drain_timeout: Duration::from_millis(10),
            },
            copy_redactor,
            |_chunk| {
                thread::sleep(Duration::from_millis(250));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(mirror, input);
        assert!(report.export_enqueued >= 1);
        assert!(report.dropped_exports >= 1);
        assert!(report.shutdown_timed_out);
    }

    #[test]
    fn receiver_panic_is_fail_open_and_reported() {
        let input = vec![b'p'; MAX_READ_CHUNK_BYTES * 2];
        let mut reader = Cursor::new(input.clone());
        let mut mirror = Vec::new();

        let report = tee_stream_decoupled(
            &mut reader,
            &mut mirror,
            LogStream::Stdout,
            DecoupledTeeConfig {
                queue_capacity: 4,
                drain_timeout: Duration::from_secs(1),
            },
            copy_redactor,
            |_chunk| -> Result<(), ()> { panic!("simulated receiver crash") },
        )
        .unwrap();

        assert_eq!(mirror, input);
        assert!(report.exporter_disconnected);
        assert!(report.exporter_panicked);
    }

    #[test]
    fn zero_capacity_is_rejected_before_reading_native_stream() {
        let input = b"not consumed";
        let mut reader = Cursor::new(input);
        let mut mirror = Vec::new();

        let error = tee_stream_decoupled(
            &mut reader,
            &mut mirror,
            LogStream::Stdout,
            DecoupledTeeConfig {
                queue_capacity: 0,
                drain_timeout: Duration::from_secs(1),
            },
            copy_redactor,
            |_chunk| Ok(()),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(mirror.is_empty());
    }

    #[test]
    fn sequence_exhaustion_disables_export_without_overflowing() {
        let mut sequence = Some(u32::MAX);
        assert_eq!(next_sequence(&mut sequence), Some(u32::MAX));
        assert_eq!(sequence, None);
        assert_eq!(next_sequence(&mut sequence), None);
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "native mirror failed",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn native_mirror_failure_remains_fatal() {
        let mut reader = Cursor::new(b"must reach GitHub");
        let mut mirror = FailingWriter;

        let error = tee_stream_decoupled(
            &mut reader,
            &mut mirror,
            LogStream::Stdout,
            DecoupledTeeConfig::default(),
            copy_redactor,
            |_chunk| Ok(()),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
