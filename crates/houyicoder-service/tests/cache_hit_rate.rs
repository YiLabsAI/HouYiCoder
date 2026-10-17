//! Live steady-state prompt-cache measurement: one endpoint, one ratio, a
//! per-call table, a fit against the block-quantized reuse ceiling, and a
//! control that shows the endpoint's cache counter reads the prefix at all.
//! Ignored because every call spends real tokens against the configured
//! endpoint.
//!
//! The measured group is the pre-registered one: a single endpoint, a single
//! model, a single workspace, and turns whose request only appends a short tail
//! to the request before it. Each repeat opens a fresh session, so its first
//! call writes the prefix the later calls reuse and the rest of the repeat is
//! the steady state the ratio is judged over.
//!
//! That ratio is the sum of cache-read input divided by the sum of inclusive
//! input. A call enters the sum only when the endpoint reported both counts:
//! a call whose endpoint said nothing about its cache read is unknown, and
//! folding it in as a miss is the one way to make a hit rate read as something
//! that did not happen.
//!
//! What this measures is the endpoint's side of the two failure modes: the
//! request bytes are proven to extend the previous request by the offline
//! reuse check in the prefix reuse suite. Run it with the config home pointed
//! at a settings file, so the model under measurement is named rather than
//! inherited from the default:
//!
//!     HOUYICODER_CONFIG_HOME=<dir> make benchmark cache
//!
//! HOUYI_BENCH_MODEL names the model for one run, so one settings file measures
//! every model its endpoint can reach without editing the file between runs.
//!
//! The measurement also switches the two automatic memory gates off, because
//! both put calls of their own through the same provider: the memory gate
//! recalls at the start of a turn and hands the session to the extractor at its
//! end, and the dream gate runs its consolidation pass on its own schedule. With
//! both off, a turn is exactly one endpoint call unless that session compacts or
//! a reply cut at the output cap is re-called, neither of which these runs
//! reach, and a turn's cached read is read off the endpoint's own report for
//! that call. Gates off also keep the memory index out of the static segment, so
//! what a call reuses here is the prompt and the tool definitions.
//!
//! An endpoint that reuses from the start of a request in fixed blocks can
//! reuse at most floor(previous input / block) * block tokens of the call that
//! follows: a partly filled trailing block is not reusable. The table fits that
//! prediction at five candidate block sizes, carries every candidate's
//! prediction and residual on each row, and totals them per candidate, so the
//! sizes the rows agree with can be read off the table rather than assumed.
//! Several sizes can fit one session's turns equally well, so a size picked by a
//! tie-break is not a measured block size, and the ceiling printed beside the
//! ratio is the arithmetic ideal for the selected block rather than a bound the
//! endpoint cannot exceed.
//!
//! HOUYI_BENCH_TURN_DELAY_MS puts a gap between the turns of a session, which
//! the harness otherwise leaves at zero. The gap is a diagnostic on top of the
//! measurement rather than part of it: a real session leaves time between the
//! call that writes a prefix and the call that reads it back, and a run with a
//! gap re-reads the same rows with that difference present. It settles the short
//! reads only if a gapped run's readings separate from an ungapped run's, and
//! two runs separated in time differ in more than the gap: a gapped reading
//! above an ungapped one taken earlier is not yet evidence for the gap.
//!
//! One group is measured: the cache form the resolved catalog declares, which
//! is what the product sends. That group is named for the declaration rather
//! than for a form, because a model with no declaration sends the
//! automatic-prefix form while a declared one sends whatever it declared. A
//! second group forces the positional-marker form onto the request, which is the
//! request bytes a catalog entry declaring positionalMarkers produces;
//! HOUYI_BENCH_COMPARE_FORMS=1 measures it too, over the same session shape, so
//! the two readings answer whether this endpoint honors the markers the second
//! form sends. It is off by default because the comparison is a property of the
//! endpoint rather than of the model, so one model settles it for every model
//! behind the same endpoint.
//!
//! The control runs last, over the same turns in a shorter session: one
//! character of the static segment is changed on the named turn's own call. A
//! counter that reads the prefix must fall on that call and recover on the next;
//! a counter that stayed where it was would not be reading the prefix at all,
//! and every row above it would be void.

use std::env::{temp_dir, var};
use std::fs::{create_dir_all, write};
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use houyicoder_api::provider::ModelProvider;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::SessionEvent;
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_protocol::cache_policy::PromptCacheFormat;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, InputItem, LlmEvent, ModelCapabilities, ProviderError,
    UsageProvenance,
};
use houyicoder_service::composition::AssembledRunner;
use houyicoder_service::composition::BuildRunnerOptions;
use houyicoder_service::composition::ProviderResolution;
use houyicoder_service::composition::build_runner;
use houyicoder_service::composition::resolve_provider;
use tokio::time::sleep;

/// Sessions measured. The pre-registered group requires at least three
/// independent repeats.
const REPEATS: usize = 3;

/// Turns per session. The first call writes the prefix, so a repeat yields one
/// prefix-building call plus the steady state after it.
const TURNS: usize = 12;

