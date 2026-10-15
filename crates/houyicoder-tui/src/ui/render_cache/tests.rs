use super::RenderCache;
use crate::markdown::render_agent_text;
use crate::view::markers::result_body_rows;

/// The cache is transparent: a cached agent render matches the direct
/// render, and the count path (no clone) agrees with the render path.
#[test]
fn test_agent_cache_round_trips() {
    let text = "one two three four five six seven eight nine ten";
    let width = 12;
    let mut cache = RenderCache::default();
    let (cached_lines, cached_plain) = cache.agent_rows(text, width);
    let (direct_lines, direct_plain) = render_agent_text("●", text, width as usize);
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
    let direct = result_body_rows(body, "call_1", None, false, true, 80);
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
