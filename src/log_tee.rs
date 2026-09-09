use std::io::{self, Read, Write};

/// Keep source reads small enough that a redactor can expand a chunk without making
/// exporter memory unbounded.
pub const MAX_READ_CHUNK_BYTES: usize = 8 * 1024;
/// Hard ceiling for the redacted/export copy. The native GitHub log copy is never
/// truncated by this limit.
pub const MAX_EXPORT_CHUNK_BYTES: usize = 16 * 1024;

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TeeReport {
    pub mirrored_bytes: u64,
    pub export_attempts: u64,
    pub export_failures: u64,
    pub truncated_exports: u64,
}

/// Mirrors every source byte to the caller-owned GitHub output before attempting export.
///
/// The `redact` callback prepares a separate copy for the central-log exporter and returns
/// whether it changed the payload. The exporter never receives the original input buffer.
/// Export failures are counted and fail open so observability cannot deadlock or fail a build.
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
    let mut sequence = 0_u32;

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            mirror.flush()?;
            return Ok(report);
        }

        let input = &buffer[..read];
        mirror.write_all(input)?;
        mirror.flush()?;
        report.mirrored_bytes = report
            .mirrored_bytes
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("mirrored byte counter overflow"))?;

        prepared.clear();
        let redacted = redact(input, &mut prepared);
        let truncated = prepared.len() > MAX_EXPORT_CHUNK_BYTES;
        if truncated {
            prepared.truncate(MAX_EXPORT_CHUNK_BYTES);
            report.truncated_exports = report
                .truncated_exports
                .checked_add(1)
                .ok_or_else(|| io::Error::other("truncated export counter overflow"))?;
        }

        report.export_attempts = report
            .export_attempts
            .checked_add(1)
            .ok_or_else(|| io::Error::other("export attempt counter overflow"))?;

        if export(ExportChunk {
            stream,
            sequence,
            bytes: &prepared,
            redacted,
            truncated,
        })
        .is_err()
        {
            report.export_failures = report
                .export_failures
                .checked_add(1)
                .ok_or_else(|| io::Error::other("export failure counter overflow"))?;
        }

        sequence = sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("log sequence overflow"))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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
        assert_eq!(
            observed,
            Some((MAX_EXPORT_CHUNK_BYTES, true, true))
        );
        assert_eq!(report.truncated_exports, 1);
    }
}
