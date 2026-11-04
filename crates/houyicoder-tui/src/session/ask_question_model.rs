//! Data model for the AskUserQuestion card: parsed question cards, option
//! structs, and the interactive state (selections, cursors, Other text).
//! The parse method validates the model JSON; the build methods assemble
//! the answers map and the updated input for the resume decision.
//!
//! The current index ranges over 0..=questions.len(). When it equals
//! questions.len() the card is in the submit view (review screen). Tab
//! navigation moves the index within that inclusive range. A single
//! single-select question auto-submits on selection and hides the nav bar
//! and submit tab entirely.

use serde_json::Map;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

/// One question card parsed from the model's AskUserQuestion tool call. The
/// header is capped at 12 chars by the schema; the TUI truncates the display
/// further if needed. options is the model-supplied list (2-4 entries); the
/// Other option is auto-appended by the card state, not stored here.
#[derive(Debug, Clone)]
pub struct QuestionCard {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

/// A single option in a question card: label + description. preview is
/// parsed but not rendered in the MVP (deferred).
#[derive(Debug, Clone)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
}

/// Interactive state for the AskUserQuestion card. current ranges over
/// 0..=questions.len(): 0..questions.len()-1 are question views,
/// questions.len() is the submit (review) view. selections holds
/// per-question option indices (including the auto-appended Other at
/// position options.len()). cursors is the focused option per question,
/// independent of selections so multi-select can move the cursor without
/// toggling. other_text holds per-question custom text for Other.
/// submit_cursor is the focused row in the submit view (0 = Submit
/// answers, 1 = Cancel). other_focused is true when the Other text input
/// is active for the current question (tab-nav is disabled during entry).
#[derive(Debug, Clone)]
pub struct AskQuestion {
    pub call_id: String,
    pub questions: Vec<QuestionCard>,
    pub current: usize,
    pub selections: Vec<Vec<usize>>,
    pub cursors: Vec<usize>,
    pub other_text: Vec<Option<String>>,
    pub other_focused: bool,
    /// Whether the card shows every row its text needs. Collapsed, each row
    /// takes one line so the card stays inside the footer budget; expanded, a
    /// long question or option wraps instead of being cut at the edge.
    pub expanded: bool,
    pub submit_cursor: usize,
    pub original_input: Value,
}

/// The rows one line needs at this width. A collapsed card gives every line
/// one row, which is what keeps it inside the footer budget.
fn line_rows(text: &str, width: u16, expanded: bool) -> u16 {
    if !expanded || width == 0 {
        return 1;
    }
    let limit = usize::from(width);
    let mut rows = 0usize;
    for line in text.lines() {
        rows += UnicodeWidthStr::width(line).max(1).div_ceil(limit);
    }
    rows.max(1) as u16
}

/// The sentinel label for the auto-appended Other option.
pub const OTHER_LABEL: &str = "__other__";

impl AskQuestion {
    /// Parse the model's tool-call input into interactive state. Returns
    /// None when the input is malformed (fewer than 2 options, more than 4
    /// questions, missing fields). On success, initializes empty selections
    /// and Other text for each question.
    pub fn parse(call_id: &str, input: &Value) -> Option<Self> {
        let qs = input.get("questions")?.as_array()?;
        if qs.is_empty() || qs.len() > 4 {
            return None;
        }
        let mut questions = Vec::with_capacity(qs.len());
        for q in qs {
            let question = q.get("question")?.as_str()?.to_string();
            let header = q.get("header")?.as_str()?.to_string();
            let multi_select = q
                .get("multiSelect")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let opts = q.get("options")?.as_array()?;
            if opts.len() < 2 || opts.len() > 4 {
                return None;
            }
            let mut options = Vec::with_capacity(opts.len());
            for o in opts {
                let label = o.get("label")?.as_str()?.to_string();
                let description = o
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let preview = o
                    .get("preview")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                options.push(QuestionOption {
                    label,
                    description,
                    preview,
                });
            }
            questions.push(QuestionCard {
                question,
                header,
                options,
                multi_select,
            });
        }
        let n = questions.len();
        Some(Self {
            call_id: call_id.to_string(),
            questions,
            current: 0,
            selections: vec![Vec::new(); n],
            cursors: vec![0; n],
            other_text: vec![None; n],
            other_focused: false,
            expanded: true,
            submit_cursor: 0,
            original_input: input.clone(),
        })
    }

