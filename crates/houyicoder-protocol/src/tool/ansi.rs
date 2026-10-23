//! Terminal control sequences removed from text headed for the screen.
//!
//! Tool output is produced by programs that colour their output, so it carries
//! escape sequences. A terminal renderer that paints the text as cells has no
//! use for them: the escape byte is dropped by the cell layer, and whatever
//! followed it is then painted as ordinary characters. Removing the whole
//! sequence before the text reaches a cell keeps that from happening.
//!
//! Two shapes are removed. A complete sequence, which starts with the escape
//! byte, and an orphaned colour fragment whose escape byte was already lost
//! upstream. The second shape is matched tightly: only a bracketed run of
//! colour parameters ending in the colour terminator, so an array index or an
//! attribute in quoted code survives.

/// Remove terminal control sequences from text headed for a character cell.
pub fn strip_ansi_codes(text: &str) -> String {
    if !may_carry_control(text) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i = skip_escape(bytes, i);
            continue;
        }
        if bytes[i] == b'['
            && let Some(end) = orphan_colour_fragment(bytes, i)
        {
            i = end;
            continue;
        }
        // Advance one whole character so a multi-byte sequence is never split.
        let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// A cheap pre-check so the common case (no control bytes and no bracket) does
/// not pay for a full scan.
fn may_carry_control(text: &str) -> bool {
    text.as_bytes().iter().any(|b| *b == 0x1b || *b == b'[')
}

/// Index just past the escape sequence that starts at i.
fn skip_escape(bytes: &[u8], i: usize) -> usize {
    let Some(&kind) = bytes.get(i + 1) else {
        return i + 1;
    };
    match kind {
        // Control sequence introducer: parameters then one final byte.
        b'[' => {
            let mut j = i + 2;
            while j < bytes.len() && !bytes[j].is_ascii_alphabetic() {
                j += 1;
            }
            (j + 1).min(bytes.len())
        }
        // Operating system command: runs to a bell or a string terminator.
        b']' => {
            let mut j = i + 2;
            while j < bytes.len() {
                if bytes[j] == 0x07 {
                    return j + 1;
                }
                if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                    return j + 2;
                }
                j += 1;
            }
            bytes.len()
        }
        // A two-character escape (character set selection and the like).
        _ => (i + 2).min(bytes.len()),
    }
}

/// If a colour fragment without its escape byte starts at i, the index just
/// past it. Only digits and semicolons are accepted between the brackets, and
/// the run must end in the colour terminator, so an array index or an
/// attribute in quoted code is left alone.
fn orphan_colour_fragment(bytes: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let mut digits = 0usize;
    while j < bytes.len() {
        match bytes[j] {
            b'0'..=b'9' => digits += 1,
            b';' => {}
            b'm' => {
                return (digits > 0 && digits <= 4).then_some(j + 1);
            }
            _ => return None,
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests;
