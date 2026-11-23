//! The chat pane: a conversation with ppxai through the engine link
//! (`ppxai.rs`), its answers rendered as Markdown. It follows ppxai's
//! desktop web app (`ppxai/web/app.js`), ADR 0010's chat steps:
//!
//! - A prompt goes to the pane's conversation, `/chat`, and its answer
//!   streams in. ppxai keeps the history per engine slot. The model's
//!   reasoning shows in a collapsed "Thought process", and tool calls in a
//!   collapsed list of steps.
//! - An answer's `[n]` markers link to the sources ppxai lists for the
//!   turn (`stream_end`'s `sources`, ppxai ADR 0019), shown under it.
//! - `/run <prompt>` starts a one-shot background run (`/v1/agent/run`),
//!   as the web app does, and opens it in the run pane (`runs.rs`); `/run
//!   ls`, `get`, `watch`, `cancel` and `collect` work on runs by id.
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
    path::Path,
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
    grammar::{self, Dispatch, TaskArgs},
    ppxai::{self, ChatEvent, Client, Consent, SideEffect, ToolKind, ToolStep},
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
pub struct Reply {
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

impl Reply {
    /// Markdown with the sources its `[n]` markers cite, and a line under
    /// it: a run's result, as the run pane shows it.
    pub fn markdown(text: &str, sources: Vec<ppxai::Source>, meta: String) -> Self {
        let text = link_citations(text, &sources);
        Self {
            blocks: blocks(&text),
            text,
            sources,
            meta,
            ..Self::default()
        }
    }
}

pub struct ChatPane {
    /// The engine slot this pane's calls go through, made once the
    /// engine is ready.
    client: Option<Client>,
    entries: Vec<Entry>,
    input: String,
    /// The conversation's answer in flight.
    turn: Option<Turn>,
    /// Whether the conversation's tools are on; None until ppxai says.
    tools: Option<bool>,
    /// The conversation's working directory, where its tools and `/task`
    /// runs resolve files; None until ppxai says.
    folder: Option<String>,
    /// The folder being typed, while it's edited.
    folder_edit: Option<String>,
    /// The folder's field has opened and hasn't had the focus yet.
    folder_focus: bool,
    /// A command ran that may have moved the folder (`/cd`): look again.
    folder_stale: bool,
    /// Calls in flight whose answers the pane shows.
    jobs: Vec<Arc<Mutex<Option<Done>>>>,
    /// ppxai's slash commands; None until they've loaded.
    roster: Option<ppxai::Roster>,
    /// A slash command running on the server.
    running: bool,
    /// Runs to open in the run pane; the app hands them over.
    opened: Vec<crate::runs::Open>,
    /// Terminals commands asked for; the app hands them over.
    terminals: Vec<TerminalRequest>,
    /// The folder of the terminal that last had the focus, when its shell
    /// has reported one; the app sets it every frame.
    terminal_folder: Option<String>,
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
    /// The images the `display_file` tool showed, as Markdown kept after
    /// the answer: `stream_end` brings the answer's text without them, as
    /// the web app's `_streamInlineImages` works around.
    displayed: String,
    /// State changes the pane hasn't taken yet.
    state: ppxai::StateSync,
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
            ChatEvent::End {
                text,
                tokens,
                sources,
            } => {
                // The whole answer, as ppxai has it.
                if !text.is_empty() {
                    self.reply.text = text;
                    self.reply.text.push_str(&self.displayed);
                }
                self.tokens = tokens;
                self.reply.sources = sources;
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
            // An image goes into the answer, as the web app shows it; the
            // other kinds the web app opens in its right-hand panel, which
            // tai-gui doesn't have for them yet.
            ChatEvent::State(state) => self.state.merge(state),
            ChatEvent::Throttled(why) => self.error = Some(why),
            ChatEvent::Degraded(reasons) => self.reply.notes.push(degraded(&reasons)),
            ChatEvent::Stopped(why) => self.reply.notes.push(format!("Stopped early: {why}")),
            ChatEvent::Display(path) => match crate::images::shown_markdown(&path) {
                Some(markdown) => {
                    self.activity = None;
                    self.displayed.push_str(&markdown);
                    self.reply.text.push_str(&markdown);
                }
                None => self.reply.notes.push(format!(
                    "ppxai showed {path}; tai-gui shows only images so far."
                )),
            },
        }
    }
}

/// How a turn whose tools were cut short says so, from ppxai's
/// `ToolGuardReason` values (`engine/types.py`).
fn degraded(reasons: &[String]) -> String {
    let said: Vec<&str> = reasons
        .iter()
        .map(|r| match r.as_str() {
            "iteration_cap" => {
                "it used every tool step it had, so it answered from what it had found"
            }
            "tool_budget_exhausted" => "a tool reached its call limit for the turn",
            "tools_withdrawn" => "its tools were taken away after it asked for a refused one again",
            "tool_repeat_loop" => "it repeated the same tool call",
            other => other,
        })
        .collect();
    format!("The tools were cut short: {}.", said.join("; "))
}

