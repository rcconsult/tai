//! ppxai's wire format, in one place (ADR 0001 decision 5): the routes
//! tai-gui calls, the JSON it sends and how it reads the answers. Nothing
//! else in tai builds a ppxai request or parses a ppxai body.
//!
//! Every call names one engine slot in `X-Session-Id` (ppxai
//! `server/state.py`). The slot holds the active conversation, so chat
//! calls and the runs started from them must name the same slot: ppxai
//! stamps a run's `session_id` with that slot's conversation, and merges a
//! run's result only into it (ppxai ADR 0018, `f7c6acee`, "feat(agent):
//! runs know their conversation; a result merges once (ADR 0018)").
//!
//! How far each route can be trusted (ppxai `docs/api-gateway.md`):
//! - `/v1/oneshot` is stable.
//! - `/v1/agent/*` isn't sealed. tai relies only on what ppxai's fence
//!   (`tests/test_run_session_contract.py`) pins: `session_id`,
//!   `merged_at`, `?session=`, `?status=`, the two structured 409s of
//!   `/sessions/merge-run-result`, and `GET /status`'s `session_name`, the
//!   slot's conversation. The other fields are read when present.
//! - `GET /providers`, `POST /providers` and `GET /models` are internal
//!   (`docs/api-gateway.md`, "What's not in the gateway tier"), read for
//!   the chat pane's provider and model picker. The contract test covers
//!   them.
//! - `/chat` (SSE), `/interrupt`, `/consent`, `/shell-consent`, `GET` and
//!   `POST /tools`, `GET /commands` and `POST /command/<name>` are internal
//!   and may change with any ppxai release; they're the conversation the
//!   web app has. The command set is ppxai's roster (ppxai ADR 0007), not
//!   a list in tai.
//!   `ChatEvent` reads only the events tai shows, so a new event type
//!   doesn't break a stream. The contract test covers them.
//! - A conversation name is opaque: `session_` and more, unique per
//!   conversation since ppxai `bac85e6e`. `GET /status`'s `session_id` is
//!   the slot, not the conversation.
//!
//! `contract_against_a_live_server` below checks all of it against a real
//! `ppxai-server --owned`.

// The run calls have no caller outside the contract test until tai-gui's
// run views exist (ADR 0011, "Reference: runs spawned from a chat session").
#![cfg_attr(not(test), allow(dead_code))]

