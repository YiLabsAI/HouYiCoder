//! Tests for the framed message reader and writer.

use tokio::io::AsyncWriteExt;
use tokio::io::duplex;

use super::*;

#[tokio::test]
async fn test_round_trip() {
    let (mut client, mut server) = duplex(4096);
    let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
    write_frame(&mut client, body).await.expect("write");
    let read = read_frame(&mut server)
        .await
        .expect("read")
        .expect("a frame");
    assert_eq!(read, body);
}

/// Headers other than the length are read past, and the name matches
/// case-insensitively.
#[tokio::test]
async fn test_extra_headers_ignored() {
    let (mut client, mut server) = duplex(4096);
    let body = b"{}";
    let header = format!(
        "content-type: application/vscode-jsonrpc; charset=utf-8\r\nCONTENT-LENGTH: {}\r\n\r\n",
        body.len()
    );
    client.write_all(header.as_bytes()).await.expect("header");
    client.write_all(body).await.expect("body");
    let read = read_frame(&mut server)
        .await
        .expect("read")
        .expect("a frame");
    assert_eq!(read, body);
}

/// A clean stream end between frames reads as None; an end inside the body
/// is an error.
#[tokio::test]
async fn test_incomplete_edges() {
    let (client, mut server) = duplex(1024);
    drop(client);
    assert!(read_frame(&mut server).await.expect("clean eof").is_none());

    let (mut client, mut server) = duplex(1024);
    client
        .write_all(b"Content-Length: 10\r\n\r\nabc")
        .await
        .expect("short frame");
    drop(client);
    assert!(read_frame(&mut server).await.is_err());
}

/// A header naming no length, or a non-numeric one, is a protocol error.
#[tokio::test]
async fn test_bad_length() {
    let (mut client, mut server) = duplex(1024);
    client
        .write_all(b"Content-Type: x\r\n\r\n")
        .await
        .expect("no length");
    drop(client);
    let error = read_frame(&mut server).await.expect_err("no length");
    assert!(error.to_string().contains("names no length"), "{error}");

    let (mut client, mut server) = duplex(1024);
    client
        .write_all(b"Content-Length: many\r\n\r\n")
        .await
        .expect("bad length");
    drop(client);
    let error = read_frame(&mut server).await.expect_err("bad length");
    assert!(error.to_string().contains("not a number"), "{error}");
}

/// A declared length over the cap is refused before any allocation of that
/// size: the corrupt length itself is the finding.
#[tokio::test]
async fn test_oversized_frame_refused() {
    let (mut client, mut server) = duplex(1024);
    let header = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
    client.write_all(header.as_bytes()).await.expect("header");
    drop(client);
    let error = read_frame(&mut server).await.expect_err("over cap");
    assert!(error.to_string().contains("exceeds the cap"), "{error}");
}

/// A zero-length body is a legal frame at this layer: the header parsed and
/// the reader hands back an empty body, which the JSON layer then rejects
/// as a bad message rather than a bad frame.
#[tokio::test]
async fn test_zero_length_frame() {
    let (mut client, mut server) = duplex(1024);
    client
        .write_all(b"Content-Length: 0\r\n\r\n")
        .await
        .expect("empty frame");
    let read = read_frame(&mut server)
        .await
        .expect("reads")
        .expect("a frame");
    assert!(read.is_empty());
}

/// An LF-only header block never reaches the blank-line terminator the
/// protocol requires, so the stream's end reports an incomplete header
/// rather than misparsing the block.
#[tokio::test]
async fn test_lf_only_headers_rejected() {
    let (mut client, mut server) = duplex(1024);
    client
        .write_all(b"Content-Length: 2\n\n{}")
        .await
        .expect("lf headers");
    drop(client);
    let error = read_frame(&mut server)
        .await
        .expect_err("incomplete header");
    assert!(error.kind() == std::io::ErrorKind::UnexpectedEof, "{error}");
}

/// A header block that never terminates is refused at its cap instead of
/// growing without bound.
#[tokio::test]
async fn test_oversized_header_refused() {
    let (mut client, mut server) = duplex(16 * 1024);
    let filler = vec![b'x'; MAX_HEADER_BYTES + 16];
    client.write_all(&filler).await.expect("filler");
    let error = read_frame(&mut server).await.expect_err("over cap");
    assert!(error.to_string().contains("exceeds its cap"), "{error}");
}

/// A stream cut inside the header block is a protocol error, not a clean
/// end: a half message is not a closed connection.
#[tokio::test]
async fn test_partial_header_at_eof() {
    let (mut client, mut server) = duplex(1024);
    client
        .write_all(b"Content-Length: 4\r\n")
        .await
        .expect("partial");
    drop(client);
    let error = read_frame(&mut server).await.expect_err("partial header");
    assert!(
        error.to_string().contains("inside a header block"),
        "{error}"
    );
}
