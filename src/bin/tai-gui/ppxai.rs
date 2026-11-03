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
//! - A conversation name is opaque: `session_` and more, unique per
//!   conversation since ppxai `bac85e6e`. `GET /status`'s `session_id` is
//!   the slot, not the conversation.
//!
//! `contract_against_a_live_server` below checks all of it against a real
//! `ppxai-server --owned`.

// The run calls have no caller outside the contract test until tai-gui's
// run views exist (ADR 0011, "Reference: runs spawned from a chat session").
#![cfg_attr(not(test), allow(dead_code))]

use std::time::Duration;

use serde_json::Value;

use crate::engine::{Link, random_hex};

/// How long a one-shot answer may take. Models can be slow; ppxai's own
/// enrichment path has its own timeout and answers 504 past it.
const ONESHOT_TIMEOUT: Duration = Duration::from_secs(300);
/// How long starting a run or merging its result may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

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

    /// One prompt, no history: `POST /v1/oneshot`.
    pub fn oneshot(&self, prompt: &str) -> Result<Answer, String> {
        let body = serde_json::json!({ "prompt": prompt });
        match self.link.post_json("/v1/oneshot", &body, ONESHOT_TIMEOUT)? {
            (200, text) => Answer::parse(&text),
            (status, text) => Err(format!("{status}: {}", detail(&text))),
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
    pub fn start_run(&self, task: &str) -> Result<String, String> {
        // ppxai stamps only from a slot that exists, and never creates one
        // for this; an expired slot would leave the run unstamped.
        self.conversation()?;
        let body = serde_json::json!({ "task": task });
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

/// A 200 from `/v1/oneshot`.
#[derive(Debug, PartialEq)]
pub struct Answer {
    pub content: String,
    pub provider: String,
    pub model: String,
    pub total_tokens: Option<u64>,
    pub finish_reason: Option<String>,
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
        })
    }
}

/// A run, from `RunMetaResponse`. `session_id` and `merged_at` are pinned
/// by ppxai's fence; the rest isn't promised.
#[derive(Debug, PartialEq)]
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
}

impl Run {
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
    use std::time::{Duration, Instant};

    use super::{Answer, Client, MergeError, Run, RunFilter, detail, encode, json};

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
            }
        );
        let cut = r#"{"content": "", "finish_reason": "length", "model": "m", "provider": "p", "usage": null}"#;
        let answer = Answer::parse(cut).unwrap();
        assert_eq!(
            (answer.total_tokens, answer.finish_reason.as_deref()),
            (None, Some("length"))
        );
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

        let answer = a.oneshot("Reply with the single word: pong").unwrap();
        assert!(
            !answer.content.is_empty(),
            "an empty one-shot answer: {answer:?}"
        );

        let id = a.start_run("Reply with the single word: pong").unwrap();
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
    }
}
