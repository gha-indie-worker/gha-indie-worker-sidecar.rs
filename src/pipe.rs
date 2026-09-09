use std::{
    env,
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    thread,
};

pub(crate) const PROTOCOL: &str = "gha-indie-worker.log-sidecar.v1";
const FRAME_MAGIC: &[u8; 4] = b"GHLG";
const FRAME_VERSION: u8 = 1;
const STREAM_STDOUT: u8 = 1;
const STREAM_STDERR: u8 = 2;
const FRAME_HEADER_BYTES: usize = 10;
const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
struct CopyStats {
    frames: u64,
    bytes: u64,
}

pub(crate) fn run_if_requested() -> io::Result<bool> {
    if env::var("GHAIW_LOG_SIDECAR_PROTOCOL").ok().as_deref() != Some(PROTOCOL) {
        return Ok(false);
    }

    #[cfg(not(unix))]
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "FD 3 metadata transport is not supported on this platform",
        ));
    }

    #[cfg(unix)]
    {
        let metadata_fd = env::var("GHAIW_LOG_SIDECAR_METADATA_FD")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|fd| (3..=1024).contains(fd))
            .unwrap_or(3);
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
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid log-sidecar frame magic",
            ));
        }
        if header[4] != FRAME_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported log-sidecar frame version",
            ));
        }
        let stream = header[5];
        if !matches!(stream, STREAM_STDOUT | STREAM_STDERR) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid log-sidecar stream id",
            ));
        }
        let frame_len = u32::from_be_bytes(header[6..10].try_into().expect("fixed header")) as usize;
        if frame_len > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "log-sidecar frame exceeds maximum size",
            ));
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
        stats.frames += 1;
        stats.bytes += frame_len as u64;
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

    #[test]
    fn routes_stdout_and_stderr_frames_without_text_decoding() {
        let mut input = frame(STREAM_STDOUT, b"hello\n");
        input.extend(frame(STREAM_STDERR, &[0xff, 0x00, b'\n']));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let stats = copy_frames(Cursor::new(input), &mut stdout, &mut stderr).unwrap();

        assert_eq!(stdout, b"hello\n");
        assert_eq!(stderr, vec![0xff, 0x00, b'\n']);
        assert_eq!(stats, CopyStats { frames: 2, bytes: 9 });
    }

    #[test]
    fn rejects_oversized_and_truncated_frames() {
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
    fn prefixes_metadata_without_rewriting_json() {
        let mut output = Vec::new();
        copy_metadata(
            Cursor::new(b"{\"event\":\"command_started\"}\n"),
            &mut output,
        )
        .unwrap();
        assert_eq!(
            output,
            b"[ghaiw-meta] {\"event\":\"command_started\"}\n"
        );
    }
}