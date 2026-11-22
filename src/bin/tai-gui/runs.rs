//! The run pane, in the Preview tab: the runs started from chat (`/run`,
//! `/task`), one at a time, as the web app's right-hand run panel shows them
//! (ppxai `web/components/views/agent-run-view.js`, `task-run-view.js`).
//!
//! - A header with the run's id, status and task, and a picker of the runs
//!   opened so far.
//! - Its events as they come, without the heartbeats.
//! - Its result as Markdown, with the sources its `[n]` markers cite
//!   (`run.sources`, ppxai ADR 0019), shown as chat shows an answer's.
//! - Cancel while it runs; Collect when it holds a result
//!   (`completed_pending_ack`): ack, then merge it into the conversation,
//!   as the web app's `RunController.ack` does. Under `execution.collect:
//!   auto` the watcher merges it itself, once; under `no` Collect is off.
//! - A `/task` run's tools and working dir; Approve and Deny while it waits
//!   for consent (a sub-agent spawn); Resume when it stopped at a clean
//!   checkpoint.
//!
//! A thread watches each run by polling `GET /v1/agent/runs/{id}` and its
//! events since the last seen `seq`, every second, backing off to 30 s
//! when the engine doesn't answer and giving up after 20 failures in a
//! row, as the web app's watcher does. The live SSE tail
//! (`/events?live=1`) never closes, so polling is the simpler basic form.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use egui_commonmark::CommonMarkCache;

use crate::chat::{Reply, Slot, show_reply, spawn};
use crate::ppxai::{Client, MergeError, Run, RunEvent, Waiting};

/// How often a running run is looked at.
const POLL: Duration = Duration::from_secs(1);
/// The longest wait between looks while the engine doesn't answer.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Failures in a row before the watcher gives up.
const MAX_FAILURES: u32 = 20;
/// Events kept per run; older ones are dropped.
const MAX_EVENTS: usize = 500;

/// A run for the pane to open, from chat.
pub struct Open {
    pub id: String,
    pub client: Client,
}

/// What a run's watcher has seen.
#[derive(Default)]
struct Watch {
    run: Option<Run>,
    events: Vec<RunEvent>,
    /// The last event's `seq`.
    since: u64,
    /// The last failure, cleared by the next look that works.
    error: Option<String>,
    /// `execution.collect`, once read.
    collect: Option<String>,
    /// The watcher has stopped: the run ended or it gave up.
    finished: bool,
    /// What auto-collect did, for the transcript.
    collected: Option<Result<String, String>>,
}

struct View {
    id: String,
    client: Client,
    watch: Arc<Mutex<Watch>>,
    stop: Arc<AtomicBool>,
    /// The result as last rendered, and the status it was rendered for.
    shown: Option<(String, Reply)>,
    /// Cancel or Collect in flight.
    action: Option<Slot<Result<String, String>>>,
}

impl View {
    fn new(open: Open, ctx: &egui::Context) -> Self {
        let mut view = Self {
            id: open.id,
            client: open.client,
            watch: Arc::default(),
            stop: Arc::default(),
            shown: None,
            action: None,
        };
        view.start(ctx);
        view
    }

    /// Start a watcher; a finished one is replaced by a fresh look.
    fn start(&mut self, ctx: &egui::Context) {
        self.stop.store(true, Ordering::Relaxed);
        self.stop = Arc::default();
        if let Ok(mut w) = self.watch.lock() {
            w.finished = false;
        }
        let (client, id) = (self.client.clone(), self.id.clone());
        let (watch, stop, ctx) = (Arc::clone(&self.watch), Arc::clone(&self.stop), ctx.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("run-{id}"))
            .spawn(move || watch_run(&client, &id, &watch, &stop, &ctx));
        if let Err(e) = spawned
            && let Ok(mut w) = self.watch.lock()
        {
            w.error = Some(format!("couldn't start a thread: {e}"));
            w.finished = true;
        }
    }
}

/// The pane: the runs opened, newest last, and the one shown.
#[derive(Default)]
pub struct RunsPane {
    views: Vec<View>,
    current: usize,
    markdown: CommonMarkCache,
    /// Lines for the chat transcript, such as "collected".
    notes: Vec<String>,
}

impl RunsPane {
    /// Show run `open.id`, opening it if it's new; a run opened before is
    /// looked at again.
    pub fn open(&mut self, open: Open, ctx: &egui::Context) {
        match self.views.iter().position(|v| v.id == open.id) {
            Some(i) => {
                self.current = i;
                let finished = self.views[i].watch.lock().is_ok_and(|w| w.finished);
                if finished {
                    self.views[i].start(ctx);
                }
            }
            None => {
                self.views.push(View::new(open, ctx));
                self.current = self.views.len() - 1;
            }
        }
    }

