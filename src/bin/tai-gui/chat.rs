//! The chat pane: a conversation with ppxai through the engine link
//! (`ppxai.rs`), its answers rendered as Markdown. It follows ppxai's
//! desktop web app (`ppxai/web/app.js`), ADR 0010's chat steps:
//!
//! - A prompt goes to the pane's conversation, `/chat`, and its answer
//!   streams in. ppxai keeps the history per engine slot. The model's
//!   reasoning shows in a collapsed "Thought process", and tool calls in a
//!   collapsed list of steps.
//! - `/run <prompt>` asks one-shot (`/v1/oneshot`), outside the
//!   conversation and grounded on the web when ppxai is set to retrieve;
//!   its `[n]` markers link to the sources it lists. The web app sends
//!   `/run` to `/v1/agent/run`, the same one-shot as a background run, but
//!   that route doesn't return the sources yet (ppxai is reworking
//!   citations).
//! - "Tools: off/on" turns the conversation's tools on (`POST /tools`).
//!   ppxai starts a slot with them off. Each file edit or risky command
//!   then asks in a dialog: Deny, Never for this, Always allow or Allow.
//! - Escape in the input, or Stop, interrupts an answer (`/interrupt`).
//! - Slash commands are ppxai's (`GET /commands`, ppxai ADR 0007), routed
//!   as its roster says, as the web app's `CommandDispatcher` does: the
//!   `coding.stream` ones (`/explain` and the like) go to `/chat` as typed,
//!   the client-dispatched ones to tai-gui's own actions (`CLIENT_ACTIONS`),
//!   and the rest to `POST /command/<name>`, whose result shows as
//!   Markdown. Typing `/` lists them. Without the roster no slash command
//!   is sent, since one could carry a secret (`/token set`).
//! - Clear, or `/clear`, starts the conversation over.
//!
//! Keys follow the mainstream harnesses (Claude Code, Codex, pi): Enter
//! sends, Shift+Enter starts a new line.
//!
//! A picker above the input names the provider and model each prompt goes
//! to. It starts on ppxai's defaults; providers without an API key are
//! listed but can't be picked.
//!
//! egui's bundled fonts have no CJK. Text they can't draw gets the font the
//! OS picks for it, added to egui as a fallback (ADR 0012, decision 6).

