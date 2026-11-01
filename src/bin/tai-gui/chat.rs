//! The chat pane, ADR 0011 step 4: a prompt sent to `/v1/oneshot` through
//! the engine link, the answer rendered as Markdown.
//!
//! One-shot only: each prompt stands alone, with no session or history on
//! the server, and nothing streams. `/chat` with SSE, interrupt and consent
//! comes after the prototype. Keys follow the mainstream harnesses (Claude
//! Code, Codex, pi): Enter sends, Shift+Enter starts a new line.
//!
//! egui's bundled fonts have no CJK. Text they can't draw gets the font the
//! OS picks for it, added to egui as a fallback (ADR 0012, decision 6).

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
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
};

/// How long a one-shot answer may take. Models can be slow; ppxai's own
/// enrichment path has its own timeout and answers 504 past it.
const ONESHOT_TIMEOUT: Duration = Duration::from_secs(300);

enum Entry {
    Prompt(String),
    Answer { text: String, meta: String },
    Error(String),
}

pub struct ChatPane {
    entries: Vec<Entry>,
    input: String,
    /// The answer to the prompt in flight, filled by its thread.
    pending: Option<(Instant, Arc<Mutex<Option<Entry>>>)>,
    markdown: CommonMarkCache,
    fonts: ChatFonts,
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
            entries: Vec::new(),
            input: String::new(),
            pending: None,
            markdown: CommonMarkCache::default(),
            fonts: ChatFonts::default(),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, engine: &Engine) {
        self.collect_answer(ui.ctx());
        let link = engine.state.lock().ok().and_then(|state| match &*state {
            State::Ready(link) => Some(link.clone()),
            _ => None,
        });

        egui::Panel::bottom("chat-input")
            .resizable(false)
            .show(ui, |ui| {
                ui.add_space(4.0);
                self.status_line(ui, engine);
                let input_id = egui::Id::new("chat-input-text");
                // Enter sends; Shift+Enter falls through to the text box as
                // a new line. Taken before the text box sees the key.
                let send = ui.memory(|m| m.has_focus(input_id))
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                let ready = link.is_some() && self.pending.is_none();
                let typed = ui.add(
                    egui::TextEdit::multiline(&mut self.input)
                        .id(input_id)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text(if ready {
                            "Ask… (Enter sends, Shift+Enter for a new line)"
                        } else {
                            "Waiting for the engine…"
                        }),
                );
                if typed.changed() {
                    self.fonts.cover(ui.ctx(), &self.input);
                }
                if send
                    && ready
                    && !self.input.trim().is_empty()
                    && let Some(link) = link.clone()
                {
                    let prompt = std::mem::take(&mut self.input).trim().to_string();
                    self.send(prompt, link, ui.ctx().clone());
                }
                ui.add_space(4.0);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.entries.is_empty() {
                        ui.weak("One-shot chat: each prompt stands alone, with no history.");
                    }
                    for entry in &self.entries {
                        match entry {
                            Entry::Prompt(text) => {
                                ui.add_space(8.0);
                                ui.label(egui::RichText::new(text).strong());
                            }
                            Entry::Answer { text, meta } => {
                                ui.add_space(4.0);
                                CommonMarkViewer::new().show(ui, &mut self.markdown, text);
                                ui.weak(meta);
                            }
                            Entry::Error(why) => {
                                ui.add_space(4.0);
                                ui.colored_label(ui.visuals().error_fg_color, why);
                            }
                        }
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

    fn send(&mut self, prompt: String, link: crate::engine::Link, ctx: egui::Context) {
        self.entries.push(Entry::Prompt(prompt.clone()));
        let slot = Arc::new(Mutex::new(None));
        self.pending = Some((Instant::now(), Arc::clone(&slot)));
        let spawned = std::thread::Builder::new()
            .name("oneshot".into())
            .spawn(move || {
                let body = serde_json::json!({ "prompt": prompt });
                let entry = match link.post_json("/v1/oneshot", &body, ONESHOT_TIMEOUT) {
                    Ok((200, text)) => answer(&text),
                    Ok((status, text)) => Entry::Error(format!("{status}: {}", detail(&text))),
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
                Entry::Answer { text, .. } | Entry::Prompt(text) | Entry::Error(text) => {
                    self.fonts.cover(ctx, text);
                }
            }
            self.entries.push(entry);
        }
    }
}

/// A 200 from /v1/oneshot: the content, and a line naming the model and
/// the tokens used.
fn answer(body: &str) -> Entry {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Entry::Error(format!("not JSON: {body}"));
    };
    let text = v["content"].as_str().unwrap_or_default().to_string();
    let mut meta = format!(
        "{} · {}",
        v["provider"].as_str().unwrap_or("?"),
        v["model"].as_str().unwrap_or("?")
    );
    if let Some(total) = v["usage"]["total_tokens"].as_u64() {
        meta.push_str(&format!(" · {total} tokens"));
    }
    if let Some(reason) = v["finish_reason"].as_str().filter(|r| *r != "stop") {
        meta.push_str(&format!(" · finished: {reason}"));
    }
    Entry::Answer { text, meta }
}

/// Whether a link is http or https, the only schemes the chat opens.
fn is_web_link(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

/// FastAPI's `{"detail": ...}`, or the body as it is.
fn detail(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| match &v["detail"] {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Null => None,
            other => Some(other.to_string()),
        })
        .unwrap_or_else(|| body.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Entry, answer, detail, is_web_link};

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
    fn an_answer_names_provider_model_and_tokens() {
        let body = r#"{"content": "**hi**", "finish_reason": "stop", "model": "m1",
            "provider": "p1", "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}}"#;
        let Entry::Answer { text, meta } = answer(body) else {
            panic!("not an answer");
        };
        assert_eq!(text, "**hi**");
        assert_eq!(meta, "p1 · m1 · 3 tokens");
    }

    #[test]
    fn a_cut_off_answer_says_why() {
        let body = r#"{"content": "", "finish_reason": "length", "model": "m", "provider": "p", "usage": null}"#;
        let Entry::Answer { meta, .. } = answer(body) else {
            panic!("not an answer");
        };
        assert_eq!(meta, "p · m · finished: length");
    }

    #[test]
    fn errors_show_fastapis_detail() {
        assert_eq!(
            detail(r#"{"detail": "Provider call failed: boom"}"#),
            "Provider call failed: boom"
        );
        assert_eq!(detail("plain text"), "plain text");
    }
}
