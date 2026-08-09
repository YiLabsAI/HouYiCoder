//! Per-line render cache for the transcript's two expensive parses: the
//! agent-markdown render (markdown::render_agent_text) and the diff/stdout
//! plan (markers::result_body_rows). A 4000-line transcript re-parses every
//! line on each draw, which is the dominant per-frame cost and the scroll
//! freeze root cause.
//!
//! The transcript is rebuilt wholesale on each event batch
//! (run_control::rebuild_transcript), so line indices are unstable and the
//! cache cannot key on position. It keys on a 128-bit content hash + pane
//! width + expand flag, which is stable across rebuilds AND across the future
//! disk-backed search load (lines arrive by content, not in-memory index).
//! Two independent 64-bit hashes (different seeds) make a collision need both
//! to collide (~2^-128) — safe as an exact key without storing the source.
//!
//! Both the count path (state::line_display_rows) and the render path
//! (working_transcript::draw_transcript) go through this cache, so a line is
//! parsed at most once per (content, width, expand) and reused across all
//! frames and across transcript rebuilds. The count path reads the cached
//! length without cloning; the render path clones the cached rows.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use ratatui::text::Line;

use crate::records::ToolOutcome;
use crate::view::markers::PlannedRow;

/// Cap on cached entries. A transcript holds at most a few thousand distinct
/// lines at one width; passing this means width churn or a very long session,
/// so clear wholesale — crude but bounds memory, and a width change makes all
/// prior entries uncacheable anyway.
const CAP: usize = 4096;

enum CachedRender {
    Agent {
        lines: Vec<Line<'static>>,
        plain: Vec<String>,
    },
    Tool {
        rows: Vec<PlannedRow>,
    },
    Plain {
        rows: Vec<String>,
    },
}

/// A single-slot (not LRU) cached render for the LIVE streaming text — the
/// live assistant reply + the live reasoning preview. These grow every delta,
/// so a content-hash LRU would never hit (each delta is a new key), accumulate
/// one dead full-content entry per delta, and trip the wholesale map.clear —
/// reintroducing the scroll freeze the cache exists to prevent. A single slot
/// that is REPLACED on a miss (not inserted into the LRU) holds at most one
/// entry per kind: the current streaming text. Durable transcript lines stay on
/// the LRU (their content is stable, so it hits).
struct LiveEntry {
    h1: u64,
    h2: u64,
    width: u16,
    render: LiveRender,
}

enum LiveRender {
    Agent {
        lines: Vec<Line<'static>>,
        plain: Vec<String>,
    },
    Plain {
        rows: Vec<String>,
    },
}

impl CachedRender {
    fn len(&self) -> usize {
        match self {
            CachedRender::Agent { lines, .. } => lines.len(),
            CachedRender::Tool { rows } => rows.len(),
            CachedRender::Plain { rows } => rows.len(),
        }
    }
}

#[derive(Hash, Eq, PartialEq, Clone)]
struct Key {
    kind: u8,
    h1: u64,
    h2: u64,
    width: u16,
    expanded: bool,
}

fn hash128(text: &str) -> (u64, u64) {
    let mut s = std::collections::hash_map::DefaultHasher::new();
    (0xA5_u8, text).hash(&mut s);
    let h1 = s.finish();
    s = std::collections::hash_map::DefaultHasher::new();
    (0x9E_u8, text).hash(&mut s);
    let h2 = s.finish();
    (h1, h2)
}

fn outcome_byte(o: Option<ToolOutcome>) -> u8 {
    match o {
        None => 0,
        Some(ToolOutcome::Running) => 1,
        Some(ToolOutcome::Success) => 2,
        Some(ToolOutcome::Error) => 3,
    }
}

fn hash128_tool(
    body: &str,
    call_id: &str,
    outcome: Option<ToolOutcome>,
    is_diff: bool,
) -> (u64, u64) {
    let mut s = std::collections::hash_map::DefaultHasher::new();
    (0xA5_u8, body, call_id, outcome_byte(outcome), is_diff).hash(&mut s);
    let h1 = s.finish();
    s = std::collections::hash_map::DefaultHasher::new();
    (0x9E_u8, body, call_id, outcome_byte(outcome), is_diff).hash(&mut s);
    let h2 = s.finish();
    (h1, h2)
}

#[derive(Default)]
pub struct RenderCache {
    map: HashMap<Key, CachedRender>,
    /// Single-slot live-streaming caches (not the LRU). See LiveEntry.
    live_agent: Option<LiveEntry>,
    live_reason: Option<LiveEntry>,
}