use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use eframe::egui::{
    self,
    epaint::text::{FontInsert, FontPriority, InsertFontFamily},
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use crate::{
    discovery::{self, FontFile},
    engine::{Engine, State},
    fonts,
    ppxai::{self, ChatEvent, Client, Consent, ToolKind, ToolStep},
};

enum Entry {
    Prompt(String),
    Answer(Reply),
    /// A line from tai or the engine, such as "Interrupted".
    Note(String),
    Error(String),
}

/// An answer as the pane shows it.
#[derive(Clone, Default)]
struct Reply {
    reasoning: String,
    tools: Vec<ToolStep>,
    /// What the model said between tool steps, before its answer.
    prose: Vec<String>,
    text: String,
    /// The answer split into its tables and the Markdown between them.
    blocks: Vec<Block>,
    /// The web sources ppxai grounded it on; its `[n]` is the nth.
    sources: Vec<ppxai::Source>,
    /// The engine's status lines and warnings during the answer.
    notes: Vec<String>,
    meta: String,
}

pub struct ChatPane {
    /// The engine slot this pane's calls go through, made once the
    /// engine is ready.
    client: Option<Client>,
    entries: Vec<Entry>,
    input: String,
    /// The `/run` answer in flight, filled by its thread.
    pending: Option<(Instant, Arc<Mutex<Option<Entry>>>)>,
    /// The conversation's answer in flight.
    turn: Option<Turn>,
    /// Whether the conversation's tools are on; None until ppxai says.
    tools: Option<bool>,
    /// Calls in flight whose answers the pane shows.
    jobs: Vec<Arc<Mutex<Option<Done>>>>,
    /// ppxai's slash commands; None until they've loaded.
    roster: Option<ppxai::Roster>,
    /// A slash command running on the server.
    running: bool,
    /// Whether the input had the focus last frame. egui drops the focus on
    /// Escape before any widget sees the key (egui 0.36
    /// `memory/mod.rs`), so Escape can't ask whether it has it now.
    input_focused: bool,
    picker: Picker,
    markdown: CommonMarkCache,
    fonts: ChatFonts,
}

/// An answer streaming in from `/chat`.
struct Turn {
    since: Instant,
    live: Arc<Mutex<Live>>,
    /// Set to stop reading the stream.
    stop: Arc<AtomicBool>,
    /// The answer's text as last split into blocks, and the blocks.
    shown: String,
    blocks: Vec<Block>,
}

/// What the stream has brought so far; its thread fills it.
#[derive(Default)]
struct Live {
    model: Option<String>,
    reply: Reply,
    /// What the engine says it's doing, until the answer starts.
    activity: Option<String>,
    /// Consent requests waiting for the user, oldest first.
    consents: VecDeque<Consent>,
    tokens: Option<u64>,
    /// `stream_end` arrived.
    ended: bool,
    error: Option<String>,
    /// The stream is over, and its thread done.
    finished: bool,
}

impl Live {
    fn apply(&mut self, event: ChatEvent) {
        match event {
            ChatEvent::Start { model } => self.model = model.or(self.model.take()),
            ChatEvent::Chunk(text) => {
                self.activity = None;
                self.reply.text.push_str(&text);
            }
            ChatEvent::Reasoning(text) => self.reply.reasoning.push_str(&text),
            ChatEvent::End { text, tokens } => {
                // The whole answer, as ppxai has it.
                if !text.is_empty() {
                    self.reply.text = text;
                }
                self.tokens = tokens;
                self.ended = true;
            }
            ChatEvent::Tool(step) => self.reply.tools.push(step),
            ChatEvent::Prose(text) => {
                self.activity = None;
                self.reply.prose.push(text);
            }
            ChatEvent::Consent(request) => self.consents.push_back(request),
            ChatEvent::Status(text) => self.reply.notes.push(text),
            ChatEvent::Info(text) => {
                if self.reply.text.is_empty() {
                    self.activity = Some(text);
                }
            }
            ChatEvent::Warning(text) => self.reply.notes.push(format!("Warning: {text}")),
            ChatEvent::Error(why) => self.error = Some(why),
        }
    }
}

/// A finished call the pane shows the outcome of.
enum Done {
    Tools(Result<bool, String>),
    Roster(Result<ppxai::Roster, String>),
    /// A slash command, by its roster name.
    Command(String, Result<ppxai::CommandOutcome, String>),
    /// A consent answer, an interrupt: shown only when it fails.
    Sent(Result<(), String>),
}

/// The client actions ppxai's roster binds to a command (ppxai
/// `commands/factory.py`, `client_action`) that tai-gui implements. The
/// others, such as `task.controller`, answer that tai-gui lacks them.
const CLIENT_ACTIONS: &[&str] = &["run.controller"];

/// Client actions whose command is a chat message: sent to `/chat` as
/// typed, its answer streaming in (the web app's `STREAMING_COMMANDS`).
const STREAM_ACTIONS: &[&str] = &["coding.stream", "coding.convert"];

/// Server commands after which the transcript no longer matches the
/// conversation, so it's emptied. ppxai's envelope doesn't say so yet.
const RESETS_TRANSCRIPT: &[&str] = &["clear"];

/// The error an interrupted `/chat` stream ends with (ppxai
/// `engine/chat.py`).
const INTERRUPTED: &str = "Interrupted by user";

/// The slot's provider and model, as the web app's header pickers: a
/// change switches the slot at once (`POST /providers`, `POST /models`), and
/// `/chat` carries only the message. Naming them in every `/chat` would
/// make ppxai switch on every turn, which it refuses while an interrupted
/// answer is still running. `/run` names them in its one-shot request.
#[derive(Default)]
struct Picker {
    asked: bool,
    providers: Vec<ppxai::Provider>,
    provider: Option<String>,
    models: Vec<ppxai::Model>,
    /// None is the provider's `default_model`.
    model: Option<String>,
    /// A list in flight, filled by its thread. Replacing it drops the
    /// answer of a list asked for earlier.
    loading: Option<Arc<Mutex<Option<Loaded>>>>,
    /// A model switch in flight, and the model to go back to if it fails.
    switching: Option<Switch>,
    error: Option<String>,
    /// Lines for the transcript, such as "Switched to model: …".
    notes: Vec<String>,
}

/// Where a thread's result lands; the UI takes it from there.
type Slot<T> = Arc<Mutex<Option<T>>>;

/// A model switch in flight: ppxai's answer, how many messages it cleared.
struct Switch {
    done: Slot<Result<Option<u64>, String>>,
    /// The model picked before, to go back to if ppxai refuses.
    previous: Option<String>,
}

enum Loaded {
    Providers(Result<ppxai::Providers, String>),
    Models(Result<Vec<ppxai::Model>, String>),
}

impl Picker {
    /// The pickers; while `busy`, they can't be changed, since ppxai
    /// refuses a switch while an answer is in flight.
    fn ui(&mut self, ui: &mut egui::Ui, client: Option<&Client>, busy: bool) {
        let Some(client) = client else {
            return;
        };
        if !self.asked {
            self.asked = true;
            let client = client.clone();
            self.loading = load(ui.ctx(), move || Loaded::Providers(client.providers()));
        }
        self.collect(client, ui.ctx());
        if self.providers.is_empty() {
            if let Some(why) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, why);
            }
            return;
        }
        self.collect_switch();
        let before = self.provider.clone();
        let before_model = self.model.clone();
        let busy = busy || self.switching.is_some();
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                let current = self.current();
                egui::ComboBox::from_id_salt("chat-provider")
                    .selected_text(current.map_or("default provider", |p| p.name.as_str()))
                    .show_ui(ui, |ui| {
                        for p in &self.providers {
                            let label = if p.has_api_key {
                                p.name.clone()
                            } else {
                                format!("{} (no API key)", p.name)
                            };
                            ui.add_enabled_ui(p.has_api_key, |ui| {
                                ui.selectable_value(&mut self.provider, Some(p.id.clone()), label);
                            });
                        }
                    });
                let default = self
                    .current()
                    .and_then(|p| p.default_model.clone())
                    .unwrap_or_else(|| "default model".into());
                egui::ComboBox::from_id_salt("chat-model")
                    .selected_text(self.model.clone().unwrap_or(default.clone()))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.model, None, format!("{default} (default)"));
                        for m in &self.models {
                            ui.selectable_value(&mut self.model, Some(m.id.clone()), &m.name);
                        }
                    });
                if self.loading.is_some() {
                    ui.spinner();
                }
                if let Some(why) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, why);
                }
            })
        });
        if self.provider != before
            && let Some(id) = self.provider.clone()
        {
            self.model = None;
            self.models.clear();
            self.load_models(client, ui.ctx(), id);
        } else if self.model != before_model {
            // "Default" switches to the provider's default model by name.
            let target = self
                .model
                .clone()
                .or_else(|| self.current().and_then(|p| p.default_model.clone()));
            if let Some(target) = target {
                let client = client.clone();
                self.error = None;
                self.switching = spawn(ui.ctx(), "chat-model", move || {
                    client
                        .set_model(&target)
                        .map_err(|why| format!("{target}: {why}"))
                })
                .map(|done| Switch {
                    done,
                    previous: before_model,
                });
            }
        }
    }

    /// Note a finished model switch, or undo the pick when ppxai refused.
    fn collect_switch(&mut self) {
        let done = self
            .switching
            .as_ref()
            .and_then(|switch| switch.done.lock().ok()?.take());
        let Some(done) = done else {
            return;
        };
        let Some(Switch { previous, .. }) = self.switching.take() else {
            return;
        };
        let name = self
            .model
            .clone()
            .or_else(|| self.current().and_then(|p| p.default_model.clone()))
            .unwrap_or_default();
        match done {
            Ok(Some(cleared)) => self.notes.push(format!(
                "Switched to model: {name} ({cleared} messages cleared from context)"
            )),
            Ok(None) => self.notes.push(format!("Switched to model: {name}")),
            Err(why) => {
                tracing::warn!("chat picker: {why}");
                self.error = Some(why);
                self.model = previous;
            }
        }
    }

    fn current(&self) -> Option<&ppxai::Provider> {
        let id = self.provider.as_ref()?;
        self.providers.iter().find(|p| &p.id == id)
    }

    fn load_models(&mut self, client: &Client, ctx: &egui::Context, provider: String) {
        self.error = None;
        let client = client.clone();
        self.loading = load(ctx, move || Loaded::Models(client.models(&provider)));
    }

    fn collect(&mut self, client: &Client, ctx: &egui::Context) {
        let done = self
            .loading
            .as_ref()
            .and_then(|slot| slot.lock().ok()?.take());
        let Some(done) = done else {
            return;
        };
        self.loading = None;
        match done {
            Loaded::Providers(Ok(providers)) => {
                self.providers = providers.list;
                self.provider = providers.current;
                if let Some(id) = self.provider.clone() {
                    self.load_models(client, ctx, id);
                }
            }
            Loaded::Models(Ok(models)) => self.models = models,
            Loaded::Providers(Err(why)) | Loaded::Models(Err(why)) => {
                tracing::warn!("chat picker: {why}");
                self.error = Some(why);
            }
        }
    }

    /// The provider and model to name in a one-shot request, None for
    /// ppxai's defaults.
    fn choice(&self) -> (Option<String>, Option<String>) {
        (self.provider.clone(), self.model.clone())
    }
}

fn load(
    ctx: &egui::Context,
    f: impl FnOnce() -> Loaded + Send + 'static,
) -> Option<Arc<Mutex<Option<Loaded>>>> {
    spawn(ctx, "chat-picker", f)
}

/// Run `f` on a thread of its own and repaint when it's done: the slot
/// its result lands in.
fn spawn<T: Send + 'static>(
    ctx: &egui::Context,
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<Arc<Mutex<Option<T>>>> {
    let slot = Arc::new(Mutex::new(None));
    let filled = Arc::clone(&slot);
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let loaded = f();
            if let Ok(mut slot) = filled.lock() {
                *slot = Some(loaded);
            }
            ctx.request_repaint();
        });
    match spawned {
        Ok(_) => Some(slot),
        Err(e) => {
            tracing::warn!("chat: couldn't start a thread for {name}: {e}");
            None
        }
    }
}

/// System fonts added to egui for codepoints its own fonts lack.
#[derive(Default)]
struct ChatFonts {
    /// Codepoints already looked at, so each is checked once.
    seen: HashSet<char>,
    /// Font files already added.
    added: HashSet<FontFile>,
}

