use std::{
    env,
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    thread,
};

use gha_indie_worker_interfaces::{
    BuildLogEvent, BuildLogMetadata, BuildLogStream, BUILD_LOG_METADATA_SCHEMA_VERSION,
    DEFAULT_DATA_FD, DEFAULT_METADATA_FD,
};

pub(crate) const PROTOCOL: &str = "gha-indie-worker.log-sidecar.v1";
pub(crate) const BUILD_LOG_PROTOCOL: &str = BUILD_LOG_METADATA_SCHEMA_VERSION;
const FRAME_MAGIC: &[u8; 4] = b"GHLG";
const FRAME_VERSION: u8 = 1;
const STREAM_STDOUT: u8 = 1;
const STREAM_STDERR: u8 = 2;
const FRAME_HEADER_BYTES: usize = 10;
const MAX_FRAME_BYTES: usize = 64 * 1024;
const MAX_METADATA_LINE_BYTES: usize = 16 * 1024;
const MAX_INHERITED_FD: u32 = 1024;

#[derive(Debug, Default, PartialEq, Eq)]
struct CopyStats {
    frames: u64,
    bytes: u64,
}

pub(crate) fn run_if_requested() -> io::Result<bool> {
    if env::var("INDIEBUILD_LOG_PROTOCOL").ok().as_deref() == Some(BUILD_LOG_PROTOCOL) {
        return run_build_log_contract();
    }
    if env::var("GHAIW_LOG_SIDECAR_PROTOCOL").ok().as_deref() != Some(PROTOCOL) {
        return Ok(false);
    }
    run_legacy_protocol()
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn parse_fd(name: &str, default_fd: i32) -> io::Result<u32> {
    let default_fd = u32::try_from(default_fd).map_err(|_| invalid_data("invalid default fd"))?;
    let fd = env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| invalid_data("invalid inherited descriptor"))
        })
        .transpose()?
        .unwrap_or(default_fd);
    if !(3..=MAX_INHERITED_FD).contains(&fd) {
        return Err(invalid_data(
            "inherited descriptor is outside the allowed range",
        ));
    }
    Ok(fd)
}

fn run_build_log_contract() -> io::Result<bool> {
    #[cfg(not(unix))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "dual build-log descriptors are not supported on this platform",
        ))
    }

    #[cfg(unix)]
    {
        let metadata_fd = parse_fd("INDIEBUILD_LOG_METADATA_FD", DEFAULT_METADATA_FD)?;
        let data_fd = parse_fd("INDIEBUILD_LOG_DATA_FD", DEFAULT_DATA_FD)?;
        if metadata_fd == data_fd {
            return Err(invalid_data("metadata and data descriptors must differ"));
        }
        let metadata = File::open(format!("/dev/fd/{metadata_fd}"))?;
        let data = File::open(format!("/dev/fd/{data_fd}"))?;
        copy_contract_streams(metadata, data, io::stdout(), io::stderr()).map(|_| true)
    }
}

fn run_legacy_protocol() -> io::Result<bool> {
    #[cfg(not(unix))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "FD 3 metadata transport is not supported on this platform",
        ))
    }

    #[cfg(unix)]
    {
        let metadata_fd = parse_fd("GHAIW_LOG_SIDECAR_METADATA_FD", 3)?;
        let metadata = File::open(format!("/dev/fd/{metadata_fd}"))?;
        let metadata_thread = thread::spawn(move || copy_metadata(metadata, io::stderr()));

        let result = copy_frames(io::stdin(), io::stdout(), io::stderr());
        match metadata_thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(io::Error::other("metadata reader thread panicked")),
        }
        result.map(|_| true)
    }
}

fn copy_metadata<R, W>(reader: R, mut writer: W) -> io::Result<()>
where
    R: Read,
    W: Write,
{
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(());
        }
        writer.write_all(b"[ghaiw-meta] ")?;
        writer.write_all(&line)?;
        if !line.ends_with(b"\n") {
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
    }
}