    /// True when the card is in the submit (review) view. The submit view
    /// index is questions.len().
    pub fn is_submit_view(&self) -> bool {
        self.current >= self.questions.len()
    }

    /// True when there is a single single-select question. In that case the
    /// nav bar and submit tab are hidden, and selecting any option (including
    /// Other) auto-submits the whole card.
    pub fn hide_submit_tab(&self) -> bool {
        self.questions.len() == 1 && !self.questions[0].multi_select
    }

    /// The upper bound for the current index (inclusive). When the submit
    /// tab is hidden, navigation is capped at the last question (no submit
    /// view). Otherwise the submit view (questions.len()) is reachable.
    pub fn max_index(&self) -> usize {
        if self.hide_submit_tab() {
            self.questions.len().saturating_sub(1)
        } else {
            self.questions.len()
        }
    }

    /// The total option count for the current question, including the
    /// auto-appended Other. For multi-select, also includes the
    /// Submit/Next button row appended after Other.
    pub fn current_option_count(&self) -> usize {
        self.questions
            .get(self.current)
            .map(|q| {
                let base = q.options.len() + 1;
                if q.multi_select { base + 1 } else { base }
            })
            .unwrap_or(0)
    }

    /// The index of the auto-appended Other option for the current question.
    /// Whether the cursor rests on the Other row, where typing goes into the
    /// card rather than into the composer.
    pub fn cursor_on_other(&self) -> bool {
        !self.is_submit_view()
            && self.cursors.get(self.current).copied().unwrap_or(0) == self.current_other_idx()
    }

    /// Append pasted text to the Other row, opening it as the input if the
    /// cursor is already there.
    pub fn append_other(&mut self, text: &str) {
        let qi = self.current;
        self.other_focused = true;
        let other_idx = self.current_other_idx();
        if self.other_text[qi].is_none() {
            self.other_text[qi] = Some(String::new());
        }
        if !self.selections[qi].contains(&other_idx) {
            self.selections[qi].push(other_idx);
        }
        if let Some(entry) = self.other_text[qi].as_mut() {
            entry.push_str(text);
        }
    }

    pub fn current_other_idx(&self) -> usize {
        self.questions
            .get(self.current)
            .map(|q| q.options.len())
            .unwrap_or(0)
    }

    /// The index of the Submit/Next button for multi-select (only valid
    /// when the current question is multi-select). It is the row after
    /// Other.
    pub fn current_submit_btn_idx(&self) -> usize {
        self.questions
            .get(self.current)
            .map(|q| q.options.len() + 1)
            .unwrap_or(0)
    }

    /// True when the current question is multi-select.
    pub fn current_multi(&self) -> bool {
        self.questions
            .get(self.current)
            .is_some_and(|q| q.multi_select)
    }

    /// True when the Other slot is among the selected indices for question qidx.
    pub fn other_is_selected(&self, qidx: usize) -> bool {
        let other_pos = self.questions.get(qidx).map(|q| q.options.len());
        other_pos.is_some_and(|pos| self.selections[qidx].contains(&pos))
    }

    /// True when the question at qidx has at least one selection.
    pub fn is_answered(&self, qidx: usize) -> bool {
        self.selections.get(qidx).is_some_and(|s| !s.is_empty())
    }

    /// True when every question has at least one selection.
    pub fn all_answered(&self) -> bool {
        self.selections.iter().all(|s| !s.is_empty())
    }

