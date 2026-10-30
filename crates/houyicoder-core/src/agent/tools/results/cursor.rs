//! The opaque place a page left off at, bound to the session and the request
//! that issued it.
//!
//! A cursor names the scope it was issued for, the request it belongs to, the
//! log epoch and the size of the index when it was issued, and the position it
//! continues from.
//! Every field is covered by a check value, so a cursor whose position was
//! edited is refused rather than followed. The check catches a cursor that was
//! changed, not one forged by a caller that computes the same value; what a
//! forged cursor can reach is a different position in a stream the caller was
//! already allowed to read.

use std::str::FromStr;

use houyicoder_api::result_catalog::ResultCatalogError;
use houyicoder_context::SessionId;
use houyicoder_protocol::result_catalog::ResultCursor;

/// The request a cursor continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Operation {
    /// A result listing.
    List,
    /// A search over stored results.
    Search,
    /// A read of one stream.
    Read,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Search => "search",
            Self::Read => "read",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "list" => Some(Self::List),
            "search" => Some(Self::Search),
            "read" => Some(Self::Read),
            _ => None,
        }
    }
}

/// A decoded cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cursor {
    /// The index size the cursor was issued against.
    pub watermark: usize,
    /// The result the next page starts at, for a listing or a search.
    pub entry: usize,
    /// The stored byte offset the next page starts at, for a read or a search
    /// inside one result.
    pub offset: u64,
}

/// The version this build writes. A cursor carrying any other version was
/// issued by a build whose positions do not mean what this one means.
const VERSION: &str = "v2";

/// The number that stands for the request a cursor continues. A cursor
/// carries the query, the stream and the filter it was issued under, so a
/// caller cannot take the position of one request and apply it to another:
/// the second request's targets are a different list, and the same index and
/// offset would silently point at the wrong place.
pub(super) fn request(parts: &[&str]) -> u64 {
    const BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = BASIS;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        // A separator keeps ["ab", "c"] apart from ["a", "bc"].
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The check value a cursor carries over its own text, so a position edited
/// after issue does not pass for one this host issued.
fn request_number(body: &str) -> u64 {
    request(&[body])
}

/// Issue a cursor for a position.
pub(super) fn issue(
    scope: SessionId,
    epoch: &str,
    operation: Operation,
    request: u64,
    watermark: usize,
    entry: usize,
    offset: u64,
) -> ResultCursor {
    let body = format!(
        "{VERSION}|{scope}|{}|{request}|{epoch}|{watermark}|{entry}|{offset}",
        operation.name()
    );
    let check = request_number(&body);
    ResultCursor::new(format!("{body}|{check:016x}"))
}

/// Read back a cursor this scope issued for this request.
pub(super) fn parse(
    cursor: &ResultCursor,
    scope: SessionId,
    epoch: &str,
    operation: Operation,
    request: u64,
    index_len: usize,
) -> Result<Cursor, ResultCatalogError> {
    let refuse = |why: &str| ResultCatalogError::InvalidCursor(why.to_string());
    let (body, check) = cursor
        .as_str()
        .rsplit_once('|')
        .ok_or_else(|| refuse("empty cursor"))?;
    let check = u64::from_str_radix(check, 16).map_err(|_| refuse("no check value"))?;
    if check != request_number(body) {
        return Err(refuse("a cursor whose position was changed"));
    }
    let mut parts = body.split('|');
    let version = parts.next().ok_or_else(|| refuse("empty cursor"))?;
    if version != VERSION {
        return Err(refuse("a cursor from another build"));
    }
    let issued_for = parts.next().ok_or_else(|| refuse("no scope"))?;
    if issued_for != scope.to_string() {
        return Err(refuse("a cursor from another session"));
    }
    let issued_op = parts.next().ok_or_else(|| refuse("no request"))?;
    if Operation::parse(issued_op) != Some(operation) {
        return Err(refuse("a cursor from another request type"));
    }
    let issued_request = parse_number::<u64>(parts.next(), "no request")?;
    if issued_request != request {
        return Err(refuse("a cursor from a different request"));
    }
    let issued_epoch = parts.next().ok_or_else(|| refuse("no epoch"))?;
    if issued_epoch != epoch {
        // A clear replaces the log state, so a position issued against the
        // epoch before it names results this session no longer holds.
        return Err(refuse("a cursor from before the history was cleared"));
    }
    let watermark = parse_number::<usize>(parts.next(), "no watermark")?;
    let entry = parse_number::<usize>(parts.next(), "no result")?;
    let offset = parse_number::<u64>(parts.next(), "no offset")?;
    if parts.next().is_some() {
        return Err(refuse("trailing fields"));
    }
    // Within one epoch the index only grows, so a position past its end names
    // work this store no longer holds.
    if entry > index_len || watermark > index_len {
        return Err(refuse(
            "the position is past the results this session holds",
        ));
    }
    Ok(Cursor {
        watermark,
        entry,
        offset,
    })
}

fn parse_number<T: FromStr>(
    text: Option<&str>,
    missing: &'static str,
) -> Result<T, ResultCatalogError> {
    text.ok_or_else(|| ResultCatalogError::InvalidCursor(missing.to_string()))?
        .parse()
        .map_err(|_| ResultCatalogError::InvalidCursor(missing.to_string()))
}