/// The pre-registered minimum of measured calls, so a run that recorded
/// nothing says so instead of printing a ratio over one or two calls.
const MIN_MEASURED_CALLS: usize = 10;

/// Block sizes the ceiling is fitted at: the size the observed reads
/// pre-registered and its neighbours, plus one much larger block, because a
/// model whose rows all sit inside a single block of that size fits it exactly
/// without that fit showing the block exists: over such rows a large block and a
/// constant reusable prefix predict the same reads.
const BLOCK_CANDIDATES: [u64; 5] = [64, 128, 256, 512, 1024];

/// The block size the observed reads pre-registered.
const REGISTERED_BLOCK: u64 = 128;

/// Turns in the control session: enough steady turns to establish the reuse
/// before the changed character, and enough after it to watch it return.
const CONTROL_TURNS: usize = 8;

/// The turn whose request carries the changed character. This is a turn number,
/// not a position among calls: the call carrying the change is found by the
/// prompt that turn sends.
const CONTROL_TURN: usize = 6;

/// Names the model one run measures, over the endpoint and credential the
/// settings file resolves.
const MODEL_ENV: &str = "HOUYI_BENCH_MODEL";

/// Measures the second cache form as well when set to 1.
const COMPARE_FORMS_ENV: &str = "HOUYI_BENCH_COMPARE_FORMS";

/// Milliseconds to wait between the turns of one session, when set. A real
/// session leaves a gap between the request that writes a prefix and the one
/// that reads it back, because the turn in between ran tools; this harness
/// leaves none, so its rows cannot tell an endpoint whose write lands late from
/// one that routes the next call elsewhere. Setting a gap re-reads the same rows
/// with that difference present, which separates the two only if the gapped
/// readings do not overlap the ungapped ones.
const TURN_DELAY_ENV: &str = "HOUYI_BENCH_TURN_DELAY_MS";

/// One endpoint call, as the durable log recorded it.
struct Call {
    turn: u32,
    input: u64,
    cached: u64,
    provenance: UsageProvenance,
    /// True when this call continued a turn whose prior call hit the output
    /// ceiling. The turn then has more than one row under its number.
    recovery: bool,
}

impl Call {
    /// Whether the endpoint reported enough of the cache account for this call
    /// to enter the ratio.
    fn measured(&self) -> bool {
        self.provenance.cache_account_complete()
    }

    /// The reported account, so a row says which parts of the cache account it
    /// carries rather than only whether the call entered the ratio.
    fn account(&self) -> &'static str {
        match (
            self.provenance.input_reported,
            self.provenance.cache_read_reported,
            self.provenance.cache_write_reported,
        ) {
            (true, true, true) => "input+read+write",
            (true, true, false) => "input+read",
            (true, false, true) => "input+write",
            (true, false, false) => "input only",
            (false, true, true) => "read+write",
            (false, true, false) => "read only",
            (false, false, true) => "write only",
            (false, false, false) => "none",
        }
    }
}

/// The call the named turn ended with, if the session recorded one. A turn that
/// re-called after hitting the output ceiling recorded one row for every call it
/// made, and the row this reads is the one whose cache read is the whole turn's
/// request; the continuation rows before it are shorter calls of the same turn.
fn ended_call(calls: &[Call], turn: usize) -> Option<&Call> {
    calls
        .iter()
        .rfind(|call| call.turn == turn as u32 && !call.recovery)
}

/// What the measured group read: the ratio, how many calls entered it, and the
/// table that led to it.
struct GroupReading {
    ratio: f64,
    measured: usize,
    table: String,
}

/// The fit of one block size over the measured rows.
struct BlockFit {
    block: u64,
    predicted: u64,
    observed: u64,
    exact: usize,
    worst: i64,
    rows: usize,
    /// The turn of every call a row was read from, in order, so the rows the fit
    /// read are the rows it says it read rather than a count of them.
    turns: Vec<u32>,
}

impl BlockFit {
    fn empty(block: u64) -> Self {
        Self {
            block,
            predicted: 0,
            observed: 0,
            exact: 0,
            worst: 0,
            rows: 0,
            turns: Vec::new(),
        }
    }

    /// Fold another session's fit into this one. A ceiling compares a call with
    /// the call before it, so the fits are summed across sessions but never
    /// across a session boundary.
    fn fold(&mut self, other: Self) {
        self.predicted += other.predicted;
        self.observed += other.observed;
        self.exact += other.exact;
        self.worst = self.worst.max(other.worst);
        self.rows += other.rows;
        self.turns.extend(other.turns);
    }
}

/// One workspace for every repeat: a stable directory, so the system prompt,
/// the tool list, and the project rules are the same bytes each time and the
/// request differs only by the tail.
fn workspace() -> PathBuf {
    let dir = temp_dir().join("houyi-cache-hit-rate");
    create_dir_all(&dir).expect("measurement workspace");
    dir
}