impl ChatFonts {
    /// Make sure egui can draw `text`: for each codepoint its fonts lack,
    /// add the font the OS picks for it, after egui's own fonts, to both
    /// families. egui lays the text out again on the next frame.
    fn cover(&mut self, ctx: &egui::Context, text: &str) {
        for ch in text.chars() {
            if ch.is_ascii() || !self.seen.insert(ch) {
                continue;
            }
            let font = egui::FontId::proportional(14.0);
            if ctx.fonts_mut(|f| f.has_glyph(&font, ch)) {
                continue;
            }
            let Some(file) = discovery::find(ch) else {
                continue;
            };
            if !self.added.insert(file.clone()) {
                continue;
            }
            let path = file.path.display();
            let bytes = match fonts::map(&file.path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!("chat font fallback: {path}: {e}");
                    continue;
                }
            };
            // epaint panics on a font it can't parse (epaint 0.36
            // `src/text/fonts.rs`, "Error parsing … TTF/OTF font file"), so
            // read it with skrifa first, as epaint will, and skip a file that
            // fails or has no outline for `ch`, as the terminal does.
            match fonts::Typeface::new(bytes, file.index) {
                Ok(face) if face.has_outline(ch) => {}
                Ok(_) => {
                    tracing::info!(
                        "chat font fallback: {path} has no outline for U+{:04X}",
                        ch as u32
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!("chat font fallback: {path} #{}: {e}", file.index);
                    continue;
                }
            }
            let mut data = egui::FontData::from_static(bytes);
            data.index = file.index;
            let fallback = |family| InsertFontFamily {
                family,
                priority: FontPriority::Lowest,
            };
            ctx.add_font(FontInsert::new(
                &format!("{path}#{}", file.index),
                data,
                vec![
                    fallback(egui::FontFamily::Proportional),
                    fallback(egui::FontFamily::Monospace),
                ],
            ));
            tracing::info!(
                "chat font fallback: U+{:04X} from {path} #{}",
                ch as u32,
                file.index
            );
        }
    }
}