/// A terminal that a command asked for: `/terminal`, or a command to type
/// into one (ppxai's `open_terminal` and `run_shell` side effects).
pub struct TerminalRequest {
    /// Where a new terminal starts; one that's running stays where it is.
    pub cwd: Option<String>,
    /// Typed at the prompt, not run: the user presses Enter.
    pub command: Option<String>,
    /// The terminal's alias (ADR 0013), as ppxai sent it; None for the
    /// terminal that last had the focus.
    pub name: Option<String>,
}

/// A finished call the pane shows the outcome of.
enum Done {
    /// A run started from `/run` or `/task`: its task and its id.
    RunStarted(String, Result<String, String>),
    /// `/run ls`, `/task ls`, as Markdown.
    Listed(Result<String, String>),
    /// A lifecycle verb on a run, such as `cancel` or `respond`: the run's
    /// id and what to say about it.
    RunAction(String, Result<String, String>),
    Tools(Result<bool, String>),
    /// The conversation's working directory, and whether to say it changed.
    Folder(Result<String, String>, bool),
    Roster(Result<ppxai::Roster, String>),
    /// A slash command, by its roster name.
    Command(String, Result<ppxai::CommandOutcome, String>),
    /// A consent answer, an interrupt: shown only when it fails.
    Sent(Result<(), String>),
}

/// The client actions ppxai's roster binds to a command (ppxai
/// `commands/factory.py`, `client_action`) that tai-gui implements. The
/// others, such as `auto.loop`, answer that tai-gui lacks them.
const CLIENT_ACTIONS: &[&str] = &["run.controller", "task.controller"];

/// The two run families: `/run` starts tool-free one-shot runs, `/task`
/// runs with the tools it's granted. They share the lifecycle verbs, as the
/// web app's `RunController` extends its `TaskController`.
#[derive(Clone, Copy, PartialEq)]
enum Family {
    Run,
    Task,
}