/// The calls one session recorded, in order. Read from the log rather than from
/// the cumulative tally: the log keeps one row per endpoint call, so a turn
/// that re-called contributes each call rather than their sum alone.
fn session_calls(runner: &Runner, session: SessionId) -> Vec<Call> {
    runner
        .store()
        .trajectory_tail(session)
        .into_iter()
        .filter_map(|entry| match entry.event {
            SessionEvent::TurnUsage {
                turn,
                input_tokens,
                cache_read_input_tokens,
                input_reported,
                cache_read_reported,
                cache_write_reported,
                recovery,
                ..
            } => Some(Call {
                turn,
                input: input_tokens,
                cached: cache_read_input_tokens,
                provenance: UsageProvenance {
                    input_reported,
                    cache_read_reported,
                    cache_write_reported,
                },
                recovery,
            }),
            _ => None,
        })
        .collect()
}

/// The options every runner in this measurement is built from.
fn base_options(project: &str) -> BuildRunnerOptions {
    let mut options = BuildRunnerOptions::default();
    options.project = Some(project.to_string());
    options
}

/// A runner over the measurement workspace, assembled by the production root so
/// every request is built the way the product builds it, with the endpoint
/// wrapped so the request can be read and rewritten on its way there. On top of
/// that the model under measurement is set here, and both automatic memory gates
/// are switched off: each of them reaches this same provider, so switching them
/// off is what makes one turn one call. The endpoint itself is the resolved one,
/// so every reading comes from the endpoint the product would talk to. The
/// wrapper comes back too: the control reads from it whether its change landed.
fn open(
    project: &str,
    form: Option<PromptCacheFormat>,
    target: Option<usize>,
) -> (AssembledRunner, Arc<TestProvider>) {
    let resolved = resolve_provider(Some(project));
    let mut options = base_options(project);
    let provider = Arc::new(TestProvider {
        endpoint: Arc::clone(&resolved.provider),
        form,
        target: target.map(turn_prompt),
        flips: AtomicUsize::new(0),
        first_request: Mutex::new(None),
    });
    options.provider = Some(ProviderResolution {
        provider: Arc::clone(&provider) as Arc<dyn ModelProvider>,
        facts: resolved.facts,
        warnings: resolved.warnings,
    });
    let assembled = build_runner(options);
    if let Some(model) = var(MODEL_ENV).ok().filter(|model| !model.is_empty()) {
        assembled.runner.set_model(model);
    }
    assembled.runner.set_auto_memory(false);
    assembled.runner.set_auto_dream(false);
    (assembled, provider)
}

/// The cache forms the measurement compares, under the names the table prints
/// them by. The declared name leaves the request as the product built it; the
/// positionalMarkers name overrides the form on the request, which is the
/// request bytes a catalog entry declaring that form produces.
fn cache_forms() -> Vec<(&'static str, Option<PromptCacheFormat>)> {
    let mut forms = vec![("declared", None)];
    if var(COMPARE_FORMS_ENV).ok().as_deref() == Some("1") {
        forms.push((
            "positionalMarkers",
            Some(PromptCacheFormat::PositionalMarkers),
        ));
    }
    forms
}

/// The name a cache form prints under, so the form a request declared reads in
/// the same words as the group name that produced it.
fn form_name(form: PromptCacheFormat) -> &'static str {
    match form {
        PromptCacheFormat::AutoPrefix => "autoPrefix",
        PromptCacheFormat::PositionalMarkers => "positionalMarkers",
    }
}

/// What the first request the endpoint received was made of: the size of the
/// static segment the cache key is taken from, in characters, against the whole
/// instruction text, and the cache form it declared on arrival.
struct RequestShape {
    boundary: Option<usize>,
    static_chars: usize,
    total_chars: usize,
    format: PromptCacheFormat,
}

/// The endpoint as the measurement uses it. Every call goes through to the real
/// endpoint; what this adds is the two things the measurement varies on top of
/// the product's own request: the cache form the request declares, and, for the
/// control, one changed character in the static segment of one named turn.
///
/// The declared form is set here rather than in the catalog because the request
/// bytes are the same either way: a catalog declaration reaches the request only
/// as this field, so setting it directly measures what that declaration sends.
struct TestProvider {
    endpoint: Arc<dyn ModelProvider>,
    form: Option<PromptCacheFormat>,
    target: Option<String>,
    flips: AtomicUsize,
    first_request: Mutex<Option<RequestShape>>,
}

impl TestProvider {
    /// Whether this is the named turn's own call: the last user item of its
    /// input is the prompt that turn sends.
    ///
    /// The turn is named by its prompt rather than by the position of the call,
    /// so the change follows the turn even if some other call reaches the
    /// endpoint between two of them.
    fn matches(&self, req: &CompletionRequest) -> bool {
        self.target
            .as_ref()
            .is_some_and(|prompt| last_user_prompt_is(&req.input, prompt))
    }

    /// The request as the endpoint receives it.
    fn rewrite(&self, mut req: CompletionRequest) -> CompletionRequest {
        if let Ok(mut first) = self.first_request.lock()
            && first.is_none()
        {
            *first = Some(RequestShape {
                boundary: req.dynamic_boundary,
                static_chars: req
                    .dynamic_boundary
                    .filter(|offset| *offset <= req.instructions.len())
                    .unwrap_or(req.instructions.len()),
                total_chars: req.instructions.len(),
                format: req.cache_format,
            });
        }
        if let Some(form) = self.form {
            req.cache_format = form;
        }
        if self.matches(&req) {
            req.instructions = flip_prefix(&req.instructions);
            self.flips.fetch_add(1, Ordering::SeqCst);
        }
        req
    }