impl ChatPane {
    pub fn new() -> Self {
        Self {
            client: None,
            entries: Vec::new(),
            input: String::new(),
            pending: None,
            turn: None,
            tools: None,
            jobs: Vec::new(),
            roster: None,
            running: false,
            input_focused: false,
            picker: Picker::default(),
            markdown: CommonMarkCache::default(),
            fonts: ChatFonts::default(),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, engine: &Engine) {
        self.collect_answer(ui.ctx());
        self.collect_turn(ui.ctx());
        self.collect_jobs();
        if self.client.is_none()
            && let Ok(state) = engine.state.lock()
            && let State::Ready(link) = &*state
        {
            match Client::new(link) {
                Ok(client) => {
                    let asked = client.clone();
                    self.job(ui.ctx(), "chat-tools", move || Done::Tools(asked.tools()));
                    let asked = client.clone();
                    self.job(ui.ctx(), "chat-commands", move || {
                        Done::Roster(asked.commands())
                    });
                    self.client = Some(client);
                }
                Err(why) => tracing::error!("chat: {why}"),
            }
        }
        let client = self.client.clone();
        let busy = self.pending.is_some() || self.turn.is_some() || self.running;

        egui::Panel::bottom("chat-input")
            .resizable(false)
            .show(ui, |ui| {
                ui.add_space(4.0);
                self.status_line(ui, engine);
                self.picker.ui(ui, client.as_ref(), busy);
                for note in self.picker.notes.drain(..) {
                    self.entries.push(Entry::Note(note));
                }
                if let Some(client) = &client {
                    self.controls(ui, client, busy);
                }
                let input_id = egui::Id::new("chat-input-text");
                let focused = ui.memory(|m| m.has_focus(input_id));
                // Enter sends; Shift+Enter falls through to the text box as
                // a new line. Taken before the text box sees the key.
                let send = focused
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                // Escape stops the answer, as in the web app, unless a
                // consent is waiting: that needs an answer, not a stop. The
                // input keeps the focus.
                if self.input_focused
                    && self.turn.as_ref().is_some_and(|t| !t.asking())
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
                {
                    self.stop(ui.ctx());
                    ui.memory_mut(|m| m.request_focus(input_id));
                }
                let ready = client.is_some() && !busy;
                let typed = ui.add(
                    egui::TextEdit::multiline(&mut self.input)
                        .id(input_id)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text(if client.is_none() {
                            "Waiting for the engine…"
                        } else if busy {
                            "Answering… (Esc stops)"
                        } else {
                            "Ask… (Enter sends, Shift+Enter for a new line, /run <prompt> for one-shot)"
                        }),
                );
                self.input_focused = typed.has_focus();
                if typed.changed() {
                    self.fonts.cover(ui.ctx(), &self.input);
                }
                self.suggestions(ui, input_id);
                if send
                    && ready
                    && !self.input.trim().is_empty()
                    && let Some(client) = client.clone()
                {
                    let prompt = std::mem::take(&mut self.input).trim().to_string();
                    self.submit(prompt, client, ui.ctx());
                }
                ui.add_space(4.0);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.entries.is_empty() && self.turn.is_none() {
                        ui.weak(
                            "A conversation with ppxai. /run <prompt> asks one-shot, \
                             outside the conversation.",
                        );
                    }
                    for (n, entry) in self.entries.iter().enumerate() {
                        match entry {
                            Entry::Prompt(text) => {
                                ui.add_space(8.0);
                                ui.label(egui::RichText::new(text).strong());
                            }
                            Entry::Answer(reply) => {
                                show_reply(ui, &mut self.markdown, n, reply, false);
                            }
                            Entry::Note(text) => {
                                ui.add_space(4.0);
                                ui.weak(egui::RichText::new(text).italics());
                            }
                            Entry::Error(why) => {
                                ui.add_space(4.0);
                                ui.colored_label(ui.visuals().error_fg_color, why);
                            }
                        }
                    }
                    let mut stop = false;
                    if let Some(turn) = &mut self.turn {
                        let (reply, activity, stopping) = turn.view(ui.ctx(), &mut self.fonts);
                        let n = self.entries.len();
                        show_reply(ui, &mut self.markdown, n, &reply, true);
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak(format!("{:.0} s", turn.since.elapsed().as_secs_f32()));
                            if stopping {
                                ui.weak("Stopping…");
                            } else {
                                if let Some(activity) = activity {
                                    ui.weak(activity);
                                }
                                stop = ui.small_button("Stop").on_hover_text("Esc").clicked();
                            }
                        });
                        ui.ctx().request_repaint_after(Duration::from_millis(500));
                    }
                    if stop {
                        self.stop(ui.ctx());
                    }
                    if let Some((since, _)) = &self.pending {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak(format!("{:.0} s", since.elapsed().as_secs_f32()));
                        });
                        // Keep the timer moving.
                        ui.ctx().request_repaint_after(Duration::from_millis(500));
                    }
                });
        });

        self.consent_dialog(ui.ctx());

        // Answers are model output, and a Markdown link can name any
        // scheme: file:, or a Windows protocol handler. Open only web links;
        // the hover still shows the full target.
        ui.ctx().output_mut(|o| {
            o.commands.retain(|command| match command {
                egui::OutputCommand::OpenUrl(open) if !is_web_link(&open.url) => {
                    tracing::warn!("chat: not opening a non-web link: {}", open.url);
                    false
                }
                _ => true,
            })
        });
    }

    /// The Tools switch and Clear, as in the web app's header.
    fn controls(&mut self, ui: &mut egui::Ui, client: &Client, busy: bool) {
        ui.horizontal(|ui| {
            let (label, on) = match self.tools {
                Some(true) => ("Tools: on", true),
                Some(false) => ("Tools: off", false),
                None => ("Tools: …", false),
            };
            let toggle = ui
                .add_enabled(
                    !busy && self.tools.is_some(),
                    egui::Button::new(label).selected(on),
                )
                .on_hover_text(
                    "Let the model read and edit files and run commands. \
                     Each edit, and each command that isn't safe, asks first.",
                );
            if toggle.clicked() {
                let client = client.clone();
                self.tools = None;
                self.job(ui.ctx(), "chat-tools", move || {
                    Done::Tools(client.set_tools(!on))
                });
            }
            let clear = ui
                .add_enabled(
                    !busy && !self.entries.is_empty(),
                    egui::Button::new("Clear"),
                )
                .on_hover_text("Start the conversation over");
            if clear.clicked() {
                self.submit("/clear".into(), client.clone(), ui.ctx());
            }
        });
    }

    fn status_line(&self, ui: &mut egui::Ui, engine: &Engine) {
        let Ok(state) = engine.state.lock() else {
            return;
        };
        match &*state {
            State::Starting(step) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(format!("Engine: {step}…"));
                });
            }
            State::Ready(link) => {
                ui.weak(format!("ppxai-server {} at {}", link.version, link.base));
            }
            State::Failed(why) => {
                ui.colored_label(ui.visuals().error_fg_color, format!("Engine: {why}"));
            }
        }
    }

    /// Send the input where it goes: a message to the conversation, or a
    /// slash command where ppxai's roster routes it.
    fn submit(&mut self, input: String, client: Client, ctx: &egui::Context) {
        let Some((name, args)) = command(&input) else {
            return self.chat(input, client, ctx);
        };
        let Some(roster) = &self.roster else {
            self.entries.push(Entry::Prompt(redact(None, &input)));
            self.entries.push(Entry::Error(
                "ppxai's command list hasn't loaded, so slash commands aren't sent".into(),
            ));
            return;
        };
        let spec = roster.resolve(name).cloned();
        let echo = redact(spec.as_ref(), &input);
        let action = spec.as_ref().and_then(|s| s.client_action.as_deref());
        match spec.as_ref() {
            Some(_) if action.is_some_and(|a| STREAM_ACTIONS.contains(&a)) => {
                self.chat(input, client, ctx)
            }
            Some(spec) if spec.client_dispatch => {
                self.entries.push(Entry::Prompt(echo));
                let action = action.unwrap_or_default();
                if !CLIENT_ACTIONS.contains(&action) {
                    self.entries.push(Entry::Error(format!(
                        "/{} runs in the client ({action}), and tai-gui doesn't have it yet",
                        spec.name
                    )));
                    return;
                }
                self.client_action(action, spec, args, client, ctx);
            }
            _ => {
                self.entries.push(Entry::Prompt(echo));
                // An unknown name goes as typed, for ppxai's own message.
                let name = spec.map_or_else(|| name.to_string(), |s| s.name);
                let args = args.to_string();
                self.running = true;
                self.job(ctx, "chat-command", move || {
                    let outcome = client.command(&name, &args);
                    Done::Command(name, outcome)
                });
            }
        }
    }

    /// One of `CLIENT_ACTIONS`.
    fn client_action(
        &mut self,
        action: &str,
        spec: &ppxai::CommandSpec,
        args: &str,
        client: Client,
        ctx: &egui::Context,
    ) {
        match action {
            // `/run <prompt>` asks one-shot. Its lifecycle verbs (`ls`,
            // `get` and the rest) need run views tai-gui doesn't have yet.
            "run.controller" => {
                let verb = args.split_whitespace().next().unwrap_or_default();
                if args.is_empty() {
                    self.entries
                        .push(Entry::Error(format!("Usage: {}", spec.usage)));
                } else if spec.subcommands.iter().any(|s| s == verb) {
                    self.entries.push(Entry::Error(format!(
                        "/{} {verb} isn't in tai-gui yet; /{} <prompt> is",
                        spec.name, spec.name
                    )));
                } else {
                    self.oneshot(args.to_string(), client, ctx.clone());
                }
            }
            other => tracing::error!("chat: {other} is in CLIENT_ACTIONS but not handled"),
        }
    }

    /// The roster's commands that match what's typed, under the input,
    /// while it's a bare `/name`; a click fills the input.
    fn suggestions(&mut self, ui: &mut egui::Ui, input_id: egui::Id) {
        let Some(roster) = &self.roster else {
            return;
        };
        let Some(typed) = self.input.strip_prefix('/') else {
            return;
        };
        if typed.contains(char::is_whitespace) {
            return;
        }
        let typed = typed.to_ascii_lowercase();
        let matches: Vec<&ppxai::CommandSpec> = roster
            .commands
            .iter()
            .filter(|c| !c.hidden)
            .filter(|c| {
                c.name.starts_with(&typed) || c.aliases.iter().any(|a| a.starts_with(&typed))
            })
            .take(8)
            .collect();
        if matches.len() == 1 && matches[0].name == typed {
            ui.weak(&matches[0].usage);
            return;
        }
        let mut chosen = None;
        for spec in matches {
            let line = format!("/{} — {}", spec.name, spec.description);
            if ui
                .add(egui::Label::new(egui::RichText::new(line).weak()).sense(egui::Sense::click()))
                .on_hover_text(&spec.usage)
                .clicked()
            {
                chosen = Some(format!("/{} ", spec.name));
            }
        }
        if let Some(text) = chosen {
            self.input = text;
            ui.memory_mut(|m| m.request_focus(input_id));
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), input_id) {
                let end = egui::text::CCursor::new(self.input.chars().count());
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(end)));
                state.store(ui.ctx(), input_id);
            }
        }
    }

    fn chat(&mut self, message: String, client: Client, ctx: &egui::Context) {
        self.entries.push(Entry::Prompt(message.clone()));
        let live = Arc::new(Mutex::new(Live::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (filled, stopped, repaint) = (Arc::clone(&live), Arc::clone(&stop), ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("chat".into())
            .spawn(move || {
                let result = client.chat(&message, &stopped, |event| {
                    if let Ok(mut live) = filled.lock() {
                        live.apply(event);
                    }
                    repaint.request_repaint();
                });
                if let Ok(mut live) = filled.lock() {
                    if let Err(why) = result {
                        live.error.get_or_insert(why);
                    }
                    live.finished = true;
                }
                repaint.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.turn = Some(Turn {
                    since: Instant::now(),
                    live,
                    stop,
                    shown: String::new(),
                    blocks: Vec::new(),
                })
            }
            Err(e) => self
                .entries
                .push(Entry::Error(format!("couldn't start a thread: {e}"))),
        }
    }

    /// Stop the answer in flight: interrupt it on the server, deny the
    /// consents it's waiting on, and stop reading its stream.
    fn stop(&mut self, ctx: &egui::Context) {
        let (Some(turn), Some(client)) = (&self.turn, self.client.clone()) else {
            return;
        };
        if turn.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        let waiting: Vec<Consent> = turn
            .live
            .lock()
            .map(|mut live| live.consents.drain(..).collect())
            .unwrap_or_default();
        self.job(ctx, "chat-interrupt", move || {
            let mut result = client.interrupt();
            for request in &waiting {
                result = result.and(client.consent(request, "n"));
            }
            Done::Sent(result)
        });
    }

    /// The oldest consent request waiting, as a dialog that stays until
    /// it's answered (the web app's: no dismissing it).
    fn consent_dialog(&mut self, ctx: &egui::Context) {
        let Some(turn) = &self.turn else {
            return;
        };
        let Some(request) = turn
            .live
            .lock()
            .ok()
            .and_then(|live| live.consents.front().cloned())
        else {
            return;
        };
        let mut answer = None;
        egui::Modal::new(egui::Id::new("chat-consent")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            let code = |ui: &mut egui::Ui, text: &str| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(text).monospace()).wrap());
                });
            };
            match &request {
                Consent::Shell {
                    command,
                    working_dir,
                    risk,
                } => {
                    ui.heading("Shell command");
                    ui.label("The model wants to run this command:");
                    code(ui, command);
                    ui.weak(format!("In {working_dir}"));
                    if let Some(risk) = risk {
                        ui.weak(format!("ppxai rates it {risk}"));
                    }
                    ui.weak("Unanswered, ppxai denies it after 60 s.");
                }
                Consent::File { path } => {
                    ui.heading("File edit");
                    ui.label("The model wants to edit:");
                    code(ui, path);
                    ui.weak("Unanswered, ppxai denies it after 5 min.");
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for (label, response, hint) in [
                    ("Deny", "n", "Not this time"),
                    (
                        "Never for this",
                        "never",
                        "Deny, and stop asking in this conversation",
                    ),
                    (
                        "Always allow",
                        "always",
                        "Allow, and stop asking in this conversation",
                    ),
                    ("Allow", "y", "Allow this one"),
                ] {
                    if ui.button(label).on_hover_text(hint).clicked() {
                        answer = Some(response);
                    }
                }
            });
        });
        let (Some(response), Some(client)) = (answer, self.client.clone()) else {
            return;
        };
        if let Ok(mut live) = turn.live.lock() {
            live.consents.pop_front();
        }
        self.job(ctx, "chat-consent", move || {
            Done::Sent(client.consent(&request, response))
        });
    }

    fn job(&mut self, ctx: &egui::Context, name: &str, f: impl FnOnce() -> Done + Send + 'static) {
        match spawn(ctx, name, f) {
            Some(slot) => self.jobs.push(slot),
            None => self
                .entries
                .push(Entry::Error(format!("couldn't start a thread for {name}"))),
        }
    }

    fn collect_jobs(&mut self) {
        let mut done = Vec::new();
        self.jobs
            .retain(|slot| match slot.lock().ok().and_then(|mut s| s.take()) {
                Some(outcome) => {
                    done.push(outcome);
                    false
                }
                None => true,
            });
        for outcome in done {
            match outcome {
                Done::Tools(Ok(on)) => {
                    if self.tools.is_none_or(|was| was != on) && !self.entries.is_empty() {
                        let state = if on { "on" } else { "off" };
                        self.entries.push(Entry::Note(format!("Tools {state}")));
                    }
                    self.tools = Some(on);
                }
                Done::Tools(Err(why)) => {
                    tracing::warn!("chat: tools: {why}");
                    self.entries.push(Entry::Error(format!("Tools: {why}")));
                    self.tools = Some(false);
                }
                Done::Roster(Ok(roster)) => self.roster = Some(roster),
                Done::Roster(Err(why)) => {
                    tracing::warn!("chat: commands: {why}");
                    self.entries.push(Entry::Error(format!(
                        "ppxai's command list didn't load, so slash commands aren't sent: {why}"
                    )));
                }
                Done::Command(name, outcome) => {
                    self.running = false;
                    match outcome {
                        Ok(outcome) => {
                            if let Some(on) = outcome.tools_enabled {
                                self.tools = Some(on);
                            }
                            if outcome.ok && RESETS_TRANSCRIPT.contains(&name.as_str()) {
                                self.entries.clear();
                                self.markdown = CommonMarkCache::default();
                            }
                            if outcome.error {
                                self.entries.push(Entry::Error(outcome.markdown));
                            } else if !outcome.markdown.is_empty() {
                                self.entries.push(Entry::Answer(Reply {
                                    blocks: blocks(&outcome.markdown),
                                    text: outcome.markdown,
                                    ..Reply::default()
                                }));
                            }
                        }
                        Err(why) => self.entries.push(Entry::Error(why)),
                    }
                }
                Done::Sent(Ok(())) => {}
                Done::Sent(Err(why)) => {
                    tracing::warn!("chat: {why}");
                    self.entries.push(Entry::Error(why));
                }
            }
        }
    }

    fn oneshot(&mut self, prompt: String, client: Client, ctx: egui::Context) {
        let slot = Arc::new(Mutex::new(None));
        self.pending = Some((Instant::now(), Arc::clone(&slot)));
        let (provider, model) = self.picker.choice();
        let spawned = std::thread::Builder::new()
            .name("oneshot".into())
            .spawn(move || {
                let entry = match client.oneshot(&prompt, provider.as_deref(), model.as_deref()) {
                    Ok(answer) => entry(answer),
                    Err(e) => Entry::Error(e),
                };
                if let Ok(mut slot) = slot.lock() {
                    *slot = Some(entry);
                }
                ctx.request_repaint();
            });
        if let Err(e) = spawned {
            self.pending = None;
            self.entries
                .push(Entry::Error(format!("couldn't start a thread: {e}")));
        }
    }

    fn collect_answer(&mut self, ctx: &egui::Context) {
        let done = self
            .pending
            .as_ref()
            .and_then(|(_, slot)| slot.lock().ok()?.take());
        if let Some(entry) = done {
            self.pending = None;
            match &entry {
                Entry::Answer(reply) => {
                    self.fonts.cover(ctx, &reply.text);
                    for title in reply.sources.iter().filter_map(|s| s.title.as_deref()) {
                        self.fonts.cover(ctx, title);
                    }
                }
                Entry::Prompt(text) | Entry::Note(text) | Entry::Error(text) => {
                    self.fonts.cover(ctx, text)
                }
            }
            self.entries.push(entry);
        }
    }

    /// Once the stream is over, its answer becomes an entry, followed by
    /// how it ended when it didn't end normally.
    fn collect_turn(&mut self, ctx: &egui::Context) {
        let Some(turn) = &self.turn else {
            return;
        };
        let Some(live) = turn
            .live
            .lock()
            .ok()
            .filter(|live| live.finished)
            .map(|mut live| std::mem::take(&mut *live))
        else {
            return;
        };
        let stopped = turn.stop.load(Ordering::Relaxed);
        self.turn = None;
        let (provider, _) = self.picker.choice();
        for (entry, text) in finish(live, stopped, provider.as_deref()) {
            if let Some(text) = text {
                self.fonts.cover(ctx, &text);
            }
            self.entries.push(entry);
        }
    }
}

