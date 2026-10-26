//! What a session is about, as recorded on its descriptor: the first and last
//! user input that states a task, and where each came from.
//!
//! One policy decides what the descriptor records, so every reader agrees on
//! a session's subject. Greetings, bare acknowledgments, bare continuations,
//! and frontend commands are not task statements and never become a title.
//! Editor metadata is stripped from the input rather than making the whole
//! input untitled, and a leading slash alone does not mark a command: a
//! pasted path such as /Users/me/src/main.rs is a real subject and stays.

use crate::descriptor::{PromptCoverage, PromptRecord, SessionDescriptor};

/// Longest prompt kept in the descriptor, in characters. A pasted document
/// is a title only up to its opening statement.
const MAX_PROMPT_CHARS: usize = 200;

/// Longest raw input read before cleaning, in characters. Editor metadata can
/// precede the user's own words, so the bound leaves room for a few blocks,
/// while a pasted megabyte must not be copied several times to produce a
/// 200-character title. The prompt arrives on the thread driving the turn, so
/// this work has to stay small whatever the input.
const MAX_RAW_CHARS: usize = 64 * 1024;

/// Longest metadata block kept whole, in characters. The bound is also where
/// the search for its close tag stops: a longer block is emitted by the same
/// editor and still is not prose, so the text from its opening tag on is
/// treated as block content and does not become a title.
const MAX_METADATA_BLOCK_CHARS: usize = 4096;

/// Longest tag name read before deciding the family. Every metadata family
/// name is shorter; a longer run of name characters still matches by prefix,
/// so only the decision needs the bound.
const MAX_TAG_NAME_CHARS: usize = 64;

/// Inputs that answer or open a conversation without saying what the work
/// is: greetings, acknowledgments, and bare continuations. Matched after
/// whitespace collapse, control stripping, trailing punctuation trim, and
/// case folding.
const NO_TASK_INPUT: [&str; 41] = [
    "hi",
    "hello",
    "hey",
    "yo",
    "hi there",
    "hello there",
    "hey there",
    "ok",
    "okay",
    "k",
    "yes",
    "yep",
    "no",
    "nope",
    "sure",
    "got it",
    "i see",
    "sounds good",
    "thanks",
    "thank you",
    "good",
    "great",
    "nice",
    "perfect",
    "continue",
    "go on",
    "keep going",
    "你好",
    "您好",
    "在吗",
    "继续",
    "好的",
    "好",
    "可以",
    "行",
    "没问题",
    "收到",
    "谢谢",
    "嗯",
    "对的",
    "就这样",
];

/// The task statement a user input carries, or None when it carries none.
///
/// The text is returned in display form: terminal control sequences removed,
/// runs of whitespace collapsed to one space, trimmed, and capped so a
/// pasted document cannot become a session title.
pub fn prompt_intent(text: &str) -> Option<String> {
    let head: String = text.chars().take(MAX_RAW_CHARS).collect();
    let cleaned = collapse_whitespace(&strip_metadata_blocks(&strip_control(&head)));
    if cleaned.is_empty() {
        return None;
    }
    if names_command(&cleaned) {
        return None;
    }
    if !cleaned.chars().any(char::is_alphanumeric) {
        return None;
    }
    let folded = cleaned
        .trim_end_matches(|c: char| c.is_ascii_punctuation() || is_cjk_end_mark(c))
        .to_lowercase();
    if NO_TASK_INPUT.contains(&folded.as_str()) {
        return None;
    }
    Some(cap(cleaned))
}

/// Fold one prompt into the descriptor. Both slots follow the event id of the
/// record they hold: the first prompt keeps the earliest statement ever seen
/// and the last keeps the newest, so two writes that land in either order
/// leave the same session subject.
///
/// The first prompt is recorded only for a session observed from its start. An
/// older session's coverage says its log was never read end to end, so a
/// prompt arriving now cannot be claimed as the first.
pub fn apply_prompt(descriptor: &mut SessionDescriptor, prompt: PromptRecord) {
    let earliest = descriptor
        .first_prompt
        .as_ref()
        .is_none_or(|stored| prompt.event_id < stored.event_id);
    if descriptor.prompt_coverage == PromptCoverage::Complete && earliest {
        descriptor.first_prompt = Some(prompt.clone());
    }
    let newest = descriptor
        .last_prompt
        .as_ref()
        .is_none_or(|stored| stored.event_id < prompt.event_id);
    if newest {
        descriptor.last_prompt = Some(prompt);
    }
}