    /// Lines for the chat transcript since the last call.
    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    /// Stop every watcher, at exit.
    pub fn stop(&self) {
        for view in &self.views {
            view.stop.store(true, Ordering::Relaxed);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if self.views.is_empty() {
            ui.weak("No runs yet. /run <prompt> or /task <prompt> --tools <a,b> in chat starts one and shows it here.");
            return;
        }
        self.collect(ui.ctx());
        self.picker(ui);
        ui.separator();
        let markdown = &mut self.markdown;
        let view = &mut self.views[self.current];
        let w = match view.watch.lock() {
            Ok(w) => w,
            Err(_) => return,
        };
        let Some(run) = w.run.clone() else {
            match &w.error {
                Some(why) => ui.colored_label(ui.visuals().error_fg_color, why),
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak(format!("Looking at run {}…", view.id));
                    })
                    .response
                }
            };
            return;
        };
        let (events, error, collect, finished) = (
            w.events.clone(),
            w.error.clone(),
            w.collect.clone(),
            w.finished,
        );
        drop(w);

        // Header.
        ui.horizontal_wrapped(|ui| {
            ui.monospace(&run.run_id);
            ui.label(egui::RichText::new(&run.status).strong());
            if !run.ended() {
                ui.spinner();
            }
            let busy = view.action.is_some();
            if !run.ended() && run.status != "cancelling" {
                let cancel = ui.add_enabled(!busy, egui::Button::new("Cancel"));
                if cancel.clicked() {
                    let (client, id) = (view.client.clone(), view.id.clone());
                    view.action = spawn(ui.ctx(), "run-cancel", move || {
                        client
                            .cancel_run(&id)
                            .map(|()| format!("Asked run {id} to stop"))
                    });
                }
            }
            if run.status == "completed_pending_ack" {
                let off = collect.as_deref() == Some("no");
                let button = ui
                    .add_enabled(!busy && !off, egui::Button::new("Collect"))
                    .on_hover_text("Put the result in the conversation")
                    .on_disabled_hover_text(
                        "Collect is off: execution.collect is \"no\" in ppxai-config.json",
                    );
                if button.clicked() {
                    let (client, id) = (view.client.clone(), view.id.clone());
                    view.action = spawn(ui.ctx(), "run-collect", move || {
                        collect_run(&client, &id)
                            .map(|()| format!("Run {id}'s result is now in the conversation"))
                    });
                }
            }
            if run.resumable && matches!(run.status.as_str(), "interrupted" | "cancelled") {
                let button = ui
                    .add_enabled(!busy, egui::Button::new("Resume"))
                    .on_hover_text("Go on from where the run stopped");
                if button.clicked() {
                    let (client, id) = (view.client.clone(), view.id.clone());
                    view.action = spawn(ui.ctx(), "run-resume", move || {
                        client
                            .resume_run(&id)
                            .map(|()| format!("Run {id} goes on from where it stopped"))
                    });
                }
            }
            if finished && !run.ended() {
                // The watcher gave up; let the user look again.
                if ui.button("Refresh").clicked() {
                    view.start(ui.ctx());
                }
            }
        });
        if !run.task.is_empty() {
            ui.label(egui::RichText::new(&run.task).italics());
        }
        if !run.tools.is_empty() {
            let mut grant = format!("Tools: {}", run.tools.join(", "));
            if let Some(dir) = &run.workdir {
                grant.push_str(&format!(" · in {dir}"));
            }
            ui.weak(grant);
        }
        if let (Some(waiting), "waiting") = (&run.waiting, run.status.as_str()) {
            consent_card(ui, view, waiting);
        }
        if let Some(why) = &error {
            ui.colored_label(ui.visuals().warn_fg_color, why);
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(!run.ended())
            .show(ui, |ui| {
                let lines: Vec<String> = events.iter().filter_map(event_line).collect();
                egui::CollapsingHeader::new(format!("Events ({})", lines.len()))
                    .id_salt(("run-events", &run.run_id))
                    .default_open(!run.ended())
                    .show(ui, |ui| {
                        for line in &lines {
                            ui.monospace(line);
                        }
                    });
                if run.succeeded() {
                    let fresh = view
                        .shown
                        .as_ref()
                        .is_some_and(|(status, _)| *status == run.status);
                    if !fresh {
                        let text = run.result.as_deref().unwrap_or_default();
                        let text = if text.trim().is_empty() {
                            "*(empty result)*"
                        } else {
                            text
                        };
                        let meta = [run.provider.as_deref(), run.model.as_deref()]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" · ");
                        let reply = Reply::markdown(text, run.sources.clone(), meta);
                        view.shown = Some((run.status.clone(), reply));
                    }
                    if let Some((_, reply)) = &view.shown {
                        show_reply(ui, markdown, 0, reply, false);
                    }
                } else if run.ended() {
                    let why = run
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("Run {}", run.status));
                    ui.colored_label(ui.visuals().error_fg_color, why);
                }
            });
    }

    /// The runs opened, to switch between.
    fn picker(&mut self, ui: &mut egui::Ui) {
        if self.views.len() < 2 {
            return;
        }
        let label = |v: &View| {
            let status = v
                .watch
                .lock()
                .ok()
                .and_then(|w| w.run.as_ref().map(|r| r.status.clone()))
                .unwrap_or_default();
            format!("{} {status}", v.id)
        };
        let shown = label(&self.views[self.current]);
        egui::ComboBox::from_id_salt("run-picker")
            .selected_text(shown)
            .show_ui(ui, |ui| {
                for i in (0..self.views.len()).rev() {
                    let text = label(&self.views[i]);
                    ui.selectable_value(&mut self.current, i, text);
                }
            });
    }

    /// Finished Cancel and Collect calls, and auto-collects, become notes
    /// for the transcript; a look at the run follows, so the pane catches
    /// up.
    fn collect(&mut self, ctx: &egui::Context) {
        for view in &mut self.views {
            let done = view
                .action
                .as_ref()
                .and_then(|slot| slot.lock().ok()?.take());
            if let Some(outcome) = done {
                view.action = None;
                self.notes
                    .push(outcome.unwrap_or_else(|why| format!("Run {}: {why}", view.id)));
                let finished = view.watch.lock().is_ok_and(|w| w.finished);
                if finished {
                    view.start(ctx);
                }
            }
            if let Some(outcome) = view.watch.lock().ok().and_then(|mut w| w.collected.take()) {
                self.notes
                    .push(outcome.unwrap_or_else(|why| format!("Run {}: {why}", view.id)));
            }
        }
    }
}