impl Turn {
    /// Whether a consent request is waiting for the user.
    fn asking(&self) -> bool {
        self.live.lock().is_ok_and(|live| !live.consents.is_empty())
    }

    /// The answer so far, what the engine is doing, and whether it's
    /// being stopped. The text is split into blocks again only when it
    /// has changed.
    fn view(
        &mut self,
        ctx: &egui::Context,
        fonts: &mut ChatFonts,
    ) -> (Reply, Option<String>, bool) {
        let (mut reply, activity) = match self.live.lock() {
            Ok(live) => (live.reply.clone(), live.activity.clone()),
            Err(_) => Default::default(),
        };
        if reply.text != self.shown {
            fonts.cover(ctx, &reply.text[common_prefix(&self.shown, &reply.text)..]);
            self.blocks = blocks(&reply.text);
            self.shown = reply.text.clone();
        }
        fonts.cover(ctx, &reply.reasoning);
        for line in &reply.prose {
            fonts.cover(ctx, line);
        }
        reply.blocks = self.blocks.clone();
        (reply, activity, self.stop.load(Ordering::Relaxed))
    }
}

/// How many bytes `a` and `b` share at the start, on a char boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    a.char_indices()
        .zip(b.chars())
        .find(|((_, x), y)| x != y)
        .map_or(a.len().min(b.len()), |((i, _), _)| i)
}