    /// How many calls carried the changed character. A control that changed no
    /// call has no reading at all, so this count is asserted before the reads.
    fn flips(&self) -> usize {
        self.flips.load(Ordering::SeqCst)
    }

    /// The shape of the first request, for the report: whether the request
    /// carried a dynamic boundary, the size of the segment before it against
    /// the whole instruction text, and the cache form the request declared
    /// before this wrapper touched it.
    fn first_request(&self) -> Option<RequestShape> {
        let first = self.first_request.lock().ok()?;
        first.as_ref().map(|shape| RequestShape {
            boundary: shape.boundary,
            static_chars: shape.static_chars,
            total_chars: shape.total_chars,
            format: shape.format,
        })
    }
}

impl ModelProvider for TestProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.endpoint.complete(self.rewrite(req))
    }

    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        self.endpoint.stream(self.rewrite(req))
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.endpoint.capabilities()
    }
}

/// Whether the last user item of the input is the named prompt.
///
/// The last item is what makes this the named turn's own call. Model input only
/// grows, so every earlier turn's prompt is still in the input of every later
/// turn: a rule that matched any item would change the static segment on each
/// turn after the named one as well, and the control would read a run of changed
/// turns rather than one.
fn last_user_prompt_is(input: &[InputItem], prompt: &str) -> bool {
    input
        .iter()
        .filter_map(|item| match item {
            InputItem::User { content } => Some(content),
            _ => None,
        })
        .next_back()
        .is_some_and(|content| content == prompt)
}

/// The same text with its first character replaced by one drawn from this
/// process's id.
///
/// The replacement is drawn per process rather than fixed because the endpoint
/// keeps its cache across runs: a fixed replacement makes every run after the
/// first send a request the endpoint has already cached, whose read is then the
/// whole prefix, and the control would read as a counter that ignored the change.
///
/// Everything after the first character is untouched, so the changed request
/// differs from the unchanged one in that character and nothing else. A reader
/// that answers from the bytes before it cannot match anything after it.
fn flip_prefix(text: &str) -> String {
    // CJK Unified Ideographs: a wide range, so two runs rarely draw the same
    // replacement character.
    let drawn = char::from_u32(0x4E00 + id() as u32 % 0x2000)
        .expect("the drawn code point is inside the range");
    let mut chars: Vec<char> = text.chars().collect();
    match chars.first_mut() {
        Some(first) => *first = drawn,
        None => chars.push(drawn),
    }
    chars.into_iter().collect()
}

/// The prompt one turn sends. The control names a turn by this text, so both
/// paths build it the same way.
fn turn_prompt(turn: usize) -> String {
    format!("Reply with the single word ack. This is message {turn}.")
}

/// Drive one session and return the calls it recorded. The prompts ask for a
/// bare word: the measurement needs the request to grow by a tail, not to make
/// the model reach for a tool.
async fn drive(assembled: &AssembledRunner, turns: usize) -> Vec<Call> {
    let runner = &assembled.runner;
    let session = assembled.session;
    let gap = turn_gap();
    for turn in 1..=turns {
        if turn > 1
            && let Some(gap) = gap
        {
            sleep(gap).await;
        }
        runner
            .run(session, turn_prompt(turn))
            .await
            .unwrap_or_else(|e| panic!("turn {turn} failed: {e:?}"));
    }
    session_calls(runner, session)
}

/// The gap this run leaves between turns, from the environment, or None for
/// none. An unset or zero value leaves no gap. Any other value that is not a
/// whole number of milliseconds stops the run: a run asked for a gap it did not
/// take would be read as a reading about that gap.
fn turn_gap() -> Option<Duration> {
    let raw = var(TURN_DELAY_ENV).ok()?;
    let millis = raw.parse::<u64>().unwrap_or_else(|e| {
        panic!("{TURN_DELAY_ENV} must be a whole number of milliseconds, not {raw:?}: {e}")
    });
    (millis > 0).then(|| Duration::from_millis(millis))
}

/// The reuse a call of the named turn can report, given the call before it. None
/// when the two calls cannot be compared: a call the endpoint did not fully
/// account for says nothing about the prefix, a predecessor that reported no
/// input leaves nothing to reuse, and a turn that re-called reads its own second
/// call rather than a window across turns.
///
/// The fit and the printed table both read their windows through this, so a row
/// the table prints with a reuse is a row the fit counted.
fn predicted_reuse(previous: &Call, current: &Call, block: u64) -> Option<u64> {
    let readable = previous.measured() && current.measured() && previous.input > 0;
    let same_turn = previous.turn == current.turn;
    (readable && !same_turn).then(|| previous.input / block * block)
}