use std::{
    io::BufRead,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use serde_json::Value;

use crate::engine::{Link, random_hex};

/// How long a one-shot answer may take. Models can be slow; ppxai's own
/// enrichment path has its own timeout and answers 504 past it.
const ONESHOT_TIMEOUT: Duration = Duration::from_secs(300);
/// How long starting a run or merging its result may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a slash command may take; `/doctor probe` calls providers.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// The client id tai-gui gives the roster and commands. ppxai knows only
/// its own clients (`KNOWN_CLIENTS`, `commands/factory.py`) and answers
/// 400 to others; tai-gui follows the web app, so it speaks as it.
const COMMAND_CLIENT: &str = "web";

/// ppxai through one engine slot.
#[derive(Clone)]
pub struct Client {
    link: Link,
}

impl Client {
    /// A client on a new slot of its own. ppxai creates the slot on the
    /// first call that resolves it.
    pub fn new(link: &Link) -> Result<Self, String> {
        let slot = random_hex(8).map_err(|e| format!("no randomness for a slot id: {e}"))?;
        Ok(Self {
            link: link.in_slot(format!("tai-{slot}")),
        })
    }

    /// One prompt, no history: `POST /v1/oneshot`. With no provider or
    /// model, ppxai uses its `default_provider` and that provider's
    /// `default_model`.
    pub fn oneshot(
        &self,
        prompt: &str,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<Answer, String> {
        let mut body = serde_json::json!({ "prompt": prompt });
        if let Some(provider) = provider {
            body["provider"] = provider.into();
        }
        if let Some(model) = model {
            body["model"] = model.into();
        }
        match self.link.post_json("/v1/oneshot", &body, ONESHOT_TIMEOUT)? {
            (200, text) => Answer::parse(&text),
            (status, text) => Err(format!("{status}: {}", detail(&text))),
        }
    }

    /// The providers ppxai has configured, and the slot's current one:
    /// `GET /providers`.
    pub fn providers(&self) -> Result<Providers, String> {
        match self.link.get("/providers")? {
            (200, text) => Providers::parse(&json(&text)?),
            (status, text) => Err(format!("/providers: {status}: {}", detail(&text))),
        }
    }

    /// The models of `provider`. ppxai lists only the slot's current
    /// provider's models, so this makes `provider` the slot's provider
    /// first (`POST /providers`, then `GET /models`).
    pub fn models(&self, provider: &str) -> Result<Vec<Model>, String> {
        let body = serde_json::json!({ "provider": provider });
        match self.link.post_json("/providers", &body, CALL_TIMEOUT)? {
            (200, _) => {}
            (status, text) => return Err(format!("{provider}: {status}: {}", detail(&text))),
        }
        match self.link.get("/models")? {
            (200, text) => Model::parse_list(&json(&text)?),
            (status, text) => Err(format!("/models: {status}: {}", detail(&text))),
        }
    }

    /// The name of the slot's active conversation (`session_…`). Also
    /// creates the slot, or recreates it if ppxai expired it.
    pub fn conversation(&self) -> Result<String, String> {
        match self.link.get("/status")? {
            (200, text) => {
                let v = json(&text)?;
                v["session_name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| format!("/status has no session_name: {text}"))
            }
            (status, text) => Err(format!("/status: {status}: {}", detail(&text))),
        }
    }

    /// Start a tool-free run of `task` (`POST /v1/agent/run`), stamped with
    /// the slot's conversation: its id.
    pub fn start_run(
        &self,
        task: &str,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<String, String> {
        // ppxai stamps only from a slot that exists, and never creates one
        // for this; an expired slot would leave the run unstamped.
        self.conversation()?;
        let mut body = serde_json::json!({ "task": task });
        if let Some(provider) = provider {
            body["provider"] = provider.into();
        }
        if let Some(model) = model {
            body["model"] = model.into();
        }
        match self.link.post_json("/v1/agent/run", &body, CALL_TIMEOUT)? {
            (200..=202, text) => json(&text)?["run_id"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("no run_id in {text}")),
            (status, text) => Err(format!("starting a run: {status}: {}", detail(&text))),
        }
    }

    /// One run: `GET /v1/agent/runs/{id}`.
    pub fn run(&self, id: &str) -> Result<Run, String> {
        match self.link.get(&format!("/v1/agent/runs/{}", encode(id)))? {
            (200, text) => Run::parse(&json(&text)?),
            (status, text) => Err(format!("run {id}: {status}: {}", detail(&text))),
        }
    }

    /// The runs `filter` keeps: `GET /v1/agent/runs`.
    pub fn runs(&self, filter: &RunFilter) -> Result<Vec<Run>, String> {
        match self.link.get(&filter.path())? {
            (200, text) => json(&text)?["runs"]
                .as_array()
                .ok_or_else(|| format!("no runs list in {text}"))?
                .iter()
                .map(Run::parse)
                .collect(),
            (status, text) => Err(format!("listing runs: {status}: {}", detail(&text))),
        }
    }

    /// Run `id`'s events after `since`, oldest first: `GET
    /// /v1/agent/runs/{id}/events`, the replay without `live`.
    pub fn run_events(&self, id: &str, since: u64) -> Result<Vec<RunEvent>, String> {
        let path = format!("/v1/agent/runs/{}/events?since={since}", encode(id));
        match self.link.get(&path)? {
            (200, text) => Ok(json(&text)?["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(RunEvent::parse)
                .collect()),
            (status, text) => Err(format!("events: {status}: {}", detail(&text))),
        }
    }

    /// Ask run `id` to stop, `POST /v1/agent/runs/{id}/cancel`.
    pub fn cancel_run(&self, id: &str) -> Result<(), String> {
        let path = format!("/v1/agent/runs/{}/cancel", encode(id));
        match self
            .link
            .post_json(&path, &serde_json::json!({}), CALL_TIMEOUT)?
        {
            (200..=202, _) => Ok(()),
            (status, text) => Err(format!("cancel: {status}: {}", detail(&text))),
        }
    }

    /// Take a held result, `POST /v1/agent/runs/{id}/ack`:
    /// `completed_pending_ack` becomes `finalized`, and a finalized run
    /// stays so (200). 409 when the run holds nothing, which the web app's
    /// Collect ignores.
    pub fn ack_run(&self, id: &str) -> Result<(), String> {
        let path = format!("/v1/agent/runs/{}/ack", encode(id));
        match self
            .link
            .post_json(&path, &serde_json::json!({}), CALL_TIMEOUT)?
        {
            (200, _) => Ok(()),
            (status, text) => Err(format!("ack: {status}: {}", detail(&text))),
        }
    }

    /// How run results reach the conversation, `GET /config/execution`'s
    /// `collect`: `auto` (merged when done), `yes` (a Collect button) or
    /// `no`. The web app's default is `yes`.
    pub fn collect_mode(&self) -> Result<String, String> {
        match self.link.get("/config/execution")? {
            (200, text) => Ok(json(&text)?["collect"]
                .as_str()
                .filter(|m| matches!(*m, "auto" | "yes" | "no"))
                .unwrap_or("yes")
                .to_string()),
            (status, text) => Err(format!("/config/execution: {status}: {}", detail(&text))),
        }
    }

    /// Merge run `id`'s result into the slot's active conversation, as a
    /// task and answer pair: `POST /sessions/merge-run-result`.
    pub fn merge(&self, id: &str) -> Result<(), MergeError> {
        let body = serde_json::json!({ "run_id": id });
        match self
            .link
            .post_json("/sessions/merge-run-result", &body, CALL_TIMEOUT)
        {
            Ok((200, _)) => Ok(()),
            Ok((status, text)) => Err(MergeError::parse(status, &text)),
            Err(e) => Err(MergeError::Failed(e)),
        }
    }
}

impl Client {
    /// Send `message` to the slot's conversation, `POST /chat`, and hand
    /// each event tai shows to `on` as it arrives. The body is only the
    /// message, as the web app sends it: naming a provider or model makes
    /// ppxai switch first, which it refuses (500) while an earlier answer is
    /// still running, even an interrupted one. Without them, the request
    /// waits on the slot's chat lock instead. Returns once the stream ends,
    /// or soon after `stop` is set: ppxai sends a keepalive at least every
    /// 5 s, so the reader wakes.
    pub fn chat(
        &self,
        message: &str,
        stop: &AtomicBool,
        mut on: impl FnMut(ChatEvent),
    ) -> Result<(), String> {
        let body = serde_json::json!({ "message": message });
        let reader = match self.link.post_stream("/chat", &body)? {
            Ok(reader) => reader,
            Err((status, text)) => return Err(format!("{status}: {}", detail(&text))),
        };
        for line in reader.lines() {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let line = line.map_err(|e| format!("the stream broke: {e}"))?;
            match sse_line(&line) {
                Some(Sse::Done) => return Ok(()),
                Some(Sse::Event(v)) => {
                    if let Some(event) = ChatEvent::parse(&v) {
                        on(event);
                    }
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Switch the slot to `model`, `POST /models`, as the web app's model
    /// picker does: how many messages ppxai cleared from the context, if
    /// any (its default `reset_context`). 409 while an answer is in flight.
    pub fn set_model(&self, model: &str) -> Result<Option<u64>, String> {
        let body = serde_json::json!({ "model": model });
        match self.link.post_json("/models", &body, CALL_TIMEOUT)? {
            (200, text) => Ok(json(&text)?["context_reset"].as_u64()),
            (status, text) => Err(format!("{status}: {}", detail(&text))),
        }
    }

    /// Stop the slot's answer in flight, `POST /interrupt`. Its stream then
    /// ends with the error "Interrupted by user".
    pub fn interrupt(&self) -> Result<(), String> {
        let body = serde_json::json!({});
        match self.link.post_json("/interrupt", &body, CALL_TIMEOUT)? {
            (200, _) => Ok(()),
            (status, text) => Err(format!("/interrupt: {status}: {}", detail(&text))),
        }
    }

    /// Answer a consent request: `y`, `n`, `always` or `never` (ppxai
    /// `common/consent.py`). ppxai matches it to the request by the exact
    /// path or command the request named.
    pub fn consent(&self, request: &Consent, response: &str) -> Result<(), String> {
        let (path, body) = match request {
            Consent::File { path } => (
                "/consent",
                serde_json::json!({ "file_path": path, "response": response }),
            ),
            Consent::Shell {
                command,
                working_dir,
                ..
            } => (
                "/shell-consent",
                serde_json::json!({
                    "command": command,
                    "working_dir": working_dir,
                    "response": response,
                }),
            ),
        };
        match self.link.post_json(path, &body, CALL_TIMEOUT)? {
            (200, _) => Ok(()),
            (status, text) => Err(format!("{path}: {status}: {}", detail(&text))),
        }
    }

    /// Whether the conversation's tools are on, `GET /tools`. ppxai starts
    /// a slot with them off.
    pub fn tools(&self) -> Result<bool, String> {
        match self.link.get("/tools")? {
            (200, text) => json(&text)?["enabled"]
                .as_bool()
                .ok_or_else(|| format!("/tools has no enabled: {text}")),
            (status, text) => Err(format!("/tools: {status}: {}", detail(&text))),
        }
    }

    /// Turn the conversation's tools on or off, `POST /tools`: whether
    /// they're on now.
    pub fn set_tools(&self, enabled: bool) -> Result<bool, String> {
        let body = serde_json::json!({ "enabled": enabled });
        match self.link.post_json("/tools", &body, CALL_TIMEOUT)? {
            (200, text) => json(&text)?["enabled"]
                .as_bool()
                .ok_or_else(|| format!("/tools has no enabled: {text}")),
            (status, text) => Err(format!("/tools: {status}: {}", detail(&text))),
        }
    }

    /// ppxai's slash commands as this client sees them, `GET /commands`.
    pub fn commands(&self) -> Result<Roster, String> {
        let path = format!("/commands?client={COMMAND_CLIENT}");
        match self.link.get(&path)? {
            (200, text) => Roster::parse(&json(&text)?),
            (status, text) => Err(format!("/commands: {status}: {}", detail(&text))),
        }
    }

    /// Run slash command `name` on the server, `POST /command/<name>`. An
    /// unknown name gets ppxai's own 404 message.
    pub fn command(&self, name: &str, args: &str) -> Result<CommandOutcome, String> {
        let body = serde_json::json!({ "args": args, "client": COMMAND_CLIENT });
        let path = format!("/command/{}", encode(name));
        match self.link.post_json(&path, &body, COMMAND_TIMEOUT)? {
            (200, text) => Ok(CommandOutcome::parse(&json(&text)?)),
            (_, text) => Err(detail(&text)),
        }
    }
}

/// ppxai's command roster (`CommandFactory.roster`, ppxai ADR 0007).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Roster {
    pub commands: Vec<CommandSpec>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandSpec {
    pub name: String,
    pub aliases: Vec<String>,
    pub description: String,
    pub usage: String,
    pub hidden: bool,
    /// Whether the client runs it (`dispatch: "client"`) instead of
    /// sending it to `POST /command/<name>`.
    pub client_dispatch: bool,
    /// The client behaviour ppxai binds to it, such as `run.controller` or
    /// `coding.stream`; the client owns the code.
    pub client_action: Option<String>,
    /// The subcommands whose value is a secret, never to be echoed.
    pub sensitive: Vec<String>,
    /// The subcommand names.
    pub subcommands: Vec<String>,
}

impl Roster {
    fn parse(v: &Value) -> Result<Self, String> {
        let list = v["commands"]
            .as_array()
            .ok_or_else(|| format!("/commands has no commands: {v}"))?;
        let strings = |v: &Value| -> Vec<String> {
            v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        };
        let commands = list
            .iter()
            .filter_map(|c| {
                let subs = c["subcommands"].as_array().cloned().unwrap_or_default();
                Some(CommandSpec {
                    name: c["name"].as_str()?.to_string(),
                    aliases: strings(&c["aliases"]),
                    description: c["description"].as_str().unwrap_or_default().to_string(),
                    usage: c["usage"].as_str().unwrap_or_default().to_string(),
                    hidden: c["hidden"].as_bool().unwrap_or(false),
                    client_dispatch: c["dispatch"] == "client",
                    client_action: c["client_action"].as_str().map(str::to_string),
                    sensitive: subs
                        .iter()
                        .filter(|s| s["sensitive"] == true)
                        .filter_map(|s| s["name"].as_str().map(str::to_string))
                        .collect(),
                    subcommands: subs
                        .iter()
                        .filter_map(|s| s["name"].as_str().map(str::to_string))
                        .collect(),
                })
            })
            .collect();
        Ok(Self { commands })
    }

    /// The command `name` (without the slash) names, by name or alias,
    /// ignoring case.
    pub fn resolve(&self, name: &str) -> Option<&CommandSpec> {
        let name = name.to_ascii_lowercase();
        self.commands
            .iter()
            .find(|c| c.name == name || c.aliases.contains(&name))
    }
}

/// What `POST /command/<name>` answered: its result as Markdown, whether it
/// failed, and the state it changed.
#[derive(Debug, PartialEq)]
pub struct CommandOutcome {
    pub ok: bool,
    /// The result's `status` was `error`.
    pub error: bool,
    pub markdown: String,
    /// `tools_enabled` from the envelope's `state_sync` events.
    pub tools_enabled: Option<bool>,
}

impl CommandOutcome {
    fn parse(v: &Value) -> Self {
        let tools_enabled = v["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["type"] == "state_sync")
            .filter_map(|e| e["data"]["tools_enabled"].as_bool())
            .next_back();
        Self {
            ok: v["ok"].as_bool().unwrap_or(true),
            error: v["result"]["status"] == "error",
            markdown: result_markdown(&v["result"]),
            tools_enabled,
        }
    }
}

/// A `CommandResult.to_dict()` as Markdown (ppxai `commands/results.py`),
/// the way the web app's `ResultRenderer` shows each type. An unknown type
/// shows its message, or its JSON.
fn result_markdown(r: &Value) -> String {
    let message = r["message"].as_str().unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    let title = |parts: &mut Vec<String>| {
        if !message.is_empty() {
            parts.push(format!("**{}**", escape_markdown(message)));
        }
    };
    let cell = |v: &Value| -> String {
        let text = match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        escape_markdown(&text).replace('\n', " ")
    };
    match r["type"].as_str().unwrap_or_default() {
        "MarkdownResult" => match r["content"].as_str() {
            Some(content) => parts.push(content.to_string()),
            None => title(&mut parts),
        },
        "CompositeResult" => {
            title(&mut parts);
            for sub in r["results"].as_array().into_iter().flatten() {
                parts.push(result_markdown(sub));
            }
        }
        "TableResult" => {
            title(&mut parts);
            let columns: Vec<String> = r["columns"]
                .as_array()
                .into_iter()
                .flatten()
                .map(cell)
                .collect();
            let rows = r["rows"].as_array().cloned().unwrap_or_default();
            if !columns.is_empty() && !rows.is_empty() {
                let mut table = format!(
                    "| {} |\n|{}\n",
                    columns.join(" | "),
                    "---|".repeat(columns.len())
                );
                for row in &rows {
                    let cells: Vec<String> = match row {
                        Value::Array(cells) => cells.iter().map(cell).collect(),
                        Value::Object(map) => columns
                            .iter()
                            .map(|c| map.get(c).map(cell).unwrap_or_default())
                            .collect(),
                        other => vec![cell(other)],
                    };
                    table.push_str(&format!("| {} |\n", cells.join(" | ")));
                }
                parts.push(table);
            }
        }
        "KeyValueResult" => {
            title(&mut parts);
            let pairs: Vec<String> = r["pairs"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| format!("- **{}:** {}", escape_markdown(k), cell(v)))
                .collect();
            if !pairs.is_empty() {
                parts.push(pairs.join("\n"));
            }
        }
        "ListResult" => {
            title(&mut parts);
            let items: Vec<String> = r["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| format!("- {}", cell(item.get("text").unwrap_or(item))))
                .collect();
            if !items.is_empty() {
                parts.push(items.join("\n"));
            }
        }
        "DirectoryTreeResult" => {
            let mut lines = Vec::new();
            tree_lines(&r["root"], 0, &mut lines);
            if !lines.is_empty() {
                parts.push(format!("```text\n{}\n```", lines.join("\n")));
            }
            title(&mut parts);
        }
        _ => {
            title(&mut parts);
            if parts.is_empty() && !r.is_null() {
                parts.push(format!(
                    "```json\n{}\n```",
                    serde_json::to_string_pretty(r).unwrap_or_default()
                ));
            }
        }
    }
    match &r["details"] {
        Value::String(d) if !d.is_empty() => parts.push(escape_markdown(d)),
        Value::Array(list) if !list.is_empty() => parts.push(
            list.iter()
                .map(|d| format!("- {}", cell(d)))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => {}
    }
    parts.join("\n\n")
}

/// A `DirectoryTreeResult` node and its children, indented, at most 500
/// lines: a tree of a home directory has thousands.
fn tree_lines(node: &Value, depth: usize, out: &mut Vec<String>) {
    const MAX: usize = 500;
    let Some(label) = node["label"].as_str() else {
        return;
    };
    if out.len() == MAX {
        out.push("…".into());
        return;
    }
    if out.len() > MAX {
        return;
    }
    out.push(format!("{}{label}", "  ".repeat(depth)));
    for child in node["children"].as_array().into_iter().flatten() {
        tree_lines(child, depth + 1, out);
    }
}

/// Text from the server shown as Markdown without being read as Markdown:
/// its punctuation escaped.
fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(
            ch,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' | '~'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// One line of an SSE stream that carries something: ppxai frames each
/// event as `data: {json}` and ends with `data: [DONE]`, sends `: keepalive`
/// comments, and uses no `event:` lines (ppxai `server/streaming.py`).
#[derive(Debug, PartialEq)]
enum Sse {
    Event(Value),
    Done,
}

fn sse_line(line: &str) -> Option<Sse> {
    let data = line.strip_prefix("data:")?.trim_start();
    if data == "[DONE]" {
        return Some(Sse::Done);
    }
    match serde_json::from_str(data) {
        Ok(v) => Some(Sse::Event(v)),
        Err(e) => {
            tracing::warn!("chat: an SSE line that isn't JSON ({e}): {data}");
            None
        }
    }
}

/// An event of a `/chat` stream that tai shows (ppxai `EventType`,
/// `engine/types.py`). The others, such as `state_sync` and the agent
/// loop's, are dropped.
#[derive(Debug, PartialEq)]
pub enum ChatEvent {
    /// The answer starts; the model answering.
    Start {
        model: Option<String>,
    },
    Chunk(String),
    Reasoning(String),
    /// The answer is complete. `text` is the whole of it, empty when ppxai
    /// had none to give; tokens are this turn's, and so are the sources its
    /// `[n]` markers cite (ppxai ADR 0019; none when it cited nothing).
    End {
        text: String,
        tokens: Option<u64>,
        sources: Vec<Source>,
    },
    Tool(ToolStep),
    /// `agent_intermediate_prose`: what the model said between tool steps.
    /// With tools on, ppxai calls the model without streaming
    /// (`engine/chat.py` `chat_with_tools`), so this is all that shows
    /// until the answer arrives whole in `End`.
    Prose(String),
    Consent(Consent),
    /// `status`: a line for the transcript.
    Status(String),
    /// `info`: what the engine is doing, while the answer is still empty.
    Info(String),
    Warning(String),
    /// `error`; an interrupted stream ends with "Interrupted by user".
    Error(String),
}

/// A tool call, its result or its error, as `tool_call`, `tool_result`
/// and `tool_error` carry them.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolStep {
    pub kind: ToolKind,
    pub tool: String,
    /// The call's arguments, the result or the error, as text.
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ToolKind {
    Call,
    Result,
    Error,
}

/// What a `consent_request` asks to do (ppxai `engine/consent_ops.py`).
/// Unanswered, ppxai denies it after 300 s for a file, 60 s for a command.
#[derive(Clone, Debug, PartialEq)]
pub enum Consent {
    /// Edit a file; the path is resolved and absolute.
    File { path: String },
    /// Run a shell command. ppxai asks only for commands it doesn't rate
    /// `safe`, and refuses `never` ones without asking.
    Shell {
        command: String,
        working_dir: String,
        risk: Option<String>,
    },
}

impl ChatEvent {
    fn parse(v: &Value) -> Option<Self> {
        let data = &v["data"];
        let string = || match data {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        let text = |value: &Value| match value {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
        };
        let tool = |kind, key| {
            Some(Self::Tool(ToolStep {
                kind,
                tool: data["tool"].as_str().unwrap_or("?").to_string(),
                detail: text(&data[key]),
            }))
        };
        match v["type"].as_str()? {
            "stream_start" => Some(Self::Start {
                model: data["model"].as_str().map(str::to_string),
            }),
            "stream_chunk" => Some(Self::Chunk(string())),
            "reasoning_chunk" => Some(Self::Reasoning(string())),
            "stream_end" => {
                let usage = &v["metadata"]["usage"];
                let tokens = usage["total_tokens"].as_u64().or_else(|| {
                    Some(usage["prompt_tokens"].as_u64()? + usage["completion_tokens"].as_u64()?)
                });
                Some(Self::End {
                    text: data.as_str().unwrap_or_default().to_string(),
                    tokens: tokens.filter(|&n| n > 0),
                    sources: Source::parse_list(&v["metadata"]["sources"]),
                })
            }
            "tool_call" => tool(ToolKind::Call, "arguments"),
            "tool_result" => tool(ToolKind::Result, "result"),
            "tool_error" => tool(ToolKind::Error, "error"),
            "agent_intermediate_prose" => data["text"]
                .as_str()
                .filter(|t| !t.trim().is_empty())
                .map(|t| Self::Prose(t.to_string())),
            "consent_request" => {
                if data["type"] == "shell" || data["command"].is_string() {
                    Some(Self::Consent(Consent::Shell {
                        command: data["command"].as_str()?.to_string(),
                        working_dir: data["working_dir"].as_str().unwrap_or(".").to_string(),
                        risk: data["risk_level"].as_str().map(str::to_string),
                    }))
                } else {
                    Some(Self::Consent(Consent::File {
                        path: data["file_path"].as_str()?.to_string(),
                    }))
                }
            }
            "status" => Some(Self::Status(string())),
            "info" => Some(Self::Info(string())),
            "warning" => Some(Self::Warning(
                data["message"].as_str().map_or_else(string, str::to_string),
            )),
            "error" => Some(Self::Error(string())),
            _ => None,
        }
    }
}

/// A 200 from `/v1/oneshot`.
#[derive(Debug, PartialEq)]
pub struct Answer {
    pub content: String,
    pub provider: String,
    pub model: String,
    pub total_tokens: Option<u64>,
    pub finish_reason: Option<String>,
    /// The web sources a retrieval-grounded answer was given, in order:
    /// the answer's `[n]` is `sources[n - 1]` (`grounding.sources`, ppxai
    /// ADR 0014, from v1.19.3). Empty when ppxai didn't search.
    pub sources: Vec<Source>,
}

/// One of an answer's sources: its `[n]` is the one with that `n`. ppxai
/// resolves Gemini's redirect links to real ones when it can, but a `url`
/// can still be a redirect, and a title can be a bare domain.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub n: usize,
    pub url: String,
    pub title: Option<String>,
}

impl Source {
    /// A turn's or a run's sources, `[{n, url, title, backend, query}]`
    /// (ppxai ADR 0019, `engine/sources.py`), in order of `n`. Rows without
    /// a URL or a positive `n` are dropped, other fields ignored.
    fn parse_list(v: &Value) -> Vec<Self> {
        let mut list: Vec<Self> = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                Some(Self {
                    n: usize::try_from(s["n"].as_u64()?).ok().filter(|&n| n > 0)?,
                    url: s["url"].as_str()?.to_string(),
                    title: s["title"].as_str().map(str::to_string),
                })
            })
            .collect();
        list.sort_by_key(|s| s.n);
        list
    }
}

impl Answer {
    fn parse(body: &str) -> Result<Self, String> {
        let v = json(body)?;
        let text = |key: &str| v[key].as_str().unwrap_or("?").to_string();
        Ok(Self {
            content: v["content"].as_str().unwrap_or_default().to_string(),
            provider: text("provider"),
            model: text("model"),
            total_tokens: v["usage"]["total_tokens"].as_u64(),
            finish_reason: v["finish_reason"].as_str().map(str::to_string),
            // `/v1/oneshot`'s list has no `n`: its `[n]` is `sources[n-1]`.
            sources: v["grounding"]["sources"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| Some((s["url"].as_str()?, s["title"].as_str())))
                .enumerate()
                .map(|(i, (url, title))| Source {
                    n: i + 1,
                    url: url.to_string(),
                    title: title.map(str::to_string),
                })
                .collect(),
        })
    }
}

/// A 200 from `GET /providers`.
#[derive(Debug, PartialEq)]
pub struct Providers {
    pub list: Vec<Provider>,
    /// The slot's current provider.
    pub current: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Provider {
    pub id: String,
    pub name: String,
    /// Whether ppxai has a key for it. Without one, calls to it fail.
    pub has_api_key: bool,
    pub default_model: Option<String>,
}

impl Providers {
    fn parse(v: &Value) -> Result<Self, String> {
        let list = v["providers"]
            .as_array()
            .ok_or_else(|| format!("no providers list in {v}"))?
            .iter()
            .map(|p| {
                let id = p["id"]
                    .as_str()
                    .ok_or_else(|| format!("a provider without an id: {p}"))?;
                Ok(Provider {
                    id: id.to_string(),
                    name: p["name"].as_str().unwrap_or(id).to_string(),
                    has_api_key: p["has_api_key"].as_bool().unwrap_or(true),
                    default_model: p["default_model"].as_str().map(str::to_string),
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(Self {
            list,
            current: v["current"].as_str().map(str::to_string),
        })
    }
}

/// One of a provider's models, from `GET /models`.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub id: String,
    pub name: String,
}

impl Model {
    fn parse_list(v: &Value) -> Result<Vec<Self>, String> {
        v["models"]
            .as_array()
            .ok_or_else(|| format!("no models list in {v}"))?
            .iter()
            .map(|m| {
                let id = m["id"]
                    .as_str()
                    .ok_or_else(|| format!("a model without an id: {m}"))?;
                Ok(Self {
                    id: id.to_string(),
                    name: m["name"].as_str().unwrap_or(id).to_string(),
                })
            })
            .collect()
    }
}

/// A run, from `RunMetaResponse`. `session_id` and `merged_at` are pinned
/// by ppxai's fence; the rest isn't promised.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub run_id: String,
    pub status: String,
    pub task: String,
    /// "task" or "oneshot".
    pub kind: String,
    pub parent_run_id: Option<String>,
    /// The conversation the run was started from; none for a run started
    /// without a slot.
    pub session_id: Option<String>,
    /// When its result was merged into a conversation, in epoch seconds.
    pub merged_at: Option<f64>,
    pub result: Option<String>,
    pub error: Option<String>,
    /// The sources its result cites (ppxai ADR 0019), filled as the run
    /// goes.
    pub sources: Vec<Source>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl Run {
    /// Whether the run has ended, one way or another (ppxai
    /// `engine/agent_runs.py`; the web app's terminal statuses).
    pub fn ended(&self) -> bool {
        matches!(
            self.status.as_str(),
            "completed"
                | "completed_pending_ack"
                | "finalized"
                | "failed"
                | "cancelled"
                | "interrupted"
        )
    }

    /// Whether it ended with a result.
    pub fn succeeded(&self) -> bool {
        matches!(
            self.status.as_str(),
            "completed" | "completed_pending_ack" | "finalized"
        )
    }

    fn parse(v: &Value) -> Result<Self, String> {
        let required = |key: &str| {
            v[key]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("a run without {key}: {v}"))
        };
        let optional = |key: &str| v[key].as_str().map(str::to_string);
        Ok(Self {
            run_id: required("run_id")?,
            status: required("status")?,
            task: optional("task").unwrap_or_default(),
            kind: optional("kind").unwrap_or_else(|| "task".into()),
            parent_run_id: optional("parent_run_id"),
            session_id: optional("session_id"),
            merged_at: v["merged_at"].as_f64(),
            result: optional("result"),
            error: optional("error"),
            sources: Source::parse_list(&v["sources"]),
            provider: optional("provider"),
            model: optional("model"),
        })
    }
}

/// One of a run's events (`{seq, ts, type, level, category, data}`, ppxai
/// ADR 0003 §11a).
#[derive(Clone, Debug, PartialEq)]
pub struct RunEvent {
    pub seq: u64,
    pub kind: String,
    pub data: Value,
}

impl RunEvent {
    fn parse(v: &Value) -> Option<Self> {
        Some(Self {
            seq: v["seq"].as_u64()?,
            kind: v["type"].as_str()?.to_string(),
            data: v["data"].clone(),
        })
    }
}

/// Which runs `GET /v1/agent/runs` returns. The filters combine.
#[derive(Default)]
pub struct RunFilter<'a> {
    /// Runs started from this conversation.
    pub session: Option<&'a str>,
    /// Runs in this status, such as "waiting". ppxai answers 400 to a
    /// status it doesn't know (`RUN_STATUSES`, `engine/agent_runs.py`).
    pub status: Option<&'a str>,
    /// "task" or "oneshot".
    pub kind: Option<&'a str>,
}

impl RunFilter<'_> {
    fn path(&self) -> String {
        let query: Vec<String> = [
            ("session", self.session),
            ("status", self.status),
            ("kind", self.kind),
        ]
        .into_iter()
        .filter_map(|(key, value)| Some(format!("{key}={}", encode(value?))))
        .collect();
        if query.is_empty() {
            "/v1/agent/runs".into()
        } else {
            format!("/v1/agent/runs?{}", query.join("&"))
        }
    }
}

/// Why a merge didn't happen.
#[derive(Debug, PartialEq)]
pub enum MergeError {
    /// The result is already in a conversation: treat it as done.
    AlreadyMerged {
        merged_at: Option<f64>,
        message: String,
    },
    /// The run belongs to another conversation, which must be the active
    /// one first.
    SessionMismatch {
        run_session_id: Option<String>,
        message: String,
    },
    /// Any other refusal: collect turned off (403), an unknown run (404),
    /// no result yet (409).
    Refused { status: u16, message: String },
    /// The request didn't get an answer.
    Failed(String),
}

impl MergeError {
    /// ppxai ADR 0018: the two 409s carry an object `detail` with a
    /// `code`; every other refusal keeps a string `detail`.
    fn parse(status: u16, body: &str) -> Self {
        let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let d = &v["detail"];
        let message = d["message"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| detail(body));
        match (status, d["code"].as_str()) {
            (409, Some("already_merged")) => Self::AlreadyMerged {
                merged_at: d["merged_at"].as_f64(),
                message,
            },
            (409, Some("session_mismatch")) => Self::SessionMismatch {
                run_session_id: d["run_session_id"].as_str().map(str::to_string),
                message,
            },
            _ => Self::Refused { status, message },
        }
    }
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyMerged { message, .. } | Self::SessionMismatch { message, .. } => {
                f.write_str(message)
            }
            Self::Refused { status, message } => write!(f, "{status}: {message}"),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

fn json(body: &str) -> Result<Value, String> {
    serde_json::from_str(body).map_err(|_| format!("not JSON: {body}"))
}

/// FastAPI's `{"detail": ...}` as text: a string as it is, an object's
/// `message`, anything else as JSON; the body itself when there's none.
pub fn detail(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| match &v["detail"] {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            other => Some(
                other["message"]
                    .as_str()
                    .map_or_else(|| other.to_string(), str::to_string),
            ),
        })
        .unwrap_or_else(|| body.to_string())
}

/// Percent-encode everything but RFC 3986's unreserved characters, for a
/// path segment or a query value.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };

    use super::{
        Answer, ChatEvent, Client, CommandOutcome, Consent, MergeError, Model, Provider, Providers,
        Roster, Run, RunFilter, Source, Sse, ToolKind, ToolStep, detail, encode, json, sse_line,
    };

    // The shapes below are the ones ppxai's fence pins
    // (tests/test_run_session_contract.py at bac85e6e).

    #[test]
    fn a_run_reads_its_conversation_and_merge_time() {
        let v = json(
            r#"{"run_id": "r1", "task": "t", "status": "completed_pending_ack",
                "kind": "oneshot", "session_id": "session_20261102_101010",
                "merged_at": 1793612345.5, "result": "pong", "error": null,
                "created_at": 1.0, "future_field": [1, 2]}"#,
        )
        .unwrap();
        let run = Run::parse(&v).unwrap();
        assert_eq!(run.session_id.as_deref(), Some("session_20261102_101010"));
        assert_eq!(run.merged_at, Some(1793612345.5));
        assert_eq!(run.result.as_deref(), Some("pong"));
        assert_eq!(run.kind, "oneshot");
    }

    #[test]
    fn a_run_without_a_conversation_or_merge_has_none() {
        let v = json(
            r#"{"run_id": "r2", "task": "t", "status": "running",
                "session_id": null, "merged_at": null}"#,
        )
        .unwrap();
        let run = Run::parse(&v).unwrap();
        assert_eq!((run.session_id, run.merged_at), (None, None));
        // A meta from before ppxai ADR 0018 has neither field.
        let old = json(r#"{"run_id": "r3", "status": "finalized"}"#).unwrap();
        let run = Run::parse(&old).unwrap();
        assert_eq!(
            (run.session_id, run.merged_at, run.kind.as_str()),
            (None, None, "task")
        );
    }

    #[test]
    fn a_run_needs_an_id_and_a_status() {
        assert!(Run::parse(&json(r#"{"status": "running"}"#).unwrap()).is_err());
        assert!(Run::parse(&json(r#"{"run_id": "r"}"#).unwrap()).is_err());
    }

    #[test]
    fn filters_combine_into_one_query() {
        assert_eq!(RunFilter::default().path(), "/v1/agent/runs");
        let filter = RunFilter {
            session: Some("session_20261102_101010"),
            status: Some("waiting"),
            kind: None,
        };
        assert_eq!(
            filter.path(),
            "/v1/agent/runs?session=session_20261102_101010&status=waiting"
        );
        let odd = RunFilter {
            session: Some("a b&c=d"),
            ..Default::default()
        };
        assert_eq!(odd.path(), "/v1/agent/runs?session=a%20b%26c%3Dd");
    }

    #[test]
    fn the_two_merge_409s_are_told_apart_by_code() {
        let mismatch = r#"{"detail": {"code": "session_mismatch",
            "message": "Run r1 belongs to session_a; load it first.",
            "run_session_id": "session_a"}}"#;
        assert_eq!(
            MergeError::parse(409, mismatch),
            MergeError::SessionMismatch {
                run_session_id: Some("session_a".into()),
                message: "Run r1 belongs to session_a; load it first.".into(),
            }
        );
        let merged = r#"{"detail": {"code": "already_merged",
            "message": "Run r1 was already merged.", "merged_at": 1793612345.5}}"#;
        assert_eq!(
            MergeError::parse(409, merged),
            MergeError::AlreadyMerged {
                merged_at: Some(1793612345.5),
                message: "Run r1 was already merged.".into(),
            }
        );
    }

    #[test]
    fn other_merge_refusals_keep_their_string_detail() {
        assert_eq!(
            MergeError::parse(409, r#"{"detail": "Run r1 has no result to merge."}"#),
            MergeError::Refused {
                status: 409,
                message: "Run r1 has no result to merge.".into(),
            }
        );
        assert_eq!(
            MergeError::parse(404, r#"{"detail": "Unknown run: r9"}"#),
            MergeError::Refused {
                status: 404,
                message: "Unknown run: r9".into(),
            }
        );
        // A code ppxai may add later is a refusal, with its message.
        assert_eq!(
            MergeError::parse(
                409,
                r#"{"detail": {"code": "unknown_run", "message": "gone"}}"#
            ),
            MergeError::Refused {
                status: 409,
                message: "gone".into(),
            }
        );
    }

    #[test]
    fn an_answer_reads_content_model_and_tokens() {
        let body = r#"{"content": "**hi**", "finish_reason": "stop", "model": "m1",
            "provider": "p1", "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}}"#;
        assert_eq!(
            Answer::parse(body).unwrap(),
            Answer {
                content: "**hi**".into(),
                provider: "p1".into(),
                model: "m1".into(),
                total_tokens: Some(3),
                finish_reason: Some("stop".into()),
                sources: Vec::new(),
            }
        );
        // Retrieval-grounded: the sources come in order, a title may be null.
        let grounded = r#"{"content": "1.100 [1][2]", "model": "m", "provider": "p",
            "grounding": {"searched": true, "backend": "gemini", "sources": [
                {"url": "https://a.example/x", "title": "a.example"},
                {"url": "https://b.example/y", "title": null}]}}"#;
        assert_eq!(
            Answer::parse(grounded).unwrap().sources,
            [
                Source {
                    n: 1,
                    url: "https://a.example/x".into(),
                    title: Some("a.example".into()),
                },
                Source {
                    n: 2,
                    url: "https://b.example/y".into(),
                    title: None,
                },
            ]
        );
        let cut = r#"{"content": "", "finish_reason": "length", "model": "m", "provider": "p", "usage": null}"#;
        let answer = Answer::parse(cut).unwrap();
        assert_eq!(
            (answer.total_tokens, answer.finish_reason.as_deref()),
            (None, Some("length"))
        );
    }

    #[test]
    fn providers_and_models_read_ids_names_and_keys() {
        let v = json(
            r#"{"providers": [
                {"id": "gemini", "name": "Gemini", "has_api_key": true,
                 "default_model": "gemini-3.8-flash", "capabilities": {"web_search": true}},
                {"id": "local", "has_api_key": false, "default_model": null}],
              "current": "gemini"}"#,
        )
        .unwrap();
        let providers = Providers::parse(&v).unwrap();
        assert_eq!(providers.current.as_deref(), Some("gemini"));
        assert_eq!(
            providers.list[1],
            Provider {
                id: "local".into(),
                name: "local".into(),
                has_api_key: false,
                default_model: None,
            }
        );
        let models = json(
            r#"{"models": [{"id": "m1", "name": "Model 1", "description": ""}, {"id": "m2"}],
              "current": "m1", "provider": "gemini"}"#,
        )
        .unwrap();
        assert_eq!(
            Model::parse_list(&models).unwrap()[1],
            Model {
                id: "m2".into(),
                name: "m2".into()
            }
        );
        assert!(Providers::parse(&json(r#"{"providers": [{"name": "x"}]}"#).unwrap()).is_err());
    }

    #[test]
    fn errors_show_fastapis_detail() {
        assert_eq!(
            detail(r#"{"detail": "Provider call failed: boom"}"#),
            "Provider call failed: boom"
        );
        assert_eq!(
            detail(r#"{"detail": {"code": "x", "message": "why"}}"#),
            "why"
        );
        assert_eq!(detail("plain text"), "plain text");
    }

    #[test]
    fn sse_lines_are_events_done_or_nothing() {
        assert_eq!(
            sse_line(r#"data: {"type":"stream_chunk","data":"hi"}"#),
            Some(Sse::Event(
                serde_json::json!({"type":"stream_chunk","data":"hi"})
            ))
        );
        assert_eq!(sse_line("data: [DONE]"), Some(Sse::Done));
        assert_eq!(sse_line(": keepalive"), None);
        assert_eq!(sse_line(""), None);
        assert_eq!(sse_line("data: not json"), None);
    }

    #[test]
    fn chat_events_read_what_tai_shows() {
        let parse = |v: serde_json::Value| ChatEvent::parse(&v);
        assert_eq!(
            parse(serde_json::json!({"type":"stream_start","data":{"model":"m"}})),
            Some(ChatEvent::Start {
                model: Some("m".into())
            })
        );
        assert_eq!(
            parse(serde_json::json!({"type":"stream_end","data":"all",
                "metadata":{"usage":{"prompt_tokens":3,"completion_tokens":4}}})),
            Some(ChatEvent::End {
                text: "all".into(),
                tokens: Some(7),
                sources: vec![],
            })
        );
        assert_eq!(
            parse(serde_json::json!({"type":"tool_call",
                "data":{"tool":"read_file","arguments":{"path":"a"}}})),
            Some(ChatEvent::Tool(ToolStep {
                kind: ToolKind::Call,
                tool: "read_file".into(),
                detail: "{\n  \"path\": \"a\"\n}".into(),
            }))
        );
        assert_eq!(
            parse(serde_json::json!({"type":"consent_request",
                "data":{"command":"rm x","working_dir":"/w","risk_level":"dangerous","type":"shell"}})),
            Some(ChatEvent::Consent(Consent::Shell {
                command: "rm x".into(),
                working_dir: "/w".into(),
                risk: Some("dangerous".into()),
            }))
        );
        assert_eq!(
            parse(serde_json::json!({"type":"consent_request","data":{"file_path":"/w/a.rs"}})),
            Some(ChatEvent::Consent(Consent::File {
                path: "/w/a.rs".into()
            }))
        );
        assert_eq!(
            parse(serde_json::json!({"type":"warning","data":{"message":"careful"}})),
            Some(ChatEvent::Warning("careful".into()))
        );
        assert_eq!(
            parse(serde_json::json!({"type":"agent_intermediate_prose",
                "data":{"text":"Checking the config next.","iteration":1}})),
            Some(ChatEvent::Prose("Checking the config next.".into()))
        );
        assert_eq!(
            parse(serde_json::json!({"type":"agent_intermediate_prose","data":{"text":" "}})),
            None
        );
        assert_eq!(
            parse(serde_json::json!({"type":"state_sync","data":{}})),
            None
        );
        assert_eq!(parse(serde_json::json!({"data":"no type"})), None);
    }

    #[test]
    fn the_roster_resolves_names_and_aliases() {
        let roster = Roster::parse(&serde_json::json!({"version": 1, "commands": [
            {"name":"clear","aliases":["c"],"description":"Clear","usage":"/clear",
             "hidden":false,"dispatch":"server","client_action":null,"subcommands":[]},
            {"name":"token","aliases":[],"description":"Token","usage":"/token",
             "hidden":false,"dispatch":"client","client_action":"token.manage",
             "subcommands":[{"name":"set","sensitive":true},{"name":"status","sensitive":false}]},
        ]}))
        .unwrap();
        assert_eq!(roster.resolve("C").map(|c| c.name.as_str()), Some("clear"));
        let token = roster.resolve("token").unwrap();
        assert!(token.client_dispatch);
        assert_eq!(token.client_action.as_deref(), Some("token.manage"));
        assert_eq!(token.sensitive, ["set"]);
        assert_eq!(token.subcommands, ["set", "status"]);
        assert!(roster.resolve("nosuch").is_none());
    }

    #[test]
    fn command_results_render_as_markdown() {
        let outcome = |v: serde_json::Value| CommandOutcome::parse(&v);
        let table = outcome(
            serde_json::json!({"ok": true, "result": {"type":"CompositeResult",
            "message":"", "results":[{"type":"TableResult","status":"success","message":"Usage",
            "columns":["Model","In"],"rows":[["a|b", 3]]}]}}),
        );
        assert_eq!(
            table.markdown,
            "**Usage**\n\n| Model | In |\n|---|---|\n| a\\|b | 3 |\n"
        );
        let pairs = outcome(serde_json::json!({"result": {"type":"KeyValueResult",
            "status":"success","message":"Status","pairs":{"Tools":"disabled"}}}));
        assert_eq!(pairs.markdown, "**Status**\n\n- **Tools:** disabled");
        let tools = outcome(serde_json::json!({"ok": true,
            "result": {"type":"ConfirmationResult","status":"success","message":"Tools enabled!"},
            "events": [{"type":"state_sync","data":{"tools_enabled":true}}]}));
        assert_eq!(tools.tools_enabled, Some(true));
        assert_eq!(tools.markdown, "**Tools enabled!**");
        let md = outcome(serde_json::json!({"result": {"type":"MarkdownResult",
            "status":"error","message":"x","content":"## Help"}}));
        assert!(md.error);
        assert_eq!(md.markdown, "## Help");
        let tree = outcome(serde_json::json!({"result": {"type":"DirectoryTreeResult",
            "message":"1 file","root":{"label":"d/","children":[{"label":"f","children":[]}]}}}));
        assert_eq!(tree.markdown, "```text\nd/\n  f\n```\n\n**1 file**");
    }

    #[test]
    fn encoding_keeps_only_unreserved_characters() {
        assert_eq!(encode("session_2026-11.02~x"), "session_2026-11.02~x");
        assert_eq!(encode("a/b?c"), "a%2Fb%3Fc");
        assert_eq!(encode("é"), "%C3%A9");
    }

    /// ppxai ADR 0018 against a real server: run it with
    /// `scripts/cargo.sh test --features gui --bin tai-gui -- --ignored contract`.
    /// It needs a ppxai-server with `--owned`, ADR 0018 and unique
    /// conversation names (feature/v1.19.5 at bac85e6e, "fix(session): two
    /// conversations started in the same second get distinct names", or
    /// later; `TAI_PPXAI_SERVER` picks it), a provider that
    /// answers, and `execution.collect` not set to "no". It starts one
    /// tool-free run.
    #[test]
    #[ignore = "starts ppxai-server and calls a model"]
    fn contract_against_a_live_server() {
        let engine = crate::engine::Engine::start(eframe::egui::Context::default());
        let deadline = Instant::now() + Duration::from_secs(180);
        let link = loop {
            match &*engine.state.lock().unwrap() {
                crate::engine::State::Ready(link) => break link.clone(),
                crate::engine::State::Failed(why) => panic!("the engine didn't start: {why}"),
                crate::engine::State::Starting(_) => {}
            }
            assert!(Instant::now() < deadline, "the engine didn't start in time");
            std::thread::sleep(Duration::from_millis(200));
        };

        let a = Client::new(&link).unwrap();
        let b = Client::new(&link).unwrap();
        // Back to back, in one second: before bac85e6e they shared a name.
        let conv_a = a.conversation().unwrap();
        let conv_b = b.conversation().unwrap();
        assert_ne!(conv_a, conv_b, "two slots share a conversation");

        let answer = a
            .oneshot("Reply with the single word: pong", None, None)
            .unwrap();
        assert!(
            !answer.content.is_empty(),
            "an empty one-shot answer: {answer:?}"
        );

        // The picker's calls: the provider that just answered is listed,
        // its models list, and a one-shot naming both is answered by them.
        let providers = b.providers().unwrap();
        let provider = providers
            .list
            .iter()
            .find(|p| p.id == answer.provider)
            .unwrap_or_else(|| panic!("{} isn't in {providers:?}", answer.provider));
        let models = b.models(&provider.id).unwrap();
        assert!(!models.is_empty(), "{} lists no models", provider.id);
        let model = provider
            .default_model
            .clone()
            .unwrap_or_else(|| models[0].id.clone());
        let chosen = b
            .oneshot(
                "Reply with the single word: pong",
                Some(&provider.id),
                Some(&model),
            )
            .unwrap();
        assert_eq!(chosen.provider, provider.id);

        // The conversation, on a slot of its own: `/clear` there can't
        // touch the run checks' conversation below.
        let c = Client::new(&link).unwrap();
        let stop = AtomicBool::new(false);
        let mut events = Vec::new();
        c.chat(
            "Remember the word kiwi. Reply with the single word: ok",
            &stop,
            |e| events.push(e),
        )
        .unwrap();
        assert!(
            events.iter().any(|e| matches!(e, ChatEvent::Start { .. }))
                && events.iter().any(|e| matches!(e, ChatEvent::End { .. })),
            "a chat stream without start and end: {events:?}"
        );
        let mut recalled = String::new();
        c.chat(
            "Which word did I ask you to remember? Reply with that word only.",
            &stop,
            |e| {
                if let ChatEvent::End { text, .. } = e {
                    recalled = text;
                }
            },
        )
        .unwrap();
        assert!(
            recalled.to_lowercase().contains("kiwi"),
            "the slot kept no history: {recalled:?}"
        );
        // A prompt right after an interrupted answer waits for it instead
        // of failing. /chat used to name the provider, and ppxai refused
        // that switch with a 500 while the first answer still ran.
        let early = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                c.chat(
                    "Write a 1500-word essay on the history of terminal emulators.",
                    &stop,
                    |_| {},
                )
            });
            std::thread::sleep(Duration::from_secs(2));
            c.interrupt().unwrap();
            let mut next = Vec::new();
            let second = c.chat(
                "Reply with the single word: ok",
                &AtomicBool::new(false),
                |e| next.push(e),
            );
            (first.join().unwrap(), second, next)
        });
        assert!(early.0.is_ok(), "the interrupted chat: {:?}", early.0);
        assert!(
            early.1.is_ok(),
            "the chat after an interrupt: {:?}",
            early.1
        );
        assert!(
            early.2.iter().any(|e| matches!(e, ChatEvent::End { .. })),
            "the chat after an interrupt didn't end: {:?}",
            early.2
        );
        let current = c.providers().unwrap().current.unwrap();
        let provider = c
            .providers()
            .unwrap()
            .list
            .into_iter()
            .find(|p| p.id == current)
            .unwrap();
        if let Some(model) = provider.default_model {
            c.set_model(&model).unwrap();
        }

        assert!(!c.tools().unwrap(), "a new slot starts with tools on");
        assert!(c.set_tools(true).unwrap());
        assert!(!c.set_tools(false).unwrap());
        c.interrupt().unwrap();
        let unasked = Consent::File {
            path: "/no/such/request".into(),
        };
        assert!(c.consent(&unasked, "n").is_err_and(|e| e.contains("404")));

        // The command roster and the routes tai-gui's dispatch relies on.
        let roster = c.commands().unwrap();
        let run = roster.resolve("run").expect("no /run in the roster");
        assert!(run.client_dispatch);
        assert_eq!(run.client_action.as_deref(), Some("run.controller"));
        let clear = roster
            .resolve("c")
            .expect("no /clear alias c in the roster");
        assert_eq!(clear.name, "clear");
        assert!(!clear.client_dispatch);
        assert!(
            roster
                .commands
                .iter()
                .any(|c| c.client_action.as_deref() == Some("coding.stream")),
            "no coding.stream commands in the roster"
        );
        assert!(c.command("clear", "").unwrap().ok);
        let enabled = c.command("tools", "enable").unwrap();
        assert_eq!(enabled.tools_enabled, Some(true));
        assert!(c.tools().unwrap());
        c.set_tools(false).unwrap();
        assert!(!c.command("status", "").unwrap().markdown.is_empty());
        assert!(
            c.command("nosuch", "")
                .is_err_and(|e| e.contains("Unknown command"))
        );

        let id = a
            .start_run("Reply with the single word: pong", None, None)
            .unwrap();
        let run = loop {
            let run = a.run(&id).unwrap();
            if !matches!(run.status.as_str(), "pending" | "running" | "waiting") {
                break run;
            }
            assert!(Instant::now() < deadline, "run {id} still {}", run.status);
            std::thread::sleep(Duration::from_millis(500));
        };
        assert!(
            run.result.is_some(),
            "run {id} ended {}: {:?}",
            run.status,
            run.error
        );
        assert_eq!(run.session_id.as_deref(), Some(conv_a.as_str()));
        assert_eq!(run.merged_at, None);
        assert!(
            run.ended() && run.succeeded(),
            "run {id} ended {}",
            run.status
        );
        // ADR 0019: an ungrounded run cites nothing, and says so with [].
        assert!(run.sources.is_empty(), "{:?}", run.sources);

        // Its events, in order; `since` replays only the later ones.
        let events = a.run_events(&id, 0).unwrap();
        assert!(!events.is_empty(), "run {id} has no events");
        assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));
        let first = events[0].seq;
        assert!(
            a.run_events(&id, first)
                .unwrap()
                .iter()
                .all(|e| e.seq > first)
        );
        let collect = a.collect_mode().unwrap();
        assert!(
            ["auto", "yes", "no"].contains(&collect.as_str()),
            "{collect}"
        );
        // Acking a held result finalizes it, and acking it again is fine
        // (idempotent, `routes/agent_v1.py` `ack_agent_run`).
        if run.status == "completed_pending_ack" {
            a.ack_run(&id).unwrap();
            assert_eq!(a.run(&id).unwrap().status, "finalized");
            a.ack_run(&id).unwrap();
        }

        let ids = |filter: RunFilter| -> Vec<String> {
            a.runs(&filter)
                .unwrap()
                .into_iter()
                .map(|r| r.run_id)
                .collect()
        };
        assert!(
            ids(RunFilter {
                session: Some(&conv_a),
                ..Default::default()
            })
            .contains(&id)
        );
        assert!(
            !ids(RunFilter {
                session: Some(&conv_b),
                ..Default::default()
            })
            .contains(&id)
        );
        assert!(
            !ids(RunFilter {
                session: Some(&conv_a),
                status: Some("waiting"),
                kind: None,
            })
            .contains(&id)
        );
        let bogus = a.runs(&RunFilter {
            status: Some("no-such-status"),
            ..Default::default()
        });
        assert!(bogus.is_err_and(|e| e.contains("400")));

        match b.merge(&id) {
            Err(MergeError::SessionMismatch { run_session_id, .. }) => {
                assert_eq!(run_session_id.as_deref(), Some(conv_a.as_str()));
            }
            other => panic!("merging from another conversation gave {other:?}"),
        }
        a.merge(&id).unwrap();
        assert!(a.run(&id).unwrap().merged_at.is_some());
        assert!(matches!(
            a.merge(&id),
            Err(MergeError::AlreadyMerged {
                merged_at: Some(_),
                ..
            })
        ));
        // The run pane's Collect: a result merged before counts as done.
        if collect != "no" {
            crate::runs::collect_run(&a, &id).unwrap();
        }
    }
}
