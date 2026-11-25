//! The Content-Length framed messages one language server speaks over its
//! standard streams: one header block, a blank line, then exactly the byte
//! count the header names. Other headers a server sends are read past and
//! ignored; a frame larger than the cap is an error, so a corrupt length
//! cannot make the reader allocate without bound.

use std::io;
use std::io::Error;
use std::io::ErrorKind;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

/// The largest frame the reader accepts. A real message from a language
/// server is far smaller; the cap exists so a wrong length is a clean error
/// rather than an allocation the host cannot afford.
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// The largest header block the reader accepts before the blank line.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Read one frame body, or None when the stream ends cleanly between
/// frames. A stream that ends inside a frame is an error: a half message is
/// not a clean close.
pub async fn read_frame<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let Some(header) = read_header_block(reader).await? else {
        return Ok(None);
    };
    let length = content_length(&header)?;
    if length > MAX_FRAME_BYTES {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("a frame of {length} bytes exceeds the cap"),
        ));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

/// Write one frame with its header and flush it, so a request is on the
/// wire before the caller starts waiting for its answer.
pub async fn write_frame<W>(writer: &mut W, body: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

/// Read header lines up to the blank one, returning the text of the block.
/// None means the stream ended before any header byte.
async fn read_header_block<R>(reader: &mut R) -> io::Result<Option<String>>
where
    R: AsyncRead + Unpin,
{
    let mut block = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte).await? {
            0 => {
                return if block.is_empty() {
                    Ok(None)
                } else {
                    Err(Error::new(
                        ErrorKind::UnexpectedEof,
                        "the stream ended inside a header block",
                    ))
                };
            }
            _ => block.push(byte[0]),
        }
        if block.ends_with(b"\r\n\r\n") {
            break;
        }
        if block.len() > MAX_HEADER_BYTES {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "the header block exceeds its cap",
            ));
        }
    }
    String::from_utf8(block)
        .map(Some)
        .map_err(|_| Error::new(ErrorKind::InvalidData, "the header block is not utf-8"))
}

/// The declared body length, case-insensitively found among the headers.
fn content_length(header: &str) -> io::Result<usize> {
    for line in header.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse::<usize>().map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    "the declared length is not a number",
                )
            });
        }
    }
    Err(Error::new(
        ErrorKind::InvalidData,
        "the header block names no length",
    ))
}

#[cfg(test)]
mod tests;