/// Fit one session's rows at one block size.
fn fit_session(calls: &[Call], block: u64) -> BlockFit {
    let mut fit = BlockFit::empty(block);
    for window in calls.windows(2) {
        let (previous, current) = (&window[0], &window[1]);
        let Some(predicted) = predicted_reuse(previous, current, block) else {
            continue;
        };
        let residual = current.cached as i64 - predicted as i64;
        fit.predicted += predicted;
        fit.observed += current.cached;
        fit.rows += 1;
        fit.turns.push(current.turn);
        if residual == 0 {
            fit.exact += 1;
        }
        fit.worst = fit.worst.max(residual.abs());
    }
    fit
}

/// The candidate block sizes in the order a row lists its per-candidate figures,
/// so the header naming them and the rows carrying them cannot come apart.
fn candidate_list() -> String {
    BLOCK_CANDIDATES
        .iter()
        .map(|block| block.to_string())
        .collect::<Vec<String>>()
        .join("/")
}

/// Every candidate block size fitted over the whole group.
fn fit_group(all: &[Vec<Call>]) -> Vec<BlockFit> {
    BLOCK_CANDIDATES
        .iter()
        .map(|block| {
            let mut fit = BlockFit::empty(*block);
            for calls in all {
                fit.fold(fit_session(calls, *block));
            }
            fit
        })
        .collect()
}

/// The candidate block size the rows actually match, if any candidate does: the
/// most rows with no residual, then the smallest worst residual, then the
/// smallest block so a full tie reads the same way every run. A candidate that
/// predicts no row at all reads nothing off these rows, so the ceiling is taken
/// from a candidate the rows agree with rather than from the pre-registered one.
fn best_fit(fits: &[BlockFit]) -> Option<&BlockFit> {
    fits.iter()
        .filter(|fit| fit.exact > 0)
        .max_by(|left, right| {
            left.exact
                .cmp(&right.exact)
                .then_with(|| right.worst.cmp(&left.worst))
                .then_with(|| right.block.cmp(&left.block))
        })
}

/// Build the per-call table, the fits, and the group ratio. The header names
/// the model the rows came from, so a table kept beside the measurement says
/// what it measured without the run's stdout beside it. The group ratio is one
/// sum over the sums, not a per-call share: each call's own share is printed
/// from the derivation the panes read.
fn report(model: &str, label: &str, all: &[Vec<Call>]) -> GroupReading {
    let mut cached = 0u64;
    let mut input = 0u64;
    let mut measured = 0usize;
    let mut excluded = 0usize;
    let mut table = String::new();
    table.push_str(&format!(
        "group {label} on {model}: {} repeats, {TURNS} turns each; each row's reuse and \
         residual are listed per candidate block {}\n",
        all.len(),
        candidate_list()
    ));
    for (index, calls) in all.iter().enumerate() {
        let first = calls.first();
        table.push_str(&format!(
            "repeat {}: {} calls; first call input {} cached {}\n",
            index + 1,
            calls.len(),
            first.map_or(0, |c| c.input),
            first.map_or(0, |c| c.cached)
        ));
        for window in calls.windows(2) {
            let (previous, call) = (&window[0], &window[1]);
            if !call.measured() {
                excluded += 1;
                table.push_str(&format!(
                    "  turn {:<3} excluded: the endpoint reported {} of the cache account\n",
                    call.turn,
                    call.account()
                ));
                continue;
            }
            measured += 1;
            cached += call.cached;
            input += call.input;
            // The share of one call comes from the same derivation every pane
            // reads, so a call whose share is unknown prints as unknown rather
            // than as a zero share.
            let share = match call.provenance.cache_hit_pct(call.cached, call.input) {
                Some(pct) => format!("{pct:>5.1}%"),
                None => "    -".to_string(),
            };
            // Every candidate's figure is listed on the row, in the order the
            // header names, so a row that one candidate explains and another
            // does not is visible without re-running the fit. A candidate whose
            // window reads no reuse prints a dash rather than a zero.
            let (reuse, residual): (Vec<String>, Vec<String>) = BLOCK_CANDIDATES
                .iter()
                .map(|block| predicted_reuse(previous, call, *block))
                .map(|predicted| match predicted {
                    Some(predicted) => (
                        predicted.to_string(),
                        format!("{:+}", call.cached as i64 - predicted as i64),
                    ),
                    None => ("-".to_string(), "-".to_string()),
                })
                .unzip();
            table.push_str(&format!(
                "  turn {:<3} input {:<7} cached {:<7} share {share}  account {:<16} \
                 previous input {:<7} reuse {}  residual {}\n",
                call.turn,
                call.input,
                call.cached,
                call.account(),
                previous.input,
                reuse.join("/"),
                residual.join("/"),
            ));
        }
    }
    let ratio = if input == 0 {
        0.0
    } else {
        cached as f64 * 100.0 / input as f64
    };
    table.push_str(&format!(
        "steady state: {cached} cache-read / {input} inclusive input = {ratio:.2}% over \
         {measured} calls ({excluded} excluded for an account the endpoint left incomplete); \
         each repeat's first call writes the prefix, is not compared with one, and sits in \
         neither sum\n"
    ));
    let fits = fit_group(all);
    for fit in &fits {
        table.push_str(&format!(
            "block {:<4} predicted {:<8} observed {:<8} exact {:<3} of {:<3} worst residual {}\n",
            fit.block, fit.predicted, fit.observed, fit.exact, fit.rows, fit.worst
        ));
    }
    match best_fit(&fits).filter(|_| input > 0) {
        Some(fit) => {
            let ceiling = fit.predicted as f64 * 100.0 / input as f64;
            table.push_str(&format!(
                "ceiling at block {} (the arithmetic ideal for that block, not a bound the \
                 endpoint cannot exceed): {} reusable of {input} inclusive input = {ceiling:.2}%\n",
                fit.block, fit.predicted
            ));
        }
        None => table.push_str(
            "ceiling: no candidate block predicts a single row, so these rows read off no \
             ceiling\n",
        ),
    }
    GroupReading {
        ratio,
        measured,
        table,
    }
}