impl RenderCache {
    /// Styled + plain agent rows for the render path (clones the cached rows).
    pub fn agent_rows(&mut self, text: &str, width: u16) -> (Vec<Line<'static>>, Vec<String>) {
        let (h1, h2) = hash128(text);
        let key = Key {
            kind: 0,
            h1,
            h2,
            width,
            expanded: false,
        };
        if let Some(CachedRender::Agent { lines, plain }) = self.map.get(&key) {
            return (lines.clone(), plain.clone());
        }
        self.evict_if_needed();
        let (lines, plain) = crate::markdown::render_agent_text("●", text, width as usize);
        self.map.insert(
            key,
            CachedRender::Agent {
                lines: lines.clone(),
                plain: plain.clone(),
            },
        );
        (lines, plain)
    }

    /// Agent row count for the count path (no clone — reads the cached length).
    pub fn agent_row_count(&mut self, text: &str, width: u16) -> usize {
        let (h1, h2) = hash128(text);
        let key = Key {
            kind: 0,
            h1,
            h2,
            width,
            expanded: false,
        };
        if let Some(c) = self.map.get(&key) {
            return c.len();
        }
        self.evict_if_needed();
        let (lines, plain) = crate::markdown::render_agent_text("●", text, width as usize);
        let n = lines.len();
        self.map.insert(key, CachedRender::Agent { lines, plain });
        n
    }

    /// Planned result rows for the render path (clones the cached rows).
    pub fn tool_rows(
        &mut self,
        body: &str,
        call_id: &str,
        outcome: Option<ToolOutcome>,
        expanded: bool,
        is_diff: bool,
        width: u16,
    ) -> Vec<PlannedRow> {
        let (h1, h2) = hash128_tool(body, call_id, outcome, is_diff);
        let key = Key {
            kind: 1,
            h1,
            h2,
            width,
            expanded,
        };
        if let Some(CachedRender::Tool { rows }) = self.map.get(&key) {
            return rows.clone();
        }
        self.evict_if_needed();
        let rows = crate::view::markers::result_body_rows(
            body, call_id, outcome, expanded, is_diff, width,
        );
        self.map
            .insert(key, CachedRender::Tool { rows: rows.clone() });
        rows
    }

    /// Planned result row count for the count path (no clone). Pass the real
    /// call_id + outcome so the count path and render path share one cache
    /// entry per result (call_id/outcome do not affect the row count, only
    /// the row content, so the length is correct for both).
    pub fn tool_row_count(
        &mut self,
        body: &str,
        call_id: &str,
        outcome: Option<ToolOutcome>,
        expanded: bool,
        is_diff: bool,
        width: u16,
    ) -> usize {
        let (h1, h2) = hash128_tool(body, call_id, outcome, is_diff);
        let key = Key {
            kind: 1,
            h1,
            h2,
            width,
            expanded,
        };
        if let Some(c) = self.map.get(&key) {
            return c.len();
        }
        self.evict_if_needed();
        let rows = crate::view::markers::result_body_rows(
            body, call_id, outcome, expanded, is_diff, width,
        );
        let n = rows.len();
        self.map.insert(key, CachedRender::Tool { rows });
        n
    }

    fn evict_if_needed(&mut self) {
        if self.map.len() >= CAP {
            self.map.clear();
        }
    }

    /// User-prompt rows for the render path (clones the cached rows). A user
    /// prompt wraps as one plain block with an angle-bracket lead and a
    /// 10k-char head+tail display cap — the model still gets the full text,
    /// only the display is capped so a hand-typed or piped-in huge prompt
    /// does not make the renderer iterate the full text each frame.
    pub fn user_rows(&mut self, text: &str, width: u16) -> Vec<String> {
        const USER_DISPLAY_CAP: usize = 10_000;
        let (h1, h2) = hash128(text);
        let key = Key {
            kind: 2,
            h1,
            h2,
            width,
            expanded: false,
        };
        if let Some(CachedRender::Plain { rows }) = self.map.get(&key) {
            return rows.clone();
        }
        self.evict_if_needed();
        let rows =
            crate::view::line_wrap::wrap_plain_block(text, "> ", width, Some(USER_DISPLAY_CAP));
        self.map
            .insert(key, CachedRender::Plain { rows: rows.clone() });
        rows
    }

    /// User-prompt row count for the count path (no clone — reads the cached
    /// length). Matches user_rows so count==render.
    pub fn user_row_count(&mut self, text: &str, width: u16) -> usize {
        const USER_DISPLAY_CAP: usize = 10_000;
        let (h1, h2) = hash128(text);
        let key = Key {
            kind: 2,
            h1,
            h2,
            width,
            expanded: false,
        };
        if let Some(c) = self.map.get(&key) {
            return c.len();
        }
        self.evict_if_needed();
        let rows =
            crate::view::line_wrap::wrap_plain_block(text, "> ", width, Some(USER_DISPLAY_CAP));
        let n = rows.len();
        self.map.insert(key, CachedRender::Plain { rows });
        n
    }

