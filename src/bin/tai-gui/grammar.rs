//! The grammar of `/task` and `/run`: whether a line is a lifecycle verb or
//! a launch, and a `/task` launch's flags.
//!
//! Ported from ppxai `ppxai/engine/task_grammar.py` (commit "style:
//! mechanical ruff bulk — UP006/UP045/I001/F401/UP035/F541 (Item 67)",
//! 4f13cc12), itself a port of the web app's
//! `ppxai/web/shared/task-controller.js`, which stays the reference (ppxai
//! pins the two together in `tests/test_task_grammar_parity.py`).

use serde_json::{Map, Value, json};

/// The lifecycle verbs, aliases included.
const VERBS: &[&str] = &[
    "help", "ls", "list", "get", "show", "open", "watch", "cancel", "respond", "collect", "ack",
    "resume",
];

/// What a command line asks for.
#[derive(Debug, PartialEq)]
pub enum Dispatch {
    /// An empty line.
    Help,
    /// A verb, by its canonical name, with the run id after it, if any.
    Lifecycle {
        verb: &'static str,
        id: Option<String>,
        /// The line after the verb, for `respond`'s answer.
        rest: String,
    },
    /// A verb followed by something that looks like a run id but isn't one:
    /// refused rather than launched as a prompt.
    NearMiss { id: String },
    /// Anything else: the whole line is the prompt, with its flags.
    Launch(String),
}