/// What the control read: the cache read of the turn before the changed
/// character, of the turn carrying it, and of the turn after, plus the shape of
/// the request the endpoint received.
struct ControlReading {
    before: u64,
    changed: u64,
    after: u64,
    flips: usize,
    shape: Option<RequestShape>,
    rows: Vec<(u32, u64, u64)>,
}

/// Drive the control session and read the turns around the changed character.
/// Every row is read by its turn number, never by position: the recorded rows
/// are the durable turn log's, and position would silently read whatever turn
/// happened to land on that number.
async fn run_control(project: &str) -> ControlReading {
    let (assembled, wrapper) = open(project, None, Some(CONTROL_TURN));
    let calls = drive(&assembled, CONTROL_TURNS).await;
    let cached_of = |turn: usize| {
        ended_call(&calls, turn)
            .map(|call| call.cached)
            .unwrap_or_else(|| {
                panic!(
                    "the control turns on turn {turn}, which the session did not record; it \
                     recorded turns {:?}",
                    calls.iter().map(|call| call.turn).collect::<Vec<_>>()
                )
            })
    };
    ControlReading {
        before: cached_of(CONTROL_TURN - 1),
        changed: cached_of(CONTROL_TURN),
        after: cached_of(CONTROL_TURN + 1),
        flips: wrapper.flips(),
        shape: wrapper.first_request(),
        rows: calls
            .iter()
            .map(|call| (call.turn, call.input, call.cached))
            .collect(),
    }
}

/// Write the table beside the measurement workspace. The name carries the group
/// and the process, so a group is not written over its sibling and two
/// measurements running at once do not write over each other.
fn persist(label: &str, table: &str) -> PathBuf {
    let path = workspace().join(format!("steady-state-{label}-{}.txt", id()));
    write(&path, table).expect("write the measurement table");
    path
}

#[tokio::test]
#[ignore = "calls a real endpoint; HOUYI_BENCH_MODEL names the model it measures"]
async fn test_steady_state_hit_rate() {
    let project = workspace();
    let project = project.display().to_string();
    let (probe, _) = open(&project, None, None);
    let facts = &probe.provider_facts;
    let model = probe.runner.status_snapshot().model;
    println!(
        "endpoint {} key {} model {model}",
        facts.base_url,
        facts.key_origin.as_deref().unwrap_or("none")
    );
    // A run with no key answers from the canned stub, which reports no cache
    // account at all. Measuring that would print a ratio about the stub, so
    // refuse it instead of reporting it.
    assert!(
        facts.key_origin.is_some(),
        "no credential resolved for {}; a stubbed run is not a measurement",
        facts.base_url
    );

    let mut readings = Vec::new();
    for (label, form) in cache_forms() {
        let mut all = Vec::new();
        for _ in 0..REPEATS {
            let (assembled, _) = open(&project, form, None);
            all.push(drive(&assembled, TURNS).await);
        }
        let reading = report(&model, label, &all);
        print!("{}", reading.table);
        let path = persist(label, &reading.table);
        println!("table written to {}", path.display());
        assert!(
            reading.measured >= MIN_MEASURED_CALLS,
            "group {label} needs at least {MIN_MEASURED_CALLS} measured calls, got {}",
            reading.measured
        );
        assert!(
            (0.0..=100.0).contains(&reading.ratio),
            "a share of the input cannot fall outside 0..100"
        );
        readings.push((label, reading));
    }
    for (label, reading) in &readings {
        println!(
            "group {label}: {:.2}% over {} calls",
            reading.ratio, reading.measured
        );
    }

    let control = run_control(&project).await;
    println!("control turns:");
    for (turn, input, cached) in &control.rows {
        println!("  turn {turn:<3} input {input:<7} cached {cached}");
    }
    match &control.shape {
        Some(shape) => println!(
            "control request: static segment {} chars of {} ({}), declaring {}",
            shape.static_chars,
            shape.total_chars,
            if shape.boundary.is_some() {
                "a dynamic boundary was set"
            } else {
                "no dynamic boundary"
            },
            form_name(shape.format)
        ),
        None => println!("control request: the endpoint saw no request"),
    }
    println!(
        "control: cache read {} on the turn before the change, {} on it, {} after it; the change \
         landed on {} call(s)",
        control.before, control.changed, control.after, control.flips
    );
    // A change that never reached the endpoint says nothing about the counter, so
    // it fails here rather than reading as a counter that ignored the prefix.
    //
    // The count itself is reported rather than guarded; the two directions below
    // are the guard, and between them they hold only for a change that landed on
    // the named turn's own call: a turn the change missed keeps the read of the
    // turn before it, so the first fails, and a change that also landed on the
    // following turn leaves it no higher than the changed one, so the second
    // fails. A count above one is a fact about which calls carried the change.
    assert!(
        control.flips >= 1,
        "the change reached no call at all; the control has no reading"
    );
    // The guard is on the direction, not on how far the read falls. Part of the
    // prompt can precede the static segment (a tool list rendered ahead of it
    // stays reusable), so the size of the drop is a fact about how the endpoint
    // arranges the prompt, while falling at all is what a counter that reads the
    // prefix must do when the prefix changes.
    assert!(
        control.changed < control.before,
        "a changed static segment must reduce the cache read: read {} before the change / {} on \
         it",
        control.before,
        control.changed
    );
    assert!(
        control.after > control.changed,
        "the cache read must recover on the turn after the changed one: read {} on the change / \
         {} after it",
        control.changed,
        control.after
    );
}

