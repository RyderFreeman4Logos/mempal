//! Test-only bounded HTTP/1.1 request reader (headers then Content-Length).

use std::io::{self, Read};
use std::net::TcpStream;
use std::time::Duration;

pub const MAX_BYTES: usize = 4096;

pub fn read_tcp(stream: &mut TcpStream, max_bytes: usize) -> io::Result<String> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    read(stream, max_bytes)
}

pub fn read(stream: &mut impl Read, max_bytes: usize) -> io::Result<String> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let bytes_read = stream.read(&mut chunk)?;
        if bytes_read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request closed before headers were complete",
            ));
        }
        if bytes_read > max_bytes.saturating_sub(request.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request exceeds size cap",
            ));
        }
        request.extend_from_slice(&chunk[..bytes_read]);
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing content-length"))?;
    let expected_len = header_end
        .checked_add(content_length)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "content-length overflow"))?;
    if expected_len > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "content-length exceeds size cap",
        ));
    }
    while request.len() < expected_len {
        let want = (expected_len - request.len()).min(chunk.len());
        let bytes_read = stream.read(&mut chunk[..want])?;
        if bytes_read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request closed before body was complete",
            ));
        }
        request.extend_from_slice(&chunk[..bytes_read]);
    }
    Ok(String::from_utf8_lossy(&request[..expected_len]).into_owned())
}