    /// Expanded-thought rows for the render path (clones the cached rows).
    /// Each logical line indented two spaces and wrapped to the pane width
    /// minus the indent — no char cap, reasoning is bounded by the model.
    pub fn thought_rows(&mut self, text: &str, width: u16) -> Vec<String> {
        let (h1, h2) = hash128(text);
        let key = Key {
            kind: 3,
            h1,
            h2,
            width,
            expanded: false,
        };
        if let Some(CachedRender::Plain { rows }) = self.map.get(&key) {
            return rows.clone();
        }
        self.evict_if_needed();
        let rows = crate::view::line_wrap::wrap_indented_block(text, "  ", width);
        self.map
            .insert(key, CachedRender::Plain { rows: rows.clone() });
        rows
    }

    /// Expanded-thought row count for the count path (no clone). Matches
    /// thought_rows so count==render.
    pub fn thought_row_count(&mut self, text: &str, width: u16) -> usize {
        let (h1, h2) = hash128(text);
        let key = Key {
            kind: 3,
            h1,
            h2,
            width,
            expanded: false,
        };
        if let Some(c) = self.map.get(&key) {
            return c.len();
        }
        self.evict_if_needed();
        let rows = crate::view::line_wrap::wrap_indented_block(text, "  ", width);
        let n = rows.len();
        self.map.insert(key, CachedRender::Plain { rows });
        n
    }

    /// Test-only entry count, to assert memoization (a repeat render is a
    /// hit and does not store a new entry).
    #[cfg(test)]
    pub fn entry_count(&self) -> usize {
        self.map.len()
    }

    // ===== single-slot live-streaming caches =====
    //
    // The live assistant reply + live reasoning preview grow every delta, so
    // the content-hash LRU above never hits them and would pollute. These
    // single-slot variants hold ONE entry per kind (the current streaming
    // text), replaced on a miss — no LRU insert, no wholesale clear. The
    // durable agent/thought rows stay on the LRU (stable content, cache hits).

    /// Live assistant reply: styled + plain rows (same render as agent_rows,
    /// but the single slot, not the LRU). Clones the cached rows on the hit
    /// path; replaces the slot on a miss.
    pub fn live_agent_rows(&mut self, text: &str, width: u16) -> (Vec<Line<'static>>, Vec<String>) {
        let (h1, h2) = hash128(text);
        if let Some(e) = &self.live_agent
            && e.h1 == h1
            && e.h2 == h2
            && e.width == width
            && let LiveRender::Agent { lines, plain } = &e.render
        {
            return (lines.clone(), plain.clone());
        }
        let (lines, plain) = crate::markdown::render_agent_text("●", text, width as usize);
        self.live_agent = Some(LiveEntry {
            h1,
            h2,
            width,
            render: LiveRender::Agent {
                lines: lines.clone(),
                plain: plain.clone(),
            },
        });
        (lines, plain)
    }

    /// Live assistant reply row count (no clone — reads the cached length).
    pub fn live_agent_row_count(&mut self, text: &str, width: u16) -> usize {
        let (h1, h2) = hash128(text);
        if let Some(e) = &self.live_agent
            && e.h1 == h1
            && e.h2 == h2
            && e.width == width
        {
            return match &e.render {
                LiveRender::Agent { lines, .. } => lines.len(),
                _ => 0,
            };
        }
        let (lines, plain) = crate::markdown::render_agent_text("●", text, width as usize);
        let n = lines.len();
        self.live_agent = Some(LiveEntry {
            h1,
            h2,
            width,
            render: LiveRender::Agent { lines, plain },
        });
        n
    }

    /// Live reasoning preview: plain wrapped rows (no char cap — reasoning is
    /// bounded by the model; wraps via wrap_indented_block, no truncation, per
    /// the #10 fix). Single slot, not the LRU.
    pub fn live_reason_rows(&mut self, text: &str, width: u16) -> Vec<String> {
        let (h1, h2) = hash128(text);
        if let Some(e) = &self.live_reason
            && e.h1 == h1
            && e.h2 == h2
            && e.width == width
            && let LiveRender::Plain { rows } = &e.render
        {
            return rows.clone();
        }
        let rows = crate::view::line_wrap::wrap_indented_block(text, "  ", width);
        self.live_reason = Some(LiveEntry {
            h1,
            h2,
            width,
            render: LiveRender::Plain { rows: rows.clone() },
        });
        rows
    }

    /// Live reasoning row count (no clone). Matches live_reason_rows so
    /// count==render.
    pub fn live_reason_row_count(&mut self, text: &str, width: u16) -> usize {
        let (h1, h2) = hash128(text);
        if let Some(e) = &self.live_reason
            && e.h1 == h1
            && e.h2 == h2
            && e.width == width
        {
            return match &e.render {
                LiveRender::Plain { rows } => rows.len(),
                _ => 0,
            };
        }
        let rows = crate::view::line_wrap::wrap_indented_block(text, "  ", width);
        let n = rows.len();
        self.live_reason = Some(LiveEntry {
            h1,
            h2,
            width,
            render: LiveRender::Plain { rows },
        });
        n
    }