/// A recorded call for the offline checks: the endpoint reported the whole
/// account unless the check says otherwise.
fn recorded_call(turn: u32, input: u64, cached: u64) -> Call {
    Call {
        turn,
        input,
        cached,
        provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: true,
        },
        recovery: false,
    }
}

/// Where the registered block sits in the candidate list a row prints its
/// figures in.
fn registered_position() -> usize {
    BLOCK_CANDIDATES
        .iter()
        .position(|block| *block == REGISTERED_BLOCK)
        .expect("the registered block is one of the candidates")
}

/// The reuse each printed row carries at the registered block, as the table
/// shows it, keyed by the turn the row names, with an empty entry for a row that
/// prints none.
fn printed_rows(table: &str) -> Vec<(u32, Option<u64>)> {
    let position = registered_position();
    table
        .lines()
        .filter_map(|line| {
            let (head, rest) = line.split_once(" reuse ")?;
            let turn = head
                .trim_start()
                .strip_prefix("turn ")?
                .split_whitespace()
                .next()?;
            let listed = rest.split_whitespace().next()?.split('/').nth(position)?;
            Some((turn.parse::<u32>().ok()?, listed.parse::<u64>().ok()))
        })
        .collect()
}

/// The turn is the call whose last user item is that turn's prompt. The
/// endpoint's cache is read across runs, so a rule that matched any user item
/// would change the static segment of every turn after the named one too, and
/// the control would read a run of changed turns as one.
#[test]
fn test_change_reads_last_prompt() {
    let target = turn_prompt(CONTROL_TURN);
    let history = vec![
        InputItem::User {
            content: target.clone(),
        },
        InputItem::Assistant {
            content: "ack".to_string(),
            tool_calls: Vec::new(),
        },
        InputItem::User {
            content: turn_prompt(CONTROL_TURN + 1),
        },
    ];
    assert!(
        !last_user_prompt_is(&history, &target),
        "an earlier turn's prompt is history in this call, not its own"
    );
    let own = vec![
        InputItem::User {
            content: turn_prompt(CONTROL_TURN - 1),
        },
        InputItem::Assistant {
            content: "ack".to_string(),
            tool_calls: Vec::new(),
        },
        InputItem::User {
            content: target.clone(),
        },
    ];
    assert!(last_user_prompt_is(&own, &target));
}

/// Only a window the endpoint fully accounted for, whose earlier call sent
/// input, and whose two calls are different turns, reads a reuse.
#[test]
fn test_reuse_needs_readable_window() {
    let previous = recorded_call(2, 1000, 768);
    let current = recorded_call(3, 1020, 896);
    assert_eq!(predicted_reuse(&previous, &current, 128), Some(896));

    let mut unaccounted = recorded_call(2, 1000, 768);
    unaccounted.provenance.cache_read_reported = false;
    assert_eq!(predicted_reuse(&unaccounted, &current, 128), None);

    let no_input = recorded_call(2, 0, 0);
    assert_eq!(predicted_reuse(&no_input, &current, 128), None);

    let continuation = recorded_call(3, 1040, 1024);
    assert_eq!(predicted_reuse(&current, &continuation, 128), None);
}