/// Whether the input opens a frontend command. The first word after the
/// slash decides: a command name holds no further slash, while a pasted
/// path such as /Users/me/src/main.rs does. The host forwards the path to
/// the model, so it stays a task statement.
fn names_command(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('/') else {
        return false;
    };
    let name = rest.split_whitespace().next().unwrap_or("");
    !name.contains('/')
}

/// Remove the wrapped metadata blocks an editor or the harness inserted,
/// keeping the prose around them. A message can carry the editor context
/// before the user's own words, or a hook note after them, and the words are
/// the subject either way. Only a block whose tag belongs to the metadata
/// families is dropped: a task that mentions a tag, such as a generic bound
/// that fails to compile or an HTML snippet the user asked about, stays. An
/// opening tag of those families whose close lies past the bound ends the
/// text, because the block continues rather than returning to prose.
fn strip_metadata_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        match scan_metadata_block(tail) {
            MetadataScan::Block(len) => rest = &tail[len..],
            MetadataScan::Unterminated => return out,
            MetadataScan::None => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// What a text starting at an opening angle bracket begins.
enum MetadataScan {
    /// A metadata block of this length, close tag included.
    Block(usize),
    /// An opening metadata tag whose close is past the block bound. The
    /// rest of the text is that block, so keeping any of it would make the
    /// tag's own name the title.
    Unterminated,
    /// Not the opening of a metadata block.
    None,
}

/// Classify the text when it starts with an opening tag of the editor or
/// harness families. A tag outside those families is prose, and a paired
/// block inside the bound is dropped whole.
fn scan_metadata_block(text: &str) -> MetadataScan {
    let Some(after_bracket) = text.strip_prefix('<') else {
        return MetadataScan::None;
    };
    // The tag name decides first. A text that opens no metadata tag is prose,
    // and the block-sized window below must not be walked for every angle
    // bracket in a large paste. The name is ASCII by the predicate, so the
    // bound cannot split a character, and a longer name still matches by
    // prefix.
    let name_len = after_bracket
        .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'))
        .unwrap_or(after_bracket.len())
        .min(MAX_TAG_NAME_CHARS);
    let name = &after_bracket[..name_len];
    if !is_metadata_tag(name) {
        return MetadataScan::None;
    }
    let window = match text.char_indices().nth(MAX_METADATA_BLOCK_CHARS) {
        Some((end, _)) => &text[..end],
        None => text,
    };
    let Some(body_start) = window.find('>') else {
        return MetadataScan::Unterminated;
    };
    let close = format!("</{name}>");
    match window[body_start..].find(&close) {
        Some(at) => MetadataScan::Block(body_start + at + close.len()),
        None => MetadataScan::Unterminated,
    }
}

fn is_metadata_tag(name: &str) -> bool {
    const FAMILIES: [&str; 4] = ["ide_", "system-reminder", "command-name", "local-command"];
    FAMILIES.iter().any(|family| name.starts_with(family))
}

/// Join the words of one whitespace run with single spaces, stopping at the
/// descriptor's cap. A large paste is read once into the first words rather
/// than collected whole and then cut.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::new();
    let mut kept = 0;
    for word in text.split_whitespace() {
        // The space costs one of the cap's characters, and a separator with
        // no word after it is never written.
        let separator = usize::from(kept > 0);
        if kept + separator >= MAX_PROMPT_CHARS {
            break;
        }
        if separator > 0 {
            out.push(' ');
            kept += 1;
        }
        for c in word.chars() {
            if kept == MAX_PROMPT_CHARS {
                break;
            }
            out.push(c);
            kept += 1;
        }
    }
    out
}

/// Trim to the descriptor's cap by whole chars, so the stored text is valid
/// UTF-8 and truncation never splits a scalar. A combining mark or half a
/// joined emoji can still fall at the cut, which renders imperfectly but
/// keeps the text short; no grapheme dependency is worth this title.
fn cap(text: String) -> String {
    text.chars().take(MAX_PROMPT_CHARS).collect()
}

fn is_cjk_end_mark(c: char) -> bool {
    matches!(
        c,
        '。' | '，' | '！' | '？' | '；' | '：' | '、' | '…' | '～'
    )
}

/// Drop terminal control sequences and keep printable text plus single
/// spaces for line breaks. The title is rendered in a terminal, so an
/// escape sequence carried in a prompt either paints over the pane or moves
/// the cursor.
fn strip_control(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests;