    /// The label for the Submit/Next button on the current multi-select
    /// question. "Submit" on the last question, "Next" otherwise.
    pub fn submit_btn_label(&self) -> &'static str {
        if self.current + 1 >= self.questions.len() {
            "Submit"
        } else {
            "Next"
        }
    }

    /// The rows each content line needs at this width, in draw order:
    /// question, every option, Other, and the multi-select button when the
    /// question has one. Collapsed, each is one row.
    pub fn content_rows(&self, width: u16) -> Vec<u16> {
        let Some(q) = self.questions.get(self.current) else {
            return Vec::new();
        };
        // Each row is drawn with a marker and a checkbox ahead of its text, so
        // the columns left for the text are the width minus that prefix.
        const OPTION_PREFIX: u16 = 7;
        const OTHER_PREFIX: u16 = 10;
        let mut rows = vec![line_rows(
            &q.question,
            width.saturating_sub(1),
            self.expanded,
        )];
        let focused = self.cursors.get(self.current).copied().unwrap_or(0);
        for (i, opt) in q.options.iter().enumerate() {
            let text = if i == focused && !opt.description.is_empty() {
                format!("{}. {} -- {}", i + 1, opt.label, opt.description)
            } else {
                format!("{}. {}", i + 1, opt.label)
            };
            rows.push(line_rows(
                &text,
                width.saturating_sub(OPTION_PREFIX),
                self.expanded,
            ));
        }
        let other = self.other_text[self.current]
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or(if q.multi_select {
                "Type something"
            } else {
                "Type something."
            });
        rows.push(line_rows(
            other,
            width.saturating_sub(OTHER_PREFIX),
            self.expanded,
        ));
        if q.multi_select {
            rows.push(1);
        }
        rows
    }

    /// The height (in terminal rows) the card needs for the current view at
    /// this width.
    pub fn card_height_for(&self, width: u16) -> u16 {
        if self.is_submit_view() {
            // Two rows per question plus the separator, the navigation bar,
            // the title, the warning, and the two select rows.
            return (2 * self.questions.len() + 6) as u16;
        }
        let content: u16 = self.content_rows(width).iter().sum();
        let nav_bar = if self.hide_submit_tab() { 0 } else { 1 };
        // separator + nav + header + content + gap + gap + hint
        content + 5 + nav_bar
    }

    /// The height (in terminal rows) the card needs for the current view.
    /// Question view: separator + nav_bar + header + question + gap +
    /// options(inc Other) + submit_btn(multi only) + gap + hint.
    /// Submit view: separator + nav_bar + title + warning + answers +
    /// prompt + select.
    pub fn card_height(&self) -> u16 {
        self.card_height_for(80)
    }

    /// Build the answers map from the current selections. Each question maps
    /// to a single label string: for single-select, the selected label; for
    /// multi-select, comma-joined labels; for Other, the typed text replaces
    /// the label. Empty when nothing was selected (the tool handles this).
    pub fn build_answers(&self) -> Value {
        let mut answers = Map::new();
        for (qi, q) in self.questions.iter().enumerate() {
            let labels: Vec<String> = self.selections[qi]
                .iter()
                .filter_map(|&idx| {
                    if idx >= q.options.len() {
                        self.other_text[qi]
                            .as_ref()
                            .filter(|t| !t.is_empty())
                            .cloned()
                    } else {
                        Some(q.options[idx].label.clone())
                    }
                })
                .collect();
            let answer = labels.join(", ");
            answers.insert(q.question.clone(), Value::String(answer));
        }
        Value::Object(answers)
    }

    /// Build the updated input for the resume decision: the original
    /// questions verbatim plus the answers map the UI assembled.
    pub fn build_updated_input(&self) -> Value {
        let mut input = self.original_input.clone();
        if let Some(obj) = input.as_object_mut() {
            obj.insert("answers".to_string(), self.build_answers());
        }
        input
    }
}

/// The card's own tests. Crate-visible because a sibling test builds a card
/// through the same fixtures rather than restating them.
#[cfg(test)]
pub(crate) mod tests;