/// A registry run id: `run_` and 12 hex digits (ppxai
/// `engine/agent_runs.py`).
pub fn is_run_id(text: &str) -> bool {
    text.strip_prefix("run_").is_some_and(|hex| {
        hex.len() == 12 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// A line is a lifecycle op only when its first word is a verb and the rest
/// is empty or starts with a run id. So `get run_ab12…` gets a run, and
/// `get the weather --tools web_search` launches. Run ids hold no spaces, so
/// only the first word after the verb counts as the id.
pub fn classify(line: &str) -> Dispatch {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Dispatch::Help;
    }
    let (first, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((first, rest)) => (first, rest.trim()),
        None => (trimmed, ""),
    };
    let first_word = rest.split_whitespace().next().unwrap_or_default();
    let verb = first.to_lowercase();
    let Some(verb) = VERBS.iter().find(|v| **v == verb) else {
        return Dispatch::Launch(trimmed.to_string());
    };
    if rest.is_empty() || is_run_id(first_word) {
        let id = (!first_word.is_empty()).then(|| first_word.to_string());
        return Dispatch::Lifecycle {
            verb: canonical(verb),
            id,
            rest: rest.to_string(),
        };
    }
    if first_word.starts_with("run_") {
        return Dispatch::NearMiss {
            id: first_word.to_string(),
        };
    }
    Dispatch::Launch(trimmed.to_string())
}

/// A verb's canonical name: the web app's aliases (`list`, `show`, `open`,
/// `ack`) are other names for `ls`, `get` and `collect`.
fn canonical(verb: &'static str) -> &'static str {
    match verb {
        "list" => "ls",
        "show" | "open" => "get",
        "ack" => "collect",
        other => other,
    }
}

/// A `/task` launch line, shaped as an `AgentTaskRequest`.
#[derive(Debug, Default, PartialEq)]
pub struct TaskArgs {
    pub task: String,
    pub tools: Vec<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub system: Option<String>,
    /// `network.allow_outbound`: a host, or `{host, paths}`.
    pub allow_outbound: Vec<Value>,
    pub budget: Map<String, Value>,
    pub spec: Option<String>,
    pub skills: Vec<String>,
    pub profile: Option<String>,
    pub enrichment: Option<bool>,
    pub workdir: Option<String>,
    /// Not empty means the line isn't sent.
    pub errors: Vec<String>,
}

impl TaskArgs {
    /// The prompt is the words before the first `--flag`, quoted or not.
    pub fn parse(line: &str) -> Self {
        let tokens = tokenize(line.trim());
        let mut out = Self::default();
        let mut i = 0;
        let mut words = Vec::new();
        while i < tokens.len() && !tokens[i].starts_with("--") {
            words.push(tokens[i].as_str());
            i += 1;
        }
        out.task = words.join(" ").trim().to_string();
        let list = |v: &str| -> Vec<String> {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        };
        while i < tokens.len() {
            let flag = tokens[i].clone();
            let value = if i + 1 >= tokens.len() || tokens[i + 1].starts_with("--") {
                None
            } else {
                i += 1;
                Some(tokens[i].clone())
            };
            i += 1;
            let known = matches!(
                flag.as_str(),
                "--tools"
                    | "--allow"
                    | "--provider"
                    | "--model"
                    | "--system"
                    | "--budget"
                    | "--spec"
                    | "--profile"
                    | "--enrichment"
                    | "--work-dir"
                    | "--skill"
            );
            if !known {
                out.errors.push(format!("unknown flag: {flag}"));
                // The JS parser reads only the flag and moves on.
                if value.is_some() {
                    i -= 1;
                }
                continue;
            }
            let Some(v) = value else {
                out.errors.push(format!("{flag} needs a value"));
                continue;
            };
            // An empty value (`--tools ""`) is ignored, as in the JS, except
            // by `--enrichment`, which refuses it.
            if v.is_empty() && flag != "--enrichment" {
                continue;
            }
            match flag.as_str() {
                "--tools" => out.tools = list(&v),
                "--allow" => out.allow_outbound = list(&v).iter().map(|s| egress(s)).collect(),
                "--provider" => out.provider = Some(v),
                "--model" => out.model = Some(v),
                "--system" => out.system = Some(v),
                "--budget" => budget(&v, &mut out),
                "--spec" => out.spec = Some(v),
                "--profile" => out.profile = Some(v),
                "--enrichment" => match v.as_str() {
                    "on" | "true" => out.enrichment = Some(true),
                    "off" | "false" => out.enrichment = Some(false),
                    _ => out.errors.push("--enrichment takes on|off".into()),
                },
                "--work-dir" => out.workdir = Some(v),
                "--skill" => {
                    for skill in list(&v) {
                        if !out.skills.contains(&skill) {
                            out.skills.push(skill);
                        }
                    }
                }
                _ => unreachable!("checked above"),
            }
        }
        out
    }

    /// A spec, skill or profile names the grant, so the server fills in what
    /// the line leaves out.
    pub fn has_source(&self) -> bool {
        self.spec.is_some() || !self.skills.is_empty() || self.profile.is_some()
    }

    /// The `POST /v1/agent/task` body, as the web app's `TaskController.run`
    /// builds it. `provider` and `model` are the picker's, used only when the
    /// line names none and no spec, skill or profile does either; `workdir`
    /// is the conversation's, used when the line has no `--work-dir`.
    pub fn body(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
        workdir: Option<&str>,
    ) -> Value {
        let mut body = json!({ "task": self.task, "tools": self.tools });
        let fallback = |picked: Option<&str>| {
            (!self.has_source())
                .then(|| picked.map(str::to_string))
                .flatten()
        };
        let mut set = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                body[key] = value;
            }
        };
        set("spec", self.spec.clone().map(Value::from));
        set(
            "skills",
            (!self.skills.is_empty()).then(|| self.skills.clone().into()),
        );
        set("profile", self.profile.clone().map(Value::from));
        set("enrichment", self.enrichment.map(Value::from));
        set(
            "provider",
            self.provider
                .clone()
                .or_else(|| fallback(provider))
                .map(Value::from),
        );
        set(
            "model",
            self.model
                .clone()
                .or_else(|| fallback(model))
                .map(Value::from),
        );
        set("system", self.system.clone().map(Value::from));
        set(
            "network",
            (!self.allow_outbound.is_empty())
                .then(|| json!({ "allow_outbound": self.allow_outbound })),
        );
        set(
            "budget",
            (!self.budget.is_empty()).then(|| Value::Object(self.budget.clone())),
        );
        set(
            "workdir",
            self.workdir
                .clone()
                .or_else(|| workdir.map(str::to_string))
                .map(Value::from),
        );
        body
    }
}

/// Split a line into words, a `"…"` or `'…'` counting as one, as the
/// regex `"([^"]*)"|'([^']*)'|(\S+)` does: a quote that isn't closed starts
/// a plain word.
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    loop {
        rest = rest.trim_start();
        let Some(c) = rest.chars().next() else {
            return out;
        };
        if (c == '"' || c == '\'')
            && let Some(end) = rest[1..].find(c)
        {
            out.push(rest[1..1 + end].to_string());
            rest = &rest[end + 2..];
            continue;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        out.push(rest[..end].to_string());
        rest = &rest[end..];
    }
}

/// `host` allows any path; `host/path` only that path.
fn egress(entry: &str) -> Value {
    match entry.find('/') {
        None => entry.into(),
        Some(slash) => json!({ "host": &entry[..slash], "paths": [&entry[slash..]] }),
    }
}

/// `100`, `100k`, `1.5m`.
fn number(text: &str) -> Option<f64> {
    let text = text.trim();
    let (digits, scale) = match text.chars().last()?.to_ascii_lowercase() {
        'k' => (&text[..text.len() - 1], 1e3),
        'm' => (&text[..text.len() - 1], 1e6),
        _ => (text, 1.0),
    };
    let valid = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && digits.bytes().next().is_some_and(|b| b.is_ascii_digit())
        && digits.matches('.').count() <= 1
        && !digits.ends_with('.');
    valid
        .then(|| digits.parse::<f64>().ok())
        .flatten()
        .map(|n| n * scale)
}

/// `--budget iters=20,time=300,tokens=100k`.
fn budget(text: &str, out: &mut TaskArgs) {
    for pair in text.split(',') {
        let Some((key, value)) = pair.split_once('=') else {
            out.errors.push(format!("bad --budget term: {pair}"));
            continue;
        };
        let Some(n) = number(value) else {
            out.errors.push(format!("bad --budget value: {pair}"));
            continue;
        };
        let (key, value) = match key.trim().to_lowercase().as_str() {
            "iters" | "iterations" => ("iterations", Value::from(n.round() as i64)),
            "time" | "time_s" => ("time_s", Value::from(n)),
            "tokens" => ("tokens", Value::from(n.round() as i64)),
            other => {
                out.errors.push(format!("unknown --budget key: {other}"));
                continue;
            }
        };
        out.budget.insert(key.into(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "run_0123456789ab";

    fn lifecycle(verb: &'static str, id: Option<&str>) -> Dispatch {
        Dispatch::Lifecycle {
            verb,
            id: id.map(str::to_string),
            rest: id.unwrap_or_default().to_string(),
        }
    }

    #[test]
    fn a_verb_counts_only_before_a_run_id_or_nothing() {
        assert_eq!(classify("  "), Dispatch::Help);
        assert_eq!(classify("ls"), lifecycle("ls", None));
        assert_eq!(classify("LIST"), lifecycle("ls", None));
        assert_eq!(classify(&format!("show {ID}")), lifecycle("get", Some(ID)));
        assert_eq!(
            classify(&format!("ack {ID}")),
            lifecycle("collect", Some(ID))
        );
        assert_eq!(
            classify("get the weather in Geneva --tools web_search"),
            Dispatch::Launch("get the weather in Geneva --tools web_search".into())
        );
        assert_eq!(
            classify("cancel run_0123"),
            Dispatch::NearMiss {
                id: "run_0123".into()
            }
        );
        assert_eq!(
            classify("\"ls\""),
            Dispatch::Launch("\"ls\"".into()),
            "quoting forces a launch"
        );
    }

    #[test]
    fn respond_keeps_its_answer() {
        assert_eq!(
            classify(&format!("respond {ID} \"not now\"")),
            Dispatch::Lifecycle {
                verb: "respond",
                id: Some(ID.into()),
                rest: format!("{ID} \"not now\""),
            }
        );
    }

    #[test]
    fn run_ids_are_run_and_twelve_hex_digits() {
        assert!(is_run_id(ID));
        assert!(!is_run_id("run_0123456789AB"));
        assert!(!is_run_id("run_0123456789a"));
        assert!(!is_run_id("0123456789ab"));
    }

    #[test]
    fn a_launch_reads_its_flags() {
        let args = TaskArgs::parse(
            "\"summarize README.md\" --tools read_file,web_search --allow example.com,docs.rs/serde \
             --budget iters=20,time=1.5k,tokens=100k --skill a,b --skill a --enrichment off \
             --work-dir '/path/to/project'",
        );
        assert_eq!(args.errors, Vec::<String>::new());
        assert_eq!(args.task, "summarize README.md");
        assert_eq!(args.tools, ["read_file", "web_search"]);
        assert_eq!(
            args.allow_outbound,
            [
                json!("example.com"),
                json!({"host": "docs.rs", "paths": ["/serde"]})
            ]
        );
        assert_eq!(
            Value::Object(args.budget.clone()),
            json!({"iterations": 20, "time_s": 1500.0, "tokens": 100000})
        );
        assert_eq!(args.skills, ["a", "b"]);
        assert_eq!(args.enrichment, Some(false));
        assert_eq!(args.workdir.as_deref(), Some("/path/to/project"));
    }

    #[test]
    fn quotes_group_words() {
        assert_eq!(tokenize(r#" "a b" 'c d' e"f "#), ["a b", "c d", "e\"f"]);
        assert_eq!(tokenize(r#""open x"#), ["\"open", "x"]);
        assert_eq!(tokenize(r#""" x"#), ["", "x"]);
    }

    #[test]
    fn bad_flags_are_errors() {
        let args = TaskArgs::parse("x --tools --nope y --budget iters=a,b --enrichment maybe");
        assert_eq!(
            args.errors,
            [
                "--tools needs a value",
                "unknown flag: --nope",
                "unknown flag: y",
                "bad --budget value: iters=a",
                "bad --budget term: b",
                "--enrichment takes on|off",
            ]
        );
    }

    #[test]
    fn the_body_takes_the_pickers_choice_unless_a_source_names_one() {
        let plain = TaskArgs::parse("do it --tools read_file");
        assert_eq!(
            plain.body(Some("openrouter"), Some("m"), Some("/path/to/project")),
            json!({"task": "do it", "tools": ["read_file"], "provider": "openrouter",
                   "model": "m", "workdir": "/path/to/project"})
        );
        let spec = TaskArgs::parse("do it --spec review --work-dir /elsewhere");
        assert_eq!(
            spec.body(Some("openrouter"), Some("m"), Some("/path/to/project")),
            json!({"task": "do it", "tools": [], "spec": "review", "workdir": "/elsewhere"})
        );
    }
}