/// A run parked for an answer, as the web app's consent card: what it asks,
/// and Approve and Deny. Another kind of park is answered from chat.
fn consent_card(ui: &mut egui::Ui, view: &mut View, waiting: &Waiting) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        let prompt = if waiting.prompt.is_empty() {
            "The run asks to go on."
        } else {
            &waiting.prompt
        };
        ui.label(prompt);
        if waiting.kind != "consent" {
            ui.weak(format!(
                "Answer with /task respond {} \"<text>\" in chat.",
                view.id
            ));
            return;
        }
        let busy = view.action.is_some();
        ui.horizontal(|ui| {
            for (label, approved) in [("Approve", true), ("Deny", false)] {
                if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                    let (client, id) = (view.client.clone(), view.id.clone());
                    let token = waiting.token.clone();
                    view.action = spawn(ui.ctx(), "run-respond", move || {
                        client
                            .respond_run(&id, &token, Some(approved), None)
                            .map(|()| match approved {
                                true => format!("Approved; run {id} goes on"),
                                false => format!("Denied; run {id} goes on without it"),
                            })
                    });
                }
            }
        });
    });
}

/// Collect run `id`'s result: ack a held result, then merge it into the
/// slot's conversation. A refused ack is fine, as in the web app: the
/// merge decides. A result merged before counts as collected.
pub fn collect_run(client: &Client, id: &str) -> Result<(), String> {
    if client.collect_mode()? == "no" {
        return Err("Collect is off: execution.collect is \"no\" in ppxai-config.json".into());
    }
    if let Err(why) = client.ack_run(id) {
        tracing::debug!("run {id}: {why}");
    }
    match client.merge(id) {
        Ok(()) | Err(MergeError::AlreadyMerged { .. }) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Look at run `id` until it ends, `stop` is set, or the engine stops
/// answering.
fn watch_run(
    client: &Client,
    id: &str,
    watch: &Mutex<Watch>,
    stop: &AtomicBool,
    ctx: &egui::Context,
) {
    let update = |f: &mut dyn FnMut(&mut Watch)| {
        if let Ok(mut w) = watch.lock() {
            f(&mut w);
        }
        ctx.request_repaint();
    };
    let mode = client.collect_mode().unwrap_or_else(|_| "yes".into());
    update(&mut |w| w.collect = Some(mode.clone()));
    let mut failures = 0;
    let mut wait = POLL;
    while !stop.load(Ordering::Relaxed) {
        let since = watch.lock().map(|w| w.since).unwrap_or_default();
        // The events after the run, so they include all it has done by then.
        let looked = client
            .run(id)
            .and_then(|run| Ok((run, client.run_events(id, since)?)));
        match looked {
            Ok((run, events)) => {
                failures = 0;
                wait = POLL;
                let ended = run.ended();
                let auto = ended && mode == "auto" && run.succeeded() && run.merged_at.is_none();
                update(&mut |w| {
                    if let Some(last) = events.last() {
                        w.since = last.seq;
                    }
                    w.events
                        .extend(events.iter().filter(|e| e.kind != "agent_beat").cloned());
                    let over = w.events.len().saturating_sub(MAX_EVENTS);
                    w.events.drain(..over);
                    w.run = Some(run.clone());
                    w.error = None;
                });
                if ended {
                    let collected = auto.then(|| {
                        collect_run(client, id)
                            .map(|()| format!("Run {id}'s result is now in the conversation"))
                    });
                    update(&mut |w| {
                        w.collected = collected.clone();
                        w.finished = true;
                    });
                    return;
                }
            }
            Err(why) => {
                failures += 1;
                if failures >= MAX_FAILURES {
                    update(&mut |w| {
                        w.error = Some(format!("Stopped watching run {id}: {why}"));
                        w.finished = true;
                    });
                    return;
                }
                update(&mut |w| w.error = Some(why.clone()));
                wait = (wait * 2).min(MAX_BACKOFF);
            }
        }
        // Sleep in steps, so a stop is seen soon.
        let mut slept = Duration::ZERO;
        while slept < wait && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            slept += Duration::from_millis(100);
        }
    }
    update(&mut |w| w.finished = true);
}

/// One event as a line in the log, or `None` for one not worth showing,
/// after the web app's `TaskRunView._eventText`.
fn event_line(event: &RunEvent) -> Option<String> {
    let d = &event.data;
    let s = |key: &str| d[key].as_str().map(str::to_string);
    let detail = match event.kind.as_str() {
        "agent_beat" => return None,
        "tool_call" | "tool_denied" => s("tool").or_else(|| s("name")),
        "tool_result" => s("tool").map(|tool| {
            let result = d["result"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| d["result"].to_string());
            format!("{tool}: {}", one_line(&result, 80))
        }),
        "network_policy_allowed" | "network_policy_denied" => Some(format!(
            "{}{}",
            s("target_host").unwrap_or_default(),
            s("target_path").unwrap_or_default()
        )),
        "path_denied" => Some(format!(
            "{} {}",
            s("mode").unwrap_or_default(),
            s("target_path").unwrap_or_default()
        )),
        "agent_waiting" => s("prompt").or_else(|| s("kind")),
        "agent_result_ready" => d["chars"].as_u64().map(|n| format!("{n} chars")),
        "agent_run_sources" => d["sources"]
            .as_array()
            .map(|list| format!("{} sources", list.len())),
        "subagent_spawned" => s("child_run_id"),
        "subagent_finished" => s("status"),
        _ if d.is_null() || d.as_object().is_some_and(|o| o.is_empty()) => None,
        _ => Some(one_line(&d.to_string(), 100)),
    };
    Some(match detail {
        Some(detail) if !detail.is_empty() => format!("{}  {detail}", event.kind),
        _ => event.kind.clone(),
    })
}

/// `text` on one line, cut to `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let cut: String = flat.chars().take(max).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(kind: &str, data: serde_json::Value) -> RunEvent {
        RunEvent {
            seq: 1,
            kind: kind.into(),
            data,
        }
    }

    #[test]
    fn events_read_as_the_web_app_shows_them() {
        assert_eq!(event_line(&event("agent_beat", json!({"n": 3}))), None);
        assert_eq!(
            event_line(&event("tool_call", json!({"tool": "web_search"}))).as_deref(),
            Some("tool_call  web_search")
        );
        assert_eq!(
            event_line(&event(
                "tool_result",
                json!({"tool": "read", "result": "a\nb"})
            ))
            .as_deref(),
            Some("tool_result  read: a b")
        );
        assert_eq!(
            event_line(&event("agent_run_sources", json!({"sources": [{}, {}]}))).as_deref(),
            Some("agent_run_sources  2 sources")
        );
        assert_eq!(
            event_line(&event("agent_run_started", json!({}))).as_deref(),
            Some("agent_run_started")
        );
    }

    #[test]
    fn long_lines_are_cut() {
        assert_eq!(one_line("abcdef", 3), "abc…");
        assert_eq!(one_line("  a \n b ", 10), "a b");
    }
}