/// The printed table and the fit read their windows through one rule: the rows
/// the table prints with a reuse are the rows the fit counted, by turn and by
/// value, and every other row prints none. The corpus covers two ways a window
/// can fail to read a reuse — a call the endpoint did not account for, excluded
/// from the ratio and leaving the windows on either side of it reading none, and
/// a call that continued its own turn — so a rule that admitted either would
/// print or count a row the other side does not. The fit's own totals are
/// asserted beside the rows, because a wrong exact count selects a different
/// block for the ceiling while every row stays accounted for.
#[test]
fn test_fit_counts_printed_rows() {
    let mut unaccounted = recorded_call(2, 0, 0);
    unaccounted.provenance.input_reported = false;
    let mut truncated = recorded_call(5, 1400, 1280);
    truncated.recovery = true;
    let calls = vec![
        recorded_call(1, 1000, 768),
        unaccounted,
        recorded_call(3, 1200, 1024),
        recorded_call(4, 1220, 1152),
        truncated,
        recorded_call(5, 1440, 1408),
    ];
    let all = vec![calls];
    let reading = report("a-model", "declared", &all);
    let fit = fit_session(&all[0], REGISTERED_BLOCK);
    let expected: Vec<(u32, Option<u64>)> = all[0]
        .windows(2)
        .filter(|window| window[1].measured())
        .map(|window| {
            (
                window[1].turn,
                predicted_reuse(&window[0], &window[1], REGISTERED_BLOCK),
            )
        })
        .collect();
    let counted: Vec<u32> = expected
        .iter()
        .filter(|(_, reuse)| reuse.is_some())
        .map(|(turn, _)| *turn)
        .collect();
    assert_eq!(
        printed_rows(&reading.table),
        expected,
        "every printed row carries its own turn and the reuse the one rule reads for it"
    );
    assert_eq!(
        fit.turns, counted,
        "the fit counted those same rows and no others"
    );
    assert!(
        counted.len() < expected.len(),
        "the corpus keeps a row that reads no reuse"
    );
    assert_eq!(
        (fit.rows, fit.exact, fit.worst),
        (counted.len(), 1, 128),
        "the fit totals the rows it counted: turn 4 lands exactly on the floor of turn 3's \
         input, turn 5 reads one block above that floor, and the continuation of turn 5 \
         predicts nothing"
    );
}

/// The fit's totals are read off hand-computed rows: two rows the registered
/// block predicts exactly, and two windows a call that reported no input makes
/// unpredictable, since it neither gets a prediction of its own nor supplies the
/// previous input the next window would need.
#[test]
fn test_fit_session_totals_rows() {
    let mut no_input = recorded_call(4, 0, 0);
    no_input.provenance.input_reported = false;
    let fit = fit_session(
        &[
            recorded_call(1, 1024, 768),
            recorded_call(2, 1200, 1024),
            recorded_call(3, 1500, 1152),
            no_input,
            recorded_call(5, 1400, 1280),
        ],
        128,
    );
    assert_eq!(
        (fit.predicted, fit.observed, fit.rows, fit.exact, fit.worst),
        (1024 + 1152, 1024 + 1152, 2, 2, 0),
    );
    assert_eq!(fit.turns, vec![2, 3]);
}

/// A candidate that matches no row reads nothing off these rows, so no ceiling
/// is taken: the fit that would be selected is the one with an exact row, and a
/// group whose rows no candidate explains states that instead of printing a
/// ceiling from a tie-break.
#[test]
fn test_report_states_no_ceiling() {
    let fits = vec![BlockFit::empty(128), BlockFit::empty(256)];
    assert!(best_fit(&fits).is_none(), "no candidate matched a row");

    // Every cached read above the floor of the call before it: no candidate
    // predicts a row exactly, and the ratio is still reported.
    let calls = vec![recorded_call(1, 1000, 900), recorded_call(2, 1500, 1500)];
    let reading = report("a-model", "declared", &[calls]);
    assert!(
        reading.table.contains("read off no ceiling"),
        "a group no candidate explains says so instead of naming a block: {}",
        reading.table
    );
    assert_eq!(reading.measured, 1);
}

/// The candidate the ceiling comes from is the one the rows match: most exact
/// rows first, then the smallest worst residual, then the smallest block so a
/// full tie reads the same way every run.
#[test]
fn test_fit_prefers_exact_rows() {
    let mut coarse = BlockFit::empty(64);
    coarse.exact = 3;
    coarse.worst = 400;
    let mut exact = BlockFit::empty(128);
    exact.exact = 3;
    exact.worst = 100;
    let mut none = BlockFit::empty(256);
    none.worst = 0;
    let mut tied = BlockFit::empty(512);
    tied.exact = 3;
    tied.worst = 100;
    let fits = vec![coarse, exact, none, tied];
    let best = best_fit(&fits).expect("a candidate matches the rows");
    assert_eq!(best.block, 128, "the tie goes to the smaller block");
}

/// The change replaces the first character and leaves the rest of the text
/// where it was, so the changed request differs from the unchanged one in that
/// one character and in nothing else.
#[test]
fn test_change_alters_first_char() {
    let before = turn_prompt(1);
    let after = flip_prefix(&before);
    assert_ne!(after, before);
    assert_eq!(after.chars().count(), before.chars().count());
    assert_eq!(
        after.chars().skip(1).collect::<String>(),
        before.chars().skip(1).collect::<String>()
    );
}

/// A turn that re-called after hitting the output ceiling recorded more than one
/// row under its number; the reading the control compares is the call the turn
/// ended with.
#[test]
fn test_ending_call_follows_retry() {
    let mut truncated = recorded_call(4, 900, 800);
    truncated.recovery = true;
    let calls = vec![
        recorded_call(3, 800, 700),
        truncated,
        recorded_call(4, 1100, 1024),
        recorded_call(5, 1200, 1100),
    ];
    let ended = ended_call(&calls, 4).expect("the turn recorded a call");
    assert_eq!(ended.input, 1100);
}