/// The entries a finished stream leaves: its answer, when it brought
/// anything, then a note or an error when it didn't end normally. Each
/// comes with text the fonts must cover.
fn finish(live: Live, stopped: bool, provider: Option<&str>) -> Vec<(Entry, Option<String>)> {
    let mut out = Vec::new();
    let mut reply = live.reply;
    let interrupted = stopped || live.error.as_deref() == Some(INTERRUPTED);
    let brought = !(reply.text.is_empty()
        && reply.reasoning.is_empty()
        && reply.tools.is_empty()
        && reply.prose.is_empty());
    if live.ended && reply.text.is_empty() && !reply.tools.is_empty() {
        // The web app's words for a turn that only ran tools.
        reply.text = "Task completed. (No additional response from AI)".into();
    }
    if brought || live.ended {
        let mut meta: Vec<String> = provider.map(str::to_string).into_iter().collect();
        meta.extend(live.model);
        if let Some(tokens) = live.tokens {
            meta.push(format!("{tokens} tokens"));
        }
        reply.meta = meta.join(" · ");
        reply.blocks = blocks(&reply.text);
        let text = format!("{}{}{}", reply.reasoning, reply.prose.concat(), reply.text);
        out.push((Entry::Answer(reply), Some(text)));
    }
    if interrupted {
        out.push((Entry::Note("Interrupted".into()), None));
    } else if let Some(why) = live.error {
        out.push((Entry::Error(why.clone()), Some(why)));
    } else if !live.ended && !brought {
        out.push((
            Entry::Error("The stream ended without an answer".into()),
            None,
        ));
    }
    out
}

/// A slash command's name and the rest of the input, trimmed. Input that
/// starts with `/` but isn't a command word, such as a path, is a message.
/// Whether the name is a command is the roster's call.
fn command(input: &str) -> Option<(&str, &str)> {
    let rest = input.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then(|| (name, args.trim()))
}

/// The input as the transcript shows it. The value of a subcommand the
/// roster marks sensitive is masked; with no roster, every argument is.
fn redact(spec: Option<&ppxai::CommandSpec>, input: &str) -> String {
    let Some((name, args)) = command(input) else {
        return input.to_string();
    };
    if args.is_empty() {
        return input.to_string();
    }
    let masked = match spec {
        None => true,
        Some(spec) => {
            let verb = args.split_whitespace().next().unwrap_or_default();
            spec.sensitive.iter().any(|s| s == verb)
        }
    };
    match (masked, spec) {
        (false, _) => input.to_string(),
        (true, Some(_)) => {
            let verb = args.split_whitespace().next().unwrap_or_default();
            format!("/{name} {verb} ••••")
        }
        (true, None) => format!("/{name} ••••"),
    }
}

/// One answer: its reasoning and tool steps collapsed, the Markdown, the
/// sources it cites, the engine's notes, and the line naming the model.
fn show_reply(
    ui: &mut egui::Ui,
    markdown: &mut CommonMarkCache,
    n: usize,
    reply: &Reply,
    live: bool,
) {
    ui.add_space(4.0);
    ui.push_id(n, |ui| {
        if !reply.reasoning.is_empty() {
            // Open while the model thinks, closed once the answer starts.
            let thinking = live && reply.text.is_empty();
            let title = if thinking {
                "Thinking…"
            } else {
                "Thought process"
            };
            egui::CollapsingHeader::new(title)
                .id_salt("reasoning")
                .open(thinking.then_some(true))
                .show(ui, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(&reply.reasoning).weak()).wrap());
                });
        }
        if !reply.tools.is_empty() {
            egui::CollapsingHeader::new(tool_summary(&reply.tools))
                .id_salt("tools")
                .show(ui, |ui| {
                    for (i, step) in reply.tools.iter().enumerate() {
                        let verb = match step.kind {
                            ToolKind::Call => "call",
                            ToolKind::Result => "result",
                            ToolKind::Error => "error",
                        };
                        egui::CollapsingHeader::new(format!("{verb} {}", step.tool))
                            .id_salt(i)
                            .show(ui, |ui| {
                                let text =
                                    egui::RichText::new(truncate(&step.detail, 2000)).monospace();
                                let text = if step.kind == ToolKind::Error {
                                    text.color(ui.visuals().error_fg_color)
                                } else {
                                    text
                                };
                                ui.add(egui::Label::new(text).wrap());
                            });
                    }
                });
        }
        for line in &reply.prose {
            ui.add(egui::Label::new(egui::RichText::new(line).weak().italics()).wrap());
        }
        for (b, block) in reply.blocks.iter().enumerate() {
            ui.push_id(b, |ui| {
                let mut show = |ui: &mut egui::Ui| {
                    CommonMarkViewer::new().show(ui, markdown, &block.text);
                };
                if block.table {
                    // Only as wide as the pane; a wider table scrolls
                    // sideways.
                    egui::ScrollArea::horizontal()
                        .auto_shrink([false, true])
                        .show(ui, show);
                } else {
                    show(ui);
                }
            });
        }
        if !reply.sources.is_empty() {
            // Numbered as the answer's `[n]` markers. The hover shows the
            // URL; only web links open.
            ui.add_space(2.0);
            ui.weak("Sources");
            for (i, source) in reply.sources.iter().enumerate() {
                let name = source.title.as_deref().unwrap_or(&source.url);
                ui.hyperlink_to(format!("[{}] {name}", i + 1), &source.url);
            }
        }
        for note in &reply.notes {
            ui.weak(note);
        }
        if !reply.meta.is_empty() {
            ui.weak(&reply.meta);
        }
    });
}

/// "3 tool steps: read_file, edit_file", with ✗ after a tool that failed.
fn tool_summary(steps: &[ToolStep]) -> String {
    let mut names: Vec<String> = Vec::new();
    for step in steps {
        match step.kind {
            ToolKind::Call if !names.contains(&step.tool) => names.push(step.tool.clone()),
            ToolKind::Error => {
                let failed = format!("{} ✗", step.tool);
                if let Some(name) = names.iter_mut().find(|n| **n == step.tool) {
                    *name = failed;
                } else if !names.contains(&failed) {
                    names.push(failed);
                }
            }
            _ => {}
        }
    }
    let calls = steps.iter().filter(|s| s.kind == ToolKind::Call).count();
    let noun = if calls == 1 {
        "tool call"
    } else {
        "tool calls"
    };
    format!("{calls} {noun}: {}", names.join(", "))
}