impl Family {
    /// The runs' `kind`, which `ls` lists.
    fn kind(self) -> &'static str {
        match self {
            Family::Run => "oneshot",
            Family::Task => "task",
        }
    }
}

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
pub type Slot<T> = Arc<Mutex<Option<T>>>;

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

    /// Show the provider and model ppxai says it's on, when they changed
    /// outside the pickers (`/model`, `/provider`). It switches nothing.
    fn sync(
        &mut self,
        client: &Client,
        ctx: &egui::Context,
        provider: Option<String>,
        model: Option<String>,
    ) {
        if self.switching.is_some() {
            return;
        }
        if let Some(id) = provider
            && self.provider.as_ref() != Some(&id)
        {
            self.provider = Some(id.clone());
            self.model = None;
            self.models.clear();
            self.load_models(client, ctx, id);
        }
        if let Some(id) = model {
            let default = self.current().and_then(|p| p.default_model.as_ref());
            self.model = (default != Some(&id)).then_some(id);
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
pub fn spawn<T: Send + 'static>(
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
    /// The runs to open in the run pane since the last call.
    pub fn take_opened(&mut self) -> Vec<crate::runs::Open> {
        std::mem::take(&mut self.opened)
    }

    /// A line in the transcript, such as a run pane's "collected".
    pub fn set_terminal_folder(&mut self, folder: Option<std::path::PathBuf>) {
        self.terminal_folder = folder.map(|f| f.to_string_lossy().into_owned());
    }

    /// The conversation's working directory, once ppxai has said.
    pub fn folder(&self) -> Option<&str> {
        self.folder.as_deref()
    }

    pub fn take_terminals(&mut self) -> Vec<TerminalRequest> {
        std::mem::take(&mut self.terminals)
    }

    pub fn note(&mut self, text: String) {
        self.entries.push(Entry::Note(text));
    }

    pub fn new() -> Self {
        Self {
            client: None,
            entries: Vec::new(),
            input: String::new(),
            turn: None,
            tools: None,
            folder: None,
            terminals: Vec::new(),
            terminal_folder: None,
            folder_edit: None,
            folder_focus: false,
            folder_stale: false,
            jobs: Vec::new(),
            roster: None,
            running: false,
            input_focused: false,
            opened: Vec::new(),
            picker: Picker::default(),
            markdown: CommonMarkCache::default(),
            fonts: ChatFonts::default(),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, engine: &Engine) {
        self.collect_turn(ui.ctx());
        self.collect_jobs(ui.ctx());
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
                    // The conversation works where tai-gui was started, as
                    // a terminal would; ppxai's own default is the home
                    // folder. Started from a launcher (cwd `/`), keep it.
                    let asked = client.clone();
                    let start = launch_dir();
                    self.job(ui.ctx(), "chat-folder", move || match start {
                        Some(dir) => Done::Folder(asked.set_working_dir(&dir), false),
                        None => {
                            Done::Folder(asked.working_dir().map(Option::unwrap_or_default), false)
                        }
                    });
                    self.client = Some(client);
                }
                Err(why) => tracing::error!("chat: {why}"),
            }
        }
        let client = self.client.clone();
        if self.folder_stale
            && let Some(asked) = client.clone()
        {
            self.folder_stale = false;
            self.job(ui.ctx(), "chat-folder", move || {
                Done::Folder(asked.working_dir().map(Option::unwrap_or_default), false)
            });
        }
        let busy = self.turn.is_some() || self.running;

        egui::Panel::bottom("chat-input")
            .resizable(false)
            .show(ui, |ui| {
                ui.add_space(4.0);
                self.status_line(ui, engine);
                self.picker.ui(ui, client.as_ref(), busy);
                for note in self.picker.notes.drain(..) {
                    self.entries.push(Entry::Note(note));
                }
                web_images_bar(ui);
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
                            "Ask… (Enter sends, Shift+Enter for a new line, /run <prompt> runs one-shot)"
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
                            "A conversation with ppxai. /run <prompt> asks one-shot in \
                             the background, and /task <prompt> --tools <a,b> works with \
                             tools; the Preview pane shows their runs.",
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
                });
        });

        self.consent_dialog(ui.ctx());
        // Which links open is decided for every pane in main.rs.
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
            // The working directory, as the web app's folder badge: its
            // last part, the whole path on hover; a click edits it.
            if let Some(folder) = self.folder.clone()
                && self.folder_edit.is_none()
            {
                let name = Path::new(&folder)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| folder.clone());
                let badge = ui
                    .add_enabled(!busy, egui::Button::new(format!("In {name}")))
                    .on_hover_text(format!(
                        "{folder}\nWhere tools and /task runs work. Click to change it."
                    ));
                if badge.clicked() {
                    self.folder_edit = Some(folder.clone());
                    self.folder_focus = true;
                }
                // Option 1 of the folder sync: the terminal's folder, on
                // request only, so a cd in a shell never moves the tools.
                let there = self.terminal_folder.clone();
                let differs = there.as_ref().is_some_and(|t| *t != folder);
                let tip = match &there {
                    Some(t) if differs => format!("Work in the terminal's folder: {t}"),
                    Some(_) => "The terminal is in this folder already.".into(),
                    None => "The terminal hasn't said which folder it's in: its shell \
                             isn't zsh or bash, or TAI_SHELL_INTEGRATION=0."
                        .into(),
                };
                let take = ui
                    .add_enabled(!busy && differs, egui::Button::new("Use terminal's"))
                    .on_hover_text(&tip)
                    .on_disabled_hover_text(&tip);
                if take.clicked()
                    && let Some(path) = there
                {
                    let asked = client.clone();
                    self.job(ui.ctx(), "chat-folder", move || {
                        Done::Folder(asked.set_working_dir(&path), true)
                    });
                }
            }
        });
        let Some(mut text) = self.folder_edit.take() else {
            return;
        };
        let mut done = false;
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .hint_text("/path/to/project")
                    .desired_width(ui.available_width() - 120.0),
            );
            // Ask until the field has it: the terminal pane takes the focus
            // whenever nothing holds it, and may win the frame the field
            // opens in.
            if self.folder_focus {
                if field.has_focus() {
                    self.folder_focus = false;
                } else {
                    field.request_focus();
                }
            }
            // Take Enter and Escape out of the frame's input, so the pane
            // that gets the keys next doesn't see them too.
            let entered = field.lost_focus()
                && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let escaped = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if (ui.button("Set").clicked() || entered) && !text.trim().is_empty() {
                let (asked, path) = (client.clone(), text.trim().to_string());
                self.job(ui.ctx(), "chat-folder", move || {
                    Done::Folder(asked.set_working_dir(&path), true)
                });
                done = true;
            }
            if ui.button("Cancel").clicked() || escaped {
                done = true;
            }
        });
        if done {
            // Hand the keys back to the chat input. Left with no focus, the
            // terminal pane would take it this frame and replay this
            // frame's keys, the Enter among them, into its shell.
            // The field has taken this frame's text; the input mustn't too.
            ui.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Text(_))));
            ui.memory_mut(|m| m.request_focus(egui::Id::new("chat-input-text")));
        } else {
            self.folder_edit = Some(text);
        }
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
        // `/run <prompt>` and `/task <prompt> --tools …` start a run in the
        // background, a lifecycle verb works on a run by id, as the web
        // app's RunController and TaskController do (ppxai
        // `web/shared/task-controller.js`, `run-controller.js`).
        let family = match action {
            "run.controller" => Family::Run,
            "task.controller" => Family::Task,
            other => {
                tracing::error!("chat: {other} is in CLIENT_ACTIONS but not handled");
                return;
            }
        };
        let cmd = format!("/{}", spec.name);
        match grammar::classify(args) {
            Dispatch::Help => self.entries.push(Entry::Note(format!(
                "{}. Usage: {}",
                spec.description, spec.usage
            ))),
            Dispatch::NearMiss { id } => self.entries.push(Entry::Error(format!(
                "{id} looks like a run id but isn't one (run_ and 12 hex digits). \
                 {cmd} ls lists the runs."
            ))),
            Dispatch::Lifecycle { verb, id, rest } => {
                self.lifecycle(family, &cmd, &spec.usage, verb, id, &rest, client, ctx)
            }
            Dispatch::Launch(line) => match family {
                Family::Run => self.launch_run(&cmd, &line, client, ctx),
                Family::Task => self.launch_task(&cmd, &spec.usage, &line, client, ctx),
            },
        }
    }

    /// `ls` with its flags: the active conversation's runs, or with `--all`
    /// every conversation's (ppxai fafebaf3).
    fn list_runs(
        &mut self,
        family: Family,
        cmd: &str,
        rest: &str,
        client: Client,
        ctx: &egui::Context,
    ) {
        let flags = match grammar::ListFlags::parse(rest) {
            Ok(flags) => flags,
            Err(why) => {
                self.entries.push(Entry::Error(format!("{cmd} ls: {why}")));
                return;
            }
        };
        let cmd = cmd.to_string();
        self.job(ctx, "chat-runs", move || {
            let listed = (|| {
                let conversation = if flags.all {
                    None
                } else {
                    Some(client.conversation()?)
                };
                let runs = client.runs(&ppxai::RunFilter {
                    kind: Some(family.kind()),
                    session: conversation.as_deref(),
                    status: flags.status.as_deref(),
                })?;
                Ok(runs_table(runs, &cmd, family, &flags))
            })();
            Done::Listed(listed)
        });
    }

    /// A lifecycle verb of `/run` or `/task`.
    #[allow(clippy::too_many_arguments)]
    fn lifecycle(
        &mut self,
        family: Family,
        cmd: &str,
        usage: &str,
        verb: &str,
        id: Option<String>,
        rest: &str,
        client: Client,
        ctx: &egui::Context,
    ) {
        let error = |text: String| Entry::Error(text);
        let Some(id) = id else {
            match verb {
                "help" => self.entries.push(Entry::Note(format!("Usage: {usage}"))),
                "ls" => self.list_runs(family, cmd, rest, client, ctx),
                verb => self.entries.push(error(format!(
                    "{cmd} {verb} needs a run id. Usage: {usage}"
                ))),
            }
            return;
        };
        if family == Family::Run && matches!(verb, "respond" | "resume") {
            self.entries.push(error(format!(
                "A {cmd} run never waits for an answer and can't be resumed; a /task run can"
            )));
            return;
        }
        match verb {
            "help" => self.entries.push(Entry::Note(format!("Usage: {usage}"))),
            // As in ppxai, `ls <run id>` is the plain listing.
            "ls" => self.list_runs(family, cmd, "", client, ctx),
            "get" | "watch" => self.opened.push(crate::runs::Open { id, client }),
            "cancel" => self.job(ctx, "chat-run-cancel", move || {
                let said = client
                    .cancel_run(&id)
                    .map(|()| format!("Asked run {id} to stop"));
                Done::RunAction(id, said)
            }),
            "collect" => self.job(ctx, "chat-run-collect", move || {
                let said = crate::runs::collect_run(&client, &id)
                    .map(|()| format!("Run {id}'s result is now in the conversation"));
                Done::RunAction(id, said)
            }),
            "resume" => self.job(ctx, "chat-run-resume", move || {
                let said = client
                    .resume_run(&id)
                    .map(|()| format!("Run {id} goes on from where it stopped"));
                Done::RunAction(id, said)
            }),
            "respond" => {
                // The words after the id.
                let answer = rest
                    .trim_start()
                    .strip_prefix(id.as_str())
                    .unwrap_or_default()
                    .trim();
                if answer.is_empty() {
                    self.entries.push(error(format!(
                        "Usage: {cmd} respond <id> approve|deny|\"<text>\""
                    )));
                    return;
                }
                let answer = unquote(answer).to_string();
                self.job(ctx, "chat-run-respond", move || {
                    let said = answer_run(&client, &id, &answer);
                    Done::RunAction(id, said)
                });
            }
            other => tracing::error!("chat: {cmd} {other} isn't handled"),
        }
    }

    /// `/run <prompt>`: a one-shot run with the picker's provider and model.
    /// It takes no flags; the server's config decides its grant.
    fn launch_run(&mut self, cmd: &str, line: &str, client: Client, ctx: &egui::Context) {
        let flag = line.split_whitespace().any(|w| {
            w.strip_prefix("--")
                .is_some_and(|f| f.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
        });
        if flag {
            self.entries.push(Entry::Error(format!(
                "{cmd} takes no flags: ppxai's config decides what it may use \
                 (execution.run.web_search). /task takes a tool grant."
            )));
            return;
        }
        let task = unquote(line).to_string();
        let (provider, model) = self.picker.choice();
        let started = task.clone();
        self.job(ctx, "chat-run", move || {
            let id = client.start_run(&task, provider.as_deref(), model.as_deref());
            Done::RunStarted(started, id)
        });
    }

    /// `/task <prompt> --tools a,b …`: a run with tools, in the
    /// conversation's working directory unless `--work-dir` names one.
    fn launch_task(
        &mut self,
        cmd: &str,
        usage: &str,
        line: &str,
        client: Client,
        ctx: &egui::Context,
    ) {
        let args = TaskArgs::parse(line);
        if !args.errors.is_empty() {
            self.entries
                .push(Entry::Error(format!("{cmd}: {}", args.errors.join("; "))));
            return;
        }
        if args.task.is_empty() {
            self.entries.push(Entry::Error(format!("Usage: {usage}")));
            return;
        }
        // ppxai refuses a run without a grant (422); say what to add instead.
        if args.tools.is_empty() && !args.has_source() {
            self.entries.push(Entry::Error(format!(
                "{cmd} needs a tool grant: add --tools web_search (or --tools a,b,c), \
                 or name a --spec, --skill or --profile. /run answers without tools."
            )));
            return;
        }
        let (provider, model) = self.picker.choice();
        let started = args.task.clone();
        self.job(ctx, "chat-task", move || {
            let workdir = match args.workdir {
                Some(_) => None,
                None => client.working_dir().unwrap_or_else(|why| {
                    tracing::warn!("chat: the working dir: {why}");
                    None
                }),
            };
            let body = args.body(provider.as_deref(), model.as_deref(), workdir.as_deref());
            Done::RunStarted(started, client.start_task(&body))
        });
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

    /// Follow what ppxai says changed: the folder, the tools, the provider
    /// and the model. A tool can move the folder, and a command such as
    /// `/model` can switch the model. `say` adds a line for a folder
    /// change; a command's result says it already.
    fn apply_state(&mut self, state: ppxai::StateSync, ctx: &egui::Context, say: bool) {
        if let Some(path) = state.working_dir
            && self.folder.as_ref() != Some(&path)
        {
            if say && self.folder.is_some() {
                self.entries
                    .push(Entry::Note(format!("Working directory changed to {path}")));
            }
            crate::images::set_workdir(Some(&path));
            self.folder = Some(path);
        }
        if let Some(on) = state.tools_enabled {
            if self.tools.is_some_and(|was| was != on) {
                let state = if on { "on" } else { "off" };
                self.entries.push(Entry::Note(format!("Tools {state}")));
            }
            self.tools = Some(on);
        }
        if let Some(client) = &self.client {
            self.picker.sync(client, ctx, state.provider, state.model);
        }
    }

    fn collect_jobs(&mut self, ctx: &egui::Context) {
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
                Done::RunStarted(task, Ok(id)) => {
                    let mut short: String = task.chars().take(60).collect();
                    if short.len() < task.len() {
                        short.push('…');
                    }
                    self.entries.push(Entry::Note(format!(
                        "Run {id} started, shown in the Preview pane: {short}"
                    )));
                    if let Some(client) = self.client.clone() {
                        self.opened.push(crate::runs::Open { id, client });
                    }
                }
                Done::RunStarted(_, Err(why)) => self
                    .entries
                    .push(Entry::Error(format!("The run didn't start: {why}"))),
                Done::Listed(Ok(table)) => self.entries.push(Entry::Answer(Reply::markdown(
                    &table,
                    Vec::new(),
                    String::new(),
                ))),
                Done::Listed(Err(why)) | Done::RunAction(_, Err(why)) => {
                    self.entries.push(Entry::Error(why))
                }
                Done::RunAction(id, Ok(text)) => {
                    self.entries.push(Entry::Note(text));
                    // Show the run as it is now: opening it in the run pane
                    // looks at it again.
                    if let Some(client) = self.client.clone() {
                        self.opened.push(crate::runs::Open { id, client });
                    }
                }
                Done::Roster(Ok(roster)) => self.roster = Some(roster),
                Done::Roster(Err(why)) => {
                    tracing::warn!("chat: commands: {why}");
                    self.entries.push(Entry::Error(format!(
                        "ppxai's command list didn't load, so slash commands aren't sent: {why}"
                    )));
                }
                Done::Folder(Ok(path), say) => {
                    if say {
                        self.entries
                            .push(Entry::Note(format!("Working directory set to {path}")));
                    }
                    crate::images::set_workdir(Some(&path));
                    self.folder = Some(path);
                }
                Done::Folder(Err(why), _) => {
                    tracing::warn!("chat: working dir: {why}");
                    self.entries
                        .push(Entry::Error(format!("Working directory: {why}")));
                }
                Done::Command(name, outcome) => {
                    self.running = false;
                    self.folder_stale = true;
                    match outcome {
                        Ok(outcome) => {
                            if outcome.ok && RESETS_TRANSCRIPT.contains(&name.as_str()) {
                                self.entries.clear();
                                self.markdown = CommonMarkCache::default();
                                crate::images::new_conversation();
                            }
                            self.apply_state(outcome.state, ctx, false);
                            let mut notes = Vec::new();
                            for effect in outcome.effects {
                                match effect {
                                    // /show <image>: the web app opens it in
                                    // its right-hand panel; here, in the
                                    // Preview stack.
                                    SideEffect::ShowImage(path) => {
                                        crate::images::request_open(&path)
                                    }
                                    SideEffect::Terminal { cwd, command, name } => {
                                        self.terminals.push(TerminalRequest { cwd, command, name })
                                    }
                                    SideEffect::RefreshRoster => {
                                        if let Some(asked) = self.client.clone() {
                                            self.job(ctx, "chat-commands", move || {
                                                Done::Roster(asked.commands())
                                            });
                                        }
                                    }
                                    SideEffect::Copy(text) => ctx.copy_text(text),
                                    SideEffect::Notify { message, error } => notes.push(if error {
                                        Entry::Error(message)
                                    } else {
                                        Entry::Note(message)
                                    }),
                                }
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
                            self.entries.extend(notes);
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

    /// Once the stream is over, its answer becomes an entry, followed by
    /// how it ended when it didn't end normally.
    fn collect_turn(&mut self, ctx: &egui::Context) {
        let Some(turn) = &self.turn else {
            return;
        };
        // State changes show while the answer is still coming.
        let state = turn
            .live
            .lock()
            .map(|mut live| std::mem::take(&mut live.state))
            .unwrap_or_default();
        if !state.is_empty() {
            self.apply_state(state, ctx, true);
        }
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
        reply.text = link_citations(&reply.text, &reply.sources);
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
pub fn show_reply(
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
                // Images resolved and gated first (images.rs).
                let text = crate::images::prepare(&block.text);
                let mut show = |ui: &mut egui::Ui| {
                    CommonMarkViewer::new()
                        .explicit_image_uri_scheme(true)
                        .show(ui, markdown, &text);
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
            // Numbered as the answer's `[n]` markers and named as the web
            // app's list names them (`web/shared/sources-render.js`): the
            // title, else the host. The hover shows the URL; only web links
            // are links.
            ui.add_space(2.0);
            ui.weak("Sources");
            for source in &reply.sources {
                let label = format!("[{}] {}", source.n, source_name(source));
                if is_web_link(&source.url) {
                    ui.hyperlink_to(label, &source.url);
                } else {
                    ui.label(label).on_hover_text(&source.url);
                }
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
pub(crate) fn markdown_options() -> pulldown_cmark::Options {
    use pulldown_cmark::Options;
    Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_DEFINITION_LIST
}

/// Turn an answer's bare `[n]` markers into links to the source numbered
/// `n`, so a click opens what it cites. Markers in code, inside a link
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
                .and_then(|n| sources.iter().find(|s| s.n == n))
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

/// `/run`'s argument without one layer of outer quotes, as the web app
/// strips them: quoting is allowed, never required.
/// The folder tai-gui was started in, for the conversation to work in. None
/// when that's the filesystem root, as for an app started from a launcher,
/// which leaves ppxai's default.
fn launch_dir() -> Option<String> {
    let dir = std::env::current_dir().ok()?;
    dir.parent()?;
    Some(dir.to_string_lossy().into_owned())
}

fn unquote(text: &str) -> &str {
    let text = text.trim();
    for q in ['"', '\''] {
        if text.len() > 1 && text.starts_with(q) && text.ends_with(q) {
            return text[1..text.len() - 1].trim();
        }
    }
    text
}

/// `/task respond <id> <answer>`: answer run `id`'s park, as the web app's
/// `respondCmd` does. `approve`, `yes`, `y` approve; `deny`, `no`, `n`
/// deny; any other text goes as text, which a consent park takes as a
/// denial with a note.
fn answer_run(client: &Client, id: &str, answer: &str) -> Result<String, String> {
    let run = client.run(id)?;
    let Some(waiting) = run.waiting else {
        return Err(format!(
            "Run {id} isn't waiting for an answer (it's {})",
            run.status
        ));
    };
    let approved = match answer.to_lowercase().as_str() {
        "approve" | "yes" | "y" => Some(true),
        "deny" | "no" | "n" => Some(false),
        _ => None,
    };
    let text = approved.is_none().then_some(answer);
    client.respond_run(id, &waiting.token, approved, text)?;
    Ok(match approved {
        Some(true) => format!("Approved; run {id} goes on"),
        Some(false) => format!("Denied; run {id} goes on without it"),
        None => format!("Answered run {id}; it goes on"),
    })
}

/// How many top-level runs `ls` shows, as ppxai's `_LIST_CAP`.
const LIST_CAP: usize = 20;

/// Order runs for `ls`: each top-level run, newest first as ppxai lists
/// them, followed by its sub-agents at depth 1. A sub-agent whose parent
/// isn't in the list stays at top level (ppxai `engine/run_scope.py`,
/// fafebaf3).
fn nest(runs: Vec<ppxai::Run>) -> Vec<(ppxai::Run, usize)> {
    let ids: HashSet<String> = runs.iter().map(|r| r.run_id.clone()).collect();
    let parent_of = |run: &ppxai::Run| {
        run.parent_run_id
            .clone()
            .filter(|p| ids.contains(p) && *p != run.run_id)
    };
    let (children, roots): (Vec<_>, Vec<_>) =
        runs.into_iter().partition(|r| parent_of(r).is_some());
    let mut children: Vec<_> = children
        .into_iter()
        .map(|c| (parent_of(&c).unwrap_or_default(), c))
        .collect();
    let mut rows = Vec::new();
    for root in roots {
        let id = root.run_id.clone();
        rows.push((root, 0));
        let mut i = 0;
        while i < children.len() {
            if children[i].0 == id {
                rows.push((children.remove(i).1, 1));
            } else {
                i += 1;
            }
        }
    }
    rows
}

/// `/run ls`, `/task ls` as a Markdown table, after ppxai's
/// `commands/task.py` (fafebaf3): sub-agents marked under their parent, the
/// first `LIST_CAP` top-level runs, and with `--all` each run's conversation.
fn runs_table(
    runs: Vec<ppxai::Run>,
    cmd: &str,
    family: Family,
    flags: &grammar::ListFlags,
) -> String {
    let kind = family.kind();
    let mut scope = String::from(if flags.all {
        "all conversations"
    } else {
        "this conversation"
    });
    if let Some(status) = &flags.status {
        scope.push_str(&format!(", status {status}"));
    }
    let rows = nest(runs);
    if rows.is_empty() {
        let hint = if flags.all {
            String::new()
        } else {
            format!(". `{cmd} ls --all` shows every conversation")
        };
        return format!("No {kind} runs ({scope}){hint}.");
    }
    let cell = |text: &str| {
        let short: String = text.chars().take(60).collect();
        short.replace('|', "\\|").replace('\n', " ")
    };
    let tops = rows.iter().filter(|(_, depth)| *depth == 0).count();
    let mut out = format!("{} {kind} run(s), {scope}", rows.len());
    if tops > LIST_CAP {
        out.push_str(&format!(", showing the newest {LIST_CAP} of {tops}"));
    }
    // The conversation goes before the task, which is wide enough to push
    // a last column out of the pane.
    out.push_str(if flags.all {
        "\n\n| Run | Status | Conversation | Task |\n|---|---|---|---|\n"
    } else {
        "\n\n| Run | Status | Task |\n|---|---|---|\n"
    });
    let mut shown = 0;
    for (run, depth) in &rows {
        if *depth == 0 {
            shown += 1;
            if shown > LIST_CAP {
                break;
            }
        }
        // In the code span: the monospace font has box drawing, the text
        // font doesn't.
        let mark = if *depth == 0 { "" } else { "└ " };
        out.push_str(&format!("| `{mark}{}` | {} |", run.run_id, run.status));
        if flags.all {
            let conversation = run.session_id.as_deref().unwrap_or("(none)");
            out.push_str(&format!(" {} |", cell(conversation)));
        }
        out.push_str(&format!(" {} |\n", cell(&run.task)));
    }
    out
}

/// What a source is called in the list: its title, else its URL's host
/// without `www.`, else the URL.
fn source_name(source: &ppxai::Source) -> &str {
    if let Some(title) = source.title.as_deref().filter(|t| !t.trim().is_empty()) {
        return title;
    }
    let rest = source
        .url
        .split_once("://")
        .map_or(source.url.as_str(), |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.strip_prefix("www.").unwrap_or(host);
    if host.is_empty() { &source.url } else { host }
}

/// Whether a link is http or https, the only schemes the chat opens.
/// The bar that asks whether this conversation's web images may load. It
/// names their hosts, and which of them send a query with the request,
/// since loading an image tells its host the URL, and an answer can put
/// anything into it.
fn web_images_bar(ui: &mut egui::Ui) {
    let hosts = crate::images::pending_hosts();
    if hosts.is_empty() {
        return;
    }
    egui::Frame::group(ui.style()).show(ui, |ui| {
        let names = hosts
            .iter()
            .map(|(host, query)| {
                if *query {
                    format!("{host} (with a query)")
                } else {
                    host.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        ui.add(
            egui::Label::new(format!(
                "Answers here show web images from {names}. Loading one tells \
                 its host the full address, which can carry data from this \
                 conversation."
            ))
            .wrap(),
        );
        ui.horizontal(|ui| {
            if ui
                .button("Load web images")
                .on_hover_text("For this conversation; /clear asks again")
                .clicked()
            {
                crate::images::set_web(ui.ctx(), crate::images::Web::Allowed);
            }
            if ui
                .button("Keep them as links")
                .on_hover_text("For this conversation; /clear asks again")
                .clicked()
            {
                crate::images::set_web(ui.ctx(), crate::images::Web::Denied);
            }
        });
    });
}

pub fn is_web_link(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

#[cfg(test)]
mod tests {
    use super::{
        Block, Entry, Family, LIST_CAP, Live, blocks, command, common_prefix, finish, is_web_link,
        link_citations, nest, redact, runs_table, source_name, tool_summary, truncate, unquote,
    };
    use crate::grammar::ListFlags;
    use crate::ppxai::{ChatEvent, CommandSpec, Source, ToolKind, ToolStep};

    fn run(id: &str, parent: Option<&str>, session: Option<&str>) -> crate::ppxai::Run {
        crate::ppxai::Run::parse(&serde_json::json!({
            "run_id": id, "status": "running", "task": format!("task {id}"),
            "kind": "task", "parent_run_id": parent, "session_id": session,
        }))
        .unwrap()
    }

    // ppxai `tests/test_run_ls_scope.py`'s nesting cases.
    #[test]
    fn ls_puts_sub_agents_under_their_parent() {
        let ids = |rows: Vec<(crate::ppxai::Run, usize)>| -> Vec<(String, usize)> {
            rows.into_iter().map(|(r, d)| (r.run_id, d)).collect()
        };
        let pair = |id: &str, d: usize| (id.to_string(), d);
        // Newest first: a child is listed before its parent.
        let runs = vec![
            run("c2", Some("p"), None),
            run("q", None, None),
            run("c1", Some("p"), None),
            run("p", None, None),
            run("orphan", Some("gone"), None),
            run("self", Some("self"), None),
        ];
        assert_eq!(
            ids(nest(runs)),
            vec![
                pair("q", 0),
                pair("p", 0),
                pair("c2", 1),
                pair("c1", 1),
                pair("orphan", 0),
                pair("self", 0),
            ]
        );
    }

    #[test]
    fn ls_tables_say_their_scope() {
        let flags = |all: bool| ListFlags { all, status: None };
        assert_eq!(
            runs_table(vec![], "/task", Family::Task, &flags(false)),
            "No task runs (this conversation). `/task ls --all` shows every conversation."
        );
        let waiting = ListFlags {
            all: true,
            status: Some("waiting".into()),
        };
        assert_eq!(
            runs_table(vec![], "/run", Family::Run, &waiting),
            "No oneshot runs (all conversations, status waiting)."
        );
        let runs = vec![run("c", Some("p"), Some("session_a")), run("p", None, None)];
        let table = runs_table(runs, "/task", Family::Task, &flags(true));
        assert!(table.starts_with(
            "2 task run(s), all conversations\n\n| Run | Status | Conversation | Task |"
        ));
        assert!(table.contains(
            "| `p` | running | (none) | task p |\n| `└ c` | running | session_a | task c |"
        ));
        let many: Vec<_> = (0..LIST_CAP + 2)
            .map(|i| run(&format!("r{i}"), None, None))
            .collect();
        let table = runs_table(many, "/task", Family::Task, &flags(false));
        assert!(table.contains("showing the newest 20 of 22"));
        assert!(table.contains("`r19`") && !table.contains("`r20`"));
    }

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
                sources: Vec::new(),
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
                    sources: Vec::new(),
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

    /// A rate limit, a cut-short tool loop and a tripped breaker each say
    /// so; state changes wait for the pane.
    #[test]
    fn a_turn_says_why_it_stopped_short() {
        let throttled = finish(
            live(vec![ChatEvent::Throttled("p refused the request".into())]),
            false,
            None,
        );
        assert!(matches!(&throttled[..], [(Entry::Error(e), _)] if e == "p refused the request"));

        let capped = live(vec![
            ChatEvent::State(crate::ppxai::StateSync {
                working_dir: Some("/w".into()),
                ..Default::default()
            }),
            ChatEvent::Degraded(vec!["iteration_cap".into()]),
            ChatEvent::Stopped("Circuit breaker tripped.".into()),
            ChatEvent::End {
                text: "best effort".into(),
                tokens: None,
                sources: Vec::new(),
            },
        ]);
        assert_eq!(capped.state.working_dir.as_deref(), Some("/w"));
        assert_eq!(
            capped.reply.notes,
            [
                "The tools were cut short: it used every tool step it had, so it answered \
                 from what it had found.",
                "Stopped early: Circuit breaker tripped."
            ]
        );
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
                n: 1,
                url: "https://a.example/x".into(),
                title: Some("a.example".into()),
            },
            Source {
                n: 2,
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

    #[test]
    fn a_chat_answer_links_the_sources_its_turn_cites() {
        let source = |n: usize, url: &str| Source {
            n,
            url: url.into(),
            title: None,
        };
        let out = finish(
            live(vec![ChatEvent::End {
                text: "Two [2] and three [3].".into(),
                tokens: None,
                sources: vec![
                    source(2, "https://b.example/"),
                    source(3, "https://c.example/"),
                ],
            }]),
            false,
            None,
        );
        let Entry::Answer(reply) = &out[0].0 else {
            panic!("not an answer");
        };
        // Found by `n`, not by place in the list.
        assert!(
            reply.text.contains(r"[\[2\]](<https://b.example/>)"),
            "{}",
            reply.text
        );
        assert!(
            reply.text.contains(r"[\[3\]](<https://c.example/>)"),
            "{}",
            reply.text
        );
        assert_eq!(reply.sources.len(), 2);
    }

    #[test]
    fn a_source_is_named_by_its_title_or_its_host() {
        let named = |url: &str, title: Option<&str>| {
            source_name(&Source {
                n: 1,
                url: url.into(),
                title: title.map(str::to_string),
            })
            .to_string()
        };
        assert_eq!(named("https://www.uefa.com/x?y", None), "uefa.com");
        assert_eq!(
            named("https://user@a.example:8/p", Some(" ")),
            "a.example:8"
        );
        assert_eq!(named("https://a.example/", Some("A page")), "A page");
        assert_eq!(named("not a url", None), "not a url");
    }

    #[test]
    fn run_strips_one_layer_of_quotes() {
        assert_eq!(unquote("\"hello world\""), "hello world");
        assert_eq!(unquote("'a' and 'b'"), "a' and 'b");
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(unquote("\""), "\"");
    }
}