    /// Test-only: how many live slots are populated (0, 1, or 2). Used to
    /// assert the streaming text never pollutes the LRU map.
    #[cfg(test)]
    pub fn live_slot_count(&self) -> usize {
        self.live_agent.is_some() as usize + self.live_reason.is_some() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::RenderCache;

    /// The cache is transparent: a cached agent render matches the direct
    /// render, and the count path (no clone) agrees with the render path.
    #[test]
    fn test_agent_cache_round_trips() {
        let text = "one two three four five six seven eight nine ten";
        let width = 12;
        let mut cache = RenderCache::default();
        let (cached_lines, cached_plain) = cache.agent_rows(text, width);
        let (direct_lines, direct_plain) =
            crate::markdown::render_agent_text("●", text, width as usize);
        assert_eq!(cached_lines.len(), direct_lines.len());
        assert_eq!(cached_plain, direct_plain);
        assert_eq!(cache.agent_row_count(text, width), cached_lines.len());
        // A repeat hit returns the same rows (the memoization contract).
        let again = cache.agent_rows(text, width);
        assert_eq!(again.1, cached_plain);
    }

    /// Width is part of the key: a long line wraps to more rows at a narrow
    /// width than a wide one, so the two cache entries must not collide.
    #[test]
    fn test_cache_keys_on_width() {
        let text = "alpha bravo charlie delta echo foxtrot golf hotel";
        let mut cache = RenderCache::default();
        let narrow = cache.agent_row_count(text, 8);
        let wide = cache.agent_row_count(text, 80);
        assert!(narrow > wide, "{narrow} should exceed {wide}");
    }

    /// The tool cache is transparent too: cached rows match the direct plan,
    /// and the count path agrees with the render path. Count and render pass
    /// the same call_id + outcome so they share one entry.
    #[test]
    fn test_tool_cache_round_trips() {
        let body = "@@ -1,2 +1,2 @@\n context\n-old line\n+new line\n";
        let mut cache = RenderCache::default();
        let cached = cache.tool_rows(body, "call_1", None, false, true, 80);
        let direct = crate::view::markers::result_body_rows(body, "call_1", None, false, true, 80);
        assert_eq!(cached.len(), direct.len());
        assert_eq!(
            cache.tool_row_count(body, "call_1", None, false, true, 80),
            cached.len()
        );
    }

    /// Memoization contract: a repeat render at the same width is a hit and
    /// does not store a new entry — the property that kills the scroll freeze
    /// (a 4000-line transcript re-renders from the cache, not by re-parsing).
    #[test]
    fn test_repeat_render_hits_cache() {
        let text = "the quick brown fox jumps over the lazy dog";
        let mut cache = RenderCache::default();
        cache.agent_rows(text, 30);
        let after_first = cache.entry_count();
        cache.agent_rows(text, 30);
        assert_eq!(cache.entry_count(), after_first);
        cache.agent_row_count(text, 30);
        assert_eq!(cache.entry_count(), after_first);
        // A distinct line adds exactly one entry; same line does not.
        cache.agent_rows("distinct content", 30);
        assert_eq!(cache.entry_count(), after_first + 1);
    }

    /// The live-streaming caches are single-slot, not LRU: streaming text that
    /// grows every delta must NOT accumulate one dead full-content entry per
    /// delta into the LRU map (which would trip the wholesale clear + reintroduce
    /// the scroll freeze). Hundreds of deltas → at most 2 live slots (one agent,
    /// one reason), and the LRU map stays empty.
    #[test]
    fn test_live_text_no_pollution() {
        let mut cache = RenderCache::default();
        // Simulate a few hundred streaming deltas (growing text).
        let base = "reasoning step by step ";
        for i in 0..200 {
            let mut s = base.repeat(i % 10 + 1);
            s.push_str(&i.to_string());
            cache.live_agent_rows(&s, 60);
            cache.live_reason_rows(&s, 60);
            cache.live_agent_row_count(&s, 60);
            cache.live_reason_row_count(&s, 60);
        }
        assert_eq!(
            cache.entry_count(),
            0,
            "streaming text must not insert into the LRU map"
        );
        assert!(
            cache.live_slot_count() <= 2,
            "at most 2 live slots (agent + reason), got {}",
            cache.live_slot_count()
        );
        // A repeat of the last content is a slot hit (no recompute — the slot
        // holds it).
        let last = format!("{}{}", base.repeat(10), 199);
        let a = cache.live_reason_rows(&last, 60);
        let b = cache.live_reason_rows(&last, 60);
        assert_eq!(a, b);
    }
}