fn copy_contract_streams<M, D, O, E>(
    metadata_reader: M,
    mut data_reader: D,
    mut stdout: O,
    mut stderr: E,
) -> io::Result<CopyStats>
where
    M: Read,
    D: Read,
    O: Write,
    E: Write,
{
    let mut metadata_reader = BufReader::new(metadata_reader);
    let mut line = Vec::new();
    let mut stats = CopyStats::default();
    loop {
        line.clear();
        let read = metadata_reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(stats);
        }
        if line.len() > MAX_METADATA_LINE_BYTES {
            return Err(invalid_data("build-log metadata line exceeds maximum size"));
        }
        if !line.ends_with(b"\n") {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated build-log metadata line",
            ));
        }
        let text = std::str::from_utf8(&line)
            .map_err(|_| invalid_data("build-log metadata must be UTF-8 JSON"))?;
        let metadata = BuildLogMetadata::parse_json_line(text)
            .map_err(|_| invalid_data("build-log metadata failed contract validation"))?;

        // FD3 is a control-plane lane. Do not echo metadata into stdout/stderr:
        // those output lanes must contain only the exact bytes read from FD4.
        match metadata.event {
            BuildLogEvent::Chunk => {
                if metadata.stream == BuildLogStream::Worker {
                    return Err(invalid_data("worker stream cannot carry raw log chunks"));
                }
                let frame_len = usize::try_from(metadata.byte_length)
                    .map_err(|_| invalid_data("invalid build-log byte length"))?;
                if frame_len == 0 || frame_len > MAX_FRAME_BYTES {
                    return Err(invalid_data("build-log chunk exceeds receiver bounds"));
                }
                let mut payload = vec![0_u8; frame_len];
                data_reader.read_exact(&mut payload)?;
                match metadata.stream {
                    BuildLogStream::Stdout => {
                        stdout.write_all(&payload)?;
                        stdout.flush()?;
                    }
                    BuildLogStream::Stderr => {
                        stderr.write_all(&payload)?;
                        stderr.flush()?;
                    }
                    BuildLogStream::Worker => unreachable!(),
                }
                stats.frames = stats.frames.saturating_add(1);
                stats.bytes = stats.bytes.saturating_add(frame_len as u64);
            }
            BuildLogEvent::Dropped => {
                if metadata.stream != BuildLogStream::Worker || metadata.byte_length != 0 {
                    return Err(invalid_data("invalid build-log drop receipt"));
                }
            }
            BuildLogEvent::ReceiverClosed | BuildLogEvent::StreamClosed => {
                if metadata.byte_length != 0 {
                    return Err(invalid_data("lifecycle event cannot claim raw data bytes"));
                }
            }
        }
    }
}

fn copy_frames<R, O, E>(mut reader: R, mut stdout: O, mut stderr: E) -> io::Result<CopyStats>
where
    R: Read,
    O: Write,
    E: Write,
{
    let mut stats = CopyStats::default();
    loop {
        let mut header = [0_u8; FRAME_HEADER_BYTES];
        if !read_exact_or_eof(&mut reader, &mut header)? {
            return Ok(stats);
        }
        if &header[..4] != FRAME_MAGIC {
            return Err(invalid_data("invalid log-sidecar frame magic"));
        }
        if header[4] != FRAME_VERSION {
            return Err(invalid_data("unsupported log-sidecar frame version"));
        }
        let stream = header[5];
        if !matches!(stream, STREAM_STDOUT | STREAM_STDERR) {
            return Err(invalid_data("invalid log-sidecar stream id"));
        }
        let frame_len =
            u32::from_be_bytes(header[6..10].try_into().expect("fixed header")) as usize;
        if frame_len > MAX_FRAME_BYTES {
            return Err(invalid_data("log-sidecar frame exceeds maximum size"));
        }
        let mut payload = vec![0_u8; frame_len];
        reader.read_exact(&mut payload)?;
        match stream {
            STREAM_STDOUT => {
                stdout.write_all(&payload)?;
                stdout.flush()?;
            }
            STREAM_STDERR => {
                stderr.write_all(&payload)?;
                stderr.flush()?;
            }
            _ => unreachable!(),
        }
        stats.frames = stats.frames.saturating_add(1);
        stats.bytes = stats.bytes.saturating_add(frame_len as u64);
    }
}