/// `text` cut to at most `max` bytes on a char boundary, marked when cut.
fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let end = (0..=max)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    format!("{}… ({} bytes more)", &text[..end], text.len() - end)
}

/// A run of an answer's Markdown: a table, or everything between tables.
#[derive(Clone, Debug, PartialEq)]
struct Block {
    table: bool,
    text: String,
}

/// Split Markdown into its top-level tables and the text between them.
/// egui_commonmark lays a table out as a grid that never wraps a cell, so
/// a wide one is wider than the pane. Drawn in the same scroll area as the
/// prose, it also widened the area the following paragraphs wrap in, and
/// they ran off the pane. Each table gets its own sideways scroll instead.
/// Link reference definitions only reach links in the same block.
fn blocks(markdown: &str) -> Vec<Block> {
    use pulldown_cmark::{Event, Parser, Tag};
    let options = markdown_options();
    let mut blocks = Vec::new();
    let mut push = |table: bool, text: &str| {
        if !text.trim().is_empty() {
            blocks.push(Block {
                table,
                text: text.to_string(),
            });
        }
    };
    let mut prose = 0;
    let mut depth = 0usize;
    for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                // A table nested in a list or a quote stays in its block.
                if depth == 0 && matches!(tag, Tag::Table(_)) {
                    push(false, &markdown[prose..range.start]);
                    push(true, &markdown[range.clone()]);
                    prose = range.end;
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    push(false, &markdown[prose..]);
    blocks
}

/// egui_commonmark's parser options (`parser_options` in
/// egui_commonmark_backend 0.25), so tai reads answers as it draws them.
fn markdown_options() -> pulldown_cmark::Options {
    use pulldown_cmark::Options;
    Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_DEFINITION_LIST
}

/// Turn a grounded answer's bare `[n]` markers into links to the nth
/// source, so a click opens what it cites. Markers in code, inside a link
/// or HTML, escaped, or followed by `(` or `:` (already a link, or a
/// reference definition) stay as written, and so does a number with no
/// web source.
fn link_citations(markdown: &str, sources: &[ppxai::Source]) -> String {
    use pulldown_cmark::{Event, Parser, Tag};
    if sources.is_empty() {
        return markdown.to_string();
    }
    let skip: Vec<std::ops::Range<usize>> = Parser::new_ext(markdown, markdown_options())
        .into_offset_iter()
        .filter(|(event, _)| {
            matches!(
                event,
                Event::Code(_)
                    | Event::Html(_)
                    | Event::InlineHtml(_)
                    | Event::Start(
                        Tag::CodeBlock(_) | Tag::HtmlBlock | Tag::Link { .. } | Tag::Image { .. }
                    )
            )
        })
        .map(|(_, range)| range)
        .collect();
    let bytes = markdown.as_bytes();
    let mut out = String::with_capacity(markdown.len());
    let (mut last, mut i) = (0, 0);
    while i < bytes.len() {
        if bytes[i] == b'['
            && (i == 0 || bytes[i - 1] != b'\\')
            && !skip.iter().any(|range| range.contains(&i))
        {
            let digits = bytes[i + 1..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            let close = i + 1 + digits;
            let url = (1..=3)
                .contains(&digits)
                .then_some(())
                .filter(|()| bytes.get(close) == Some(&b']'))
                .filter(|()| !matches!(bytes.get(close + 1), Some(b'(' | b':')))
                .and_then(|()| markdown[i + 1..close].parse::<usize>().ok())
                .and_then(|n| sources.get(n.checked_sub(1)?))
                .map(|source| source.url.as_str())
                .filter(|url| is_web_link(url) && !url.contains(['<', '>', ' ', '\n']));
            if let Some(url) = url {
                let n = &markdown[i + 1..close];
                out.push_str(&markdown[last..i]);
                out.push_str(&format!("[\\[{n}\\]](<{url}>)"));
                i = close + 1;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&markdown[last..]);
    out
}

/// A `/run` answer, with a line naming the model and the tokens used.
fn entry(answer: ppxai::Answer) -> Entry {
    let mut meta = format!("{} · {}", answer.provider, answer.model);
    if let Some(total) = answer.total_tokens {
        meta.push_str(&format!(" · {total} tokens"));
    }
    if let Some(reason) = answer.finish_reason.as_deref().and_then(finish_reason) {
        meta.push_str(&format!(" · finished: {reason}"));
    }
    let text = link_citations(&answer.content, &answer.sources);
    Entry::Answer(Reply {
        blocks: blocks(&text),
        sources: answer.sources,
        text,
        meta,
        ..Reply::default()
    })
}

/// Why an answer ended, or `None` when it ended normally. ppxai passes the
/// provider's own value through (bac85e6e): OpenAI's `stop`, Anthropic's
/// `end_turn`, and Gemini's enum as `str()` prints it, `FinishReason.STOP`
/// (`engine/providers/gemini.py`).
fn finish_reason(raw: &str) -> Option<String> {
    let reason = raw.rsplit('.').next().unwrap_or(raw).to_ascii_lowercase();
    (!matches!(reason.as_str(), "stop" | "end_turn")).then_some(reason)
}

/// Whether a link is http or https, the only schemes the chat opens.
fn is_web_link(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

#[cfg(test)]
mod tests {
    use super::{
        Block, Entry, Live, Reply, blocks, command, common_prefix, entry, finish, is_web_link,
        link_citations, redact, tool_summary, truncate,
    };
    use crate::ppxai::{Answer, ChatEvent, CommandSpec, Source, ToolKind, ToolStep};

    fn step(kind: ToolKind, tool: &str) -> ToolStep {
        ToolStep {
            kind,
            tool: tool.into(),
            detail: String::new(),
        }
    }

    fn live(events: Vec<ChatEvent>) -> Live {
        let mut live = Live::default();
        for event in events {
            live.apply(event);
        }
        live.finished = true;
        live
    }

    #[test]
    fn a_stream_builds_its_answer() {
        let l = live(vec![
            ChatEvent::Start {
                model: Some("m".into()),
            },
            ChatEvent::Info("thinking".into()),
            ChatEvent::Reasoning("hmm".into()),
            ChatEvent::Chunk("Hel".into()),
            ChatEvent::Info("ignored once text started".into()),
            ChatEvent::Chunk("lo".into()),
            ChatEvent::End {
                text: "Hello!".into(),
                tokens: Some(9),
            },
        ]);
        assert_eq!(l.activity, None);
        let out = finish(l, false, Some("p"));
        assert_eq!(out.len(), 1);
        let Entry::Answer(reply) = &out[0].0 else {
            panic!("not an answer");
        };
        assert_eq!(reply.text, "Hello!");
        assert_eq!(reply.reasoning, "hmm");
        assert_eq!(reply.meta, "p · m · 9 tokens");
    }

    #[test]
    fn a_stream_that_ends_badly_says_how() {
        let interrupted = finish(
            live(vec![
                ChatEvent::Chunk("par".into()),
                ChatEvent::Error("Interrupted by user".into()),
            ]),
            false,
            None,
        );
        assert!(matches!(&interrupted[0].0, Entry::Answer(r) if r.text == "par"));
        assert!(matches!(&interrupted[1].0, Entry::Note(n) if n == "Interrupted"));

        let stopped = finish(live(vec![]), true, None);
        assert!(matches!(&stopped[..], [(Entry::Note(_), _)]));

        let failed = finish(live(vec![ChatEvent::Error("boom".into())]), false, None);
        assert!(matches!(&failed[..], [(Entry::Error(e), _)] if e == "boom"));

        let empty = finish(live(vec![]), false, None);
        assert!(matches!(&empty[..], [(Entry::Error(_), _)]));

        let tools_only = finish(
            live(vec![
                ChatEvent::Tool(step(ToolKind::Call, "ls")),
                ChatEvent::End {
                    text: String::new(),
                    tokens: None,
                },
            ]),
            false,
            None,
        );
        assert!(
            matches!(&tools_only[0].0, Entry::Answer(r) if r.text.starts_with("Task completed"))
        );

        let narrated = finish(
            live(vec![
                ChatEvent::Prose("Looking at the file.".into()),
                ChatEvent::Error("Interrupted by user".into()),
            ]),
            false,
            None,
        );
        assert!(matches!(&narrated[0].0, Entry::Answer(r) if r.prose == ["Looking at the file."]));
    }

    #[test]
    fn tool_steps_are_summed_up() {
        let steps = [
            step(ToolKind::Call, "read"),
            step(ToolKind::Result, "read"),
            step(ToolKind::Call, "edit"),
            step(ToolKind::Error, "edit"),
            step(ToolKind::Call, "read"),
        ];
        assert_eq!(tool_summary(&steps), "3 tool calls: read, edit ✗");
        assert_eq!(tool_summary(&steps[..1]), "1 tool call: read");
    }

    #[test]
    fn slash_commands_are_words_not_paths() {
        assert_eq!(command("/run  what is up "), Some(("run", "what is up")));
        assert_eq!(command("/clear"), Some(("clear", "")));
        assert_eq!(command("/debug-log on"), Some(("debug-log", "on")));
        assert_eq!(command("/usr/bin is a path"), None);
        assert_eq!(command("/"), None);
        assert_eq!(command("hello /run"), None);
    }

    #[test]
    fn sensitive_values_are_masked_in_the_echo() {
        let token = CommandSpec {
            name: "token".into(),
            aliases: vec![],
            description: String::new(),
            usage: String::new(),
            hidden: false,
            client_dispatch: true,
            client_action: Some("token.manage".into()),
            sensitive: vec!["set".into()],
            subcommands: vec!["set".into(), "status".into()],
        };
        assert_eq!(redact(Some(&token), "/token set abc123"), "/token set ••••");
        assert_eq!(redact(Some(&token), "/token status"), "/token status");
        assert_eq!(redact(None, "/token set abc123"), "/token ••••");
        assert_eq!(redact(None, "/clear"), "/clear");
        assert_eq!(redact(None, "plain message"), "plain message");
    }

    #[test]
    fn text_helpers_keep_char_boundaries() {
        assert_eq!(common_prefix("héllo", "hélp"), 4);
        assert_eq!(common_prefix("ab", "abc"), 2);
        assert_eq!(truncate("ééé", 3), "é… (4 bytes more)");
        assert_eq!(truncate("abc", 3), "abc");
    }

    fn block(table: bool, text: &str) -> Block {
        Block {
            table,
            text: text.to_string(),
        }
    }

    #[test]
    fn citation_markers_link_to_their_sources() {
        let sources = [
            Source {
                url: "https://a.example/x".into(),
                title: Some("a.example".into()),
            },
            Source {
                url: "file:///etc/passwd".into(),
                title: None,
            },
        ];
        let link = |md: &str| link_citations(md, &sources);
        // Bare markers, back to back, become links.
        assert_eq!(
            link("Rust 1.100 [1][1]."),
            "Rust 1.100 [\\[1\\]](<https://a.example/x>)[\\[1\\]](<https://a.example/x>)."
        );
        // No such source, not a web link, or not a marker: as written.
        for md in [
            "see [3]",
            "see [2]",
            "see [0]",
            "a [x] b",
            "see `[1]`",
            "```\n[1]\n```\n",
            "[1](https://other.example)",
            "[see [1]](https://other.example)",
            "escaped \\[1]",
            "[1]: https://def.example",
        ] {
            assert_eq!(link(md), md, "{md}");
        }
        // Without sources nothing changes.
        assert_eq!(link_citations("x [1]", &[]), "x [1]");
    }

    #[test]
    fn tables_are_split_from_the_text_around_them() {
        let md = "Intro.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nAfter.\n";
        assert_eq!(
            blocks(md),
            [
                block(false, "Intro.\n\n"),
                block(true, "| a | b |\n|---|---|\n| 1 | 2 |\n"),
                block(false, "\nAfter.\n"),
            ]
        );
        // Without a table, the answer is one block.
        assert_eq!(blocks("# H\n\ntext"), [block(false, "# H\n\ntext")]);
        // A pipe in a code block is not a table.
        let code = "```\n| a | b |\n|---|---|\n```\n";
        assert_eq!(blocks(code), [block(false, code)]);
        // A table in a list item stays with its list.
        let nested = "- item\n\n  | a |\n  |---|\n  | 1 |\n";
        assert_eq!(blocks(nested), [block(false, nested)]);
    }

    #[test]
    fn only_web_links_open() {
        assert!(is_web_link("https://example.com/a"));
        assert!(is_web_link("HTTP://example.com"));
        assert!(!is_web_link("file:///C:/Windows/System32/calc.exe"));
        assert!(!is_web_link("ms-msdt:/id PCWDiagnostic"));
        assert!(!is_web_link("javascript:alert(1)"));
        assert!(!is_web_link("httpx://example.com"));
    }

    fn answer(tokens: Option<u64>, finish: &str) -> Answer {
        Answer {
            content: "**hi**".into(),
            provider: "p1".into(),
            model: "m1".into(),
            total_tokens: tokens,
            finish_reason: Some(finish.into()),
            sources: Vec::new(),
        }
    }

    #[test]
    fn an_answer_names_provider_model_and_tokens() {
        let Entry::Answer(Reply { text, meta, .. }) = entry(answer(Some(3), "stop")) else {
            panic!("not an answer");
        };
        assert_eq!(text, "**hi**");
        assert_eq!(meta, "p1 · m1 · 3 tokens");
    }

    #[test]
    fn a_cut_off_answer_says_why() {
        let Entry::Answer(Reply { meta, .. }) = entry(answer(None, "length")) else {
            panic!("not an answer");
        };
        assert_eq!(meta, "p1 · m1 · finished: length");
    }

    #[test]
    fn a_normal_end_is_not_shown_in_any_provider_spelling() {
        for normal in ["stop", "end_turn", "FinishReason.STOP"] {
            let Entry::Answer(Reply { meta, .. }) = entry(answer(None, normal)) else {
                panic!("not an answer");
            };
            assert_eq!(meta, "p1 · m1", "{normal}");
        }
        let Entry::Answer(Reply { meta, .. }) = entry(answer(None, "FinishReason.MAX_TOKENS"))
        else {
            panic!("not an answer");
        };
        assert_eq!(meta, "p1 · m1 · finished: max_tokens");
    }
}