fn read_exact_or_eof<R: Read>(reader: &mut R, buffer: &mut [u8]) -> io::Result<bool> {
    let mut offset = 0;
    while offset < buffer.len() {
        match reader.read(&mut buffer[offset..])? {
            0 if offset == 0 => return Ok(false),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated log-sidecar frame header",
                ));
            }
            read => offset += read,
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(stream: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(FRAME_MAGIC);
        bytes.push(FRAME_VERSION);
        bytes.push(stream);
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn contract_line(event: &str, stream: &str, sequence: u32, byte_length: u32) -> String {
        format!(
            "{{\"schemaVersion\":\"{BUILD_LOG_PROTOCOL}\",\"event\":\"{event}\",\"jobId\":\"build-1\",\"stream\":\"{stream}\",\"sequence\":{sequence},\"byteLength\":{byte_length},\"timestamp\":\"2026-09-09T19:45:00Z\"}}\n"
        )
    }

    #[test]
    fn routes_contract_stdout_and_stderr_without_text_decoding() {
        let mut metadata = contract_line("chunk", "stdout", 1, 6);
        metadata.push_str(&contract_line("chunk", "stderr", 1, 3));
        metadata.push_str(&contract_line("stream_closed", "stdout", 2, 0));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let stats = copy_contract_streams(
            Cursor::new(metadata.into_bytes()),
            Cursor::new([b"hello\n".as_slice(), &[0xff, 0x00, b'\n']].concat()),
            &mut stdout,
            &mut stderr,
        )
        .unwrap();

        assert_eq!(stdout, b"hello\n");
        assert_eq!(stderr, vec![0xff, 0x00, b'\n']);
        assert_eq!(
            stats,
            CopyStats {
                frames: 2,
                bytes: 9
            }
        );
    }

    #[test]
    fn contract_rejects_unknown_or_credential_fields() {
        let credential = format!(
            "{{\"schemaVersion\":\"{BUILD_LOG_PROTOCOL}\",\"event\":\"chunk\",\"jobId\":\"build-1\",\"stream\":\"stdout\",\"sequence\":1,\"byteLength\":1,\"timestamp\":\"2026-09-09T19:45:00Z\",\"accessToken\":\"forbidden\"}}\n"
        );
        assert_eq!(
            copy_contract_streams(
                Cursor::new(credential.into_bytes()),
                Cursor::new(b"x"),
                Vec::new(),
                Vec::new()
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn contract_rejects_wrong_version_and_oversized_chunk() {
        let wrong_version = "{\"schemaVersion\":\"gha-indie-worker.build-log-metadata/v0\",\"event\":\"chunk\",\"jobId\":\"build-1\",\"stream\":\"stdout\",\"sequence\":1,\"byteLength\":1,\"timestamp\":\"2026-09-09T19:45:00Z\"}\n";
        assert_eq!(
            copy_contract_streams(
                Cursor::new(wrong_version.as_bytes()),
                Cursor::new(b"x"),
                Vec::new(),
                Vec::new()
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );

        let oversized = contract_line("chunk", "stdout", 1, (MAX_FRAME_BYTES as u32) + 1);
        assert_eq!(
            copy_contract_streams(
                Cursor::new(oversized.into_bytes()),
                Cursor::new(Vec::<u8>::new()),
                Vec::new(),
                Vec::new()
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn contract_rejects_truncated_raw_data_and_metadata() {
        let metadata = contract_line("chunk", "stdout", 1, 5);
        assert_eq!(
            copy_contract_streams(
                Cursor::new(metadata.into_bytes()),
                Cursor::new(b"abc"),
                Vec::new(),
                Vec::new()
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::UnexpectedEof
        );

        let truncated_metadata = contract_line("stream_closed", "stdout", 2, 0)
            .trim_end_matches('\n')
            .as_bytes()
            .to_vec();
        assert_eq!(
            copy_contract_streams(
                Cursor::new(truncated_metadata),
                Cursor::new(Vec::<u8>::new()),
                Vec::new(),
                Vec::new()
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn contract_requires_worker_drop_receipts_and_zero_lifecycle_bytes() {
        let invalid_drop = contract_line("dropped", "stdout", 1, 0);
        assert_eq!(
            copy_contract_streams(
                Cursor::new(invalid_drop.into_bytes()),
                Cursor::new(Vec::<u8>::new()),
                Vec::new(),
                Vec::new()
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );

        let lifecycle_with_data = contract_line("stream_closed", "stdout", 2, 1);
        assert_eq!(
            copy_contract_streams(
                Cursor::new(lifecycle_with_data.into_bytes()),
                Cursor::new(b"x"),
                Vec::new(),
                Vec::new()
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn routes_legacy_stdout_and_stderr_frames_without_text_decoding() {
        let mut input = frame(STREAM_STDOUT, b"hello\n");
        input.extend(frame(STREAM_STDERR, &[0xff, 0x00, b'\n']));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let stats = copy_frames(Cursor::new(input), &mut stdout, &mut stderr).unwrap();

        assert_eq!(stdout, b"hello\n");
        assert_eq!(stderr, vec![0xff, 0x00, b'\n']);
        assert_eq!(
            stats,
            CopyStats {
                frames: 2,
                bytes: 9
            }
        );
    }

    #[test]
    fn rejects_oversized_and_truncated_legacy_frames() {
        let mut oversized = Vec::new();
        oversized.extend_from_slice(FRAME_MAGIC);
        oversized.push(FRAME_VERSION);
        oversized.push(STREAM_STDOUT);
        oversized.extend_from_slice(&((MAX_FRAME_BYTES as u32) + 1).to_be_bytes());
        assert_eq!(
            copy_frames(Cursor::new(oversized), Vec::new(), Vec::new())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let truncated = b"GHLG\x01".to_vec();
        assert_eq!(
            copy_frames(Cursor::new(truncated), Vec::new(), Vec::new())
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn prefixes_legacy_metadata_without_rewriting_json() {
        let mut output = Vec::new();
        copy_metadata(
            Cursor::new(b"{\"event\":\"command_started\"}\n"),
            &mut output,
        )
        .unwrap();
        assert_eq!(output, b"[ghaiw-meta] {\"event\":\"command_started\"}\n");
    }
}
