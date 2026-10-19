//! Ported wholesale from gray-transform. Entry points renamed: manifest/handle/main -> *_transform.
//! gray-transform — rewrite or intercept submitted user input.
//!
//! Port of pi's `input-transform` (+ the streaming gate from
//! `input-transform-streaming`). Claims `input/submit` (protocol 2.0):
//!
//! - `?quick <q>`   → "Respond briefly in 1-2 sentences: <q>"
//! - `?verbose <q>` → "Answer thoroughly with examples: <q>"
//! - `ping`         → host/say "pong", input swallowed
//! - `time`         → host/say the local time, input swallowed
//!
//! Extra rules live in `~/.gray/transform/rules.json` as
//! `[{"prefix": "...", "rewrite": "..."}]` — `{rest}` in a rewrite is the
//! text after the prefix; without `{rest}` the rest is appended. Missing or
//! malformed file = built-ins only. `/transform` lists the active rules.
//!
//! `host/say` needs the `host.say` capability. A failed say never leaks the
//! input through: `ping`/`time` still swallow.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

/// How long an `input/submit` handler waits on `host/say` before giving up.
const HOST_TTL: Duration = Duration::from_secs(10);

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    prefix: String,
    rewrite: String,
}

pub fn manifest_transform() -> Value {
    json!({
        "name": "transform",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "commands": ["/transform"],
        "hooks": ["input/submit"],
        "capabilities": ["host.say"],
    })
}

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("transform")
}

fn parse_rules(s: &str) -> Vec<Rule> {
    let Ok(v) = serde_json::from_str::<Value>(s) else {
        return vec![];
    };
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    let prefix = r.get("prefix")?.as_str()?.trim().to_string();
                    let rewrite = r.get("rewrite")?.as_str()?.to_string();
                    if prefix.is_empty() {
                        None
                    } else {
                        Some(Rule { prefix, rewrite })
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Rules reload every call so edits to rules.json apply immediately.
pub fn load_rules() -> Vec<Rule> {
    std::fs::read_to_string(state_dir().join("rules.json"))
        .map(|s| parse_rules(&s))
        .unwrap_or_default()
}

fn local_time() -> String {
    #[cfg(windows)]
    {
        // No `date` binary — ask PowerShell for the same shape.
        std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Get-Date -Format 'yyyy-MM-dd HH:mm:ss zzz'",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(no clock on this system)".into())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("date")
            .arg("+%Y-%m-%d %H:%M:%S %Z")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(no `date` on this system)".into())
    }
}

/// What `input/submit` should do with a text.
enum Act {
    /// `{`}` — leave the input alone.
    Pass,
    /// `{"text": ...}` — replace the input.
    Rewrite(String),
    /// host/say this, then `{"handled": true}` regardless of say's outcome.
    Say(String),
}

/// Apply `rewrite` to the text after `prefix` (`rest`): `{rest}` placeholder
/// wins; otherwise the rest is appended.
fn expand(rewrite: &str, rest: &str) -> String {
    if rewrite.contains("{rest}") {
        rewrite.replace("{rest}", rest)
    } else if rest.is_empty() {
        rewrite.to_string()
    } else {
        format!("{rewrite} {rest}")
    }
}

fn classify(text: &str, rules: &[Rule]) -> Act {
    let t = text.trim();
    let lower = t.to_lowercase();
    if lower == "ping" {
        return Act::Say("pong".into());
    }
    if lower == "time" {
        return Act::Say(local_time());
    }
    let builtins = [
        ("?quick", "Respond briefly in 1-2 sentences: {rest}"),
        ("?verbose", "Answer thoroughly with examples: {rest}"),
    ];
    for (prefix, tpl) in builtins {
        if t == prefix {
            return Act::Say(format!("Usage: {prefix} <question>"));
        }
        if let Some(rest) = t.strip_prefix(prefix).and_then(|s| s.strip_prefix(' ')) {
            return Act::Rewrite(expand(tpl, rest.trim()));
        }
    }
    for r in rules {
        if t == r.prefix {
            return Act::Rewrite(expand(&r.rewrite, ""));
        }
        if let Some(rest) = t.strip_prefix(&r.prefix).and_then(|s| s.strip_prefix(' ')) {
            return Act::Rewrite(expand(&r.rewrite, rest.trim()));
        }
    }
    Act::Pass
}

fn on_submit(params: &Value, rules: &[Rule], say: &mut dyn FnMut(&str)) -> Value {
    let text = params.get("text").and_then(Value::as_str).unwrap_or("");
    match classify(text, rules) {
        Act::Pass => json!({}),
        Act::Rewrite(t) => json!({ "text": t }),
        Act::Say(t) => {
            say(&t);
            json!({ "handled": true })
        }
    }
}

fn rules_listing(rules: &[Rule]) -> String {
    let mut out = String::from("built-ins: ?quick <q>, ?verbose <q>, ping, time");
    if rules.is_empty() {
        out.push_str("\nno custom rules (add them in ~/.gray/transform/rules.json)");
    } else {
        out.push_str("\ncustom rules:");
        for r in rules {
            out.push_str(&format!("\n  {} → {}", r.prefix, r.rewrite));
        }
    }
    out
}

/// `/transform …` — `argv` excludes the command name.
fn run_command(argv: &[&str], rules: &[Rule]) -> String {
    match argv.first().copied() {
        None | Some("list") => format!(
            "gray-transform {} — {}",
            env!("CARGO_PKG_VERSION"),
            rules_listing(rules)
        ),
        Some(other) => {
            format!("unknown /transform subcommand {other:?}. /transform lists the active rules.")
        }
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_transform(
    req: &Value,
    say: &mut dyn FnMut(&str),
    rules: &[Rule],
) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_transform(),
        "input/submit" => on_submit(&params, rules, say),
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_command(&argv, rules) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

/// sidecar→host request: string id, reply routed by the reader thread.
fn host_call(
    out: &Arc<Mutex<std::io::Stdout>>,
    pending: &Pending,
    counter: &Arc<Mutex<u64>>,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let id = {
        let mut n = counter.lock().expect("id counter");
        *n += 1;
        format!("q{n}")
    };
    let (tx, rx) = mpsc::channel();
    pending.lock().expect("pending").insert(id.clone(), tx);
    {
        let req = json!({ "id": id, "method": method, "params": params });
        let mut o = out.lock().expect("stdout");
        if writeln!(o, "{req}").and_then(|_| o.flush()).is_err() {
            pending.lock().expect("pending").remove(&id);
            return Err("stdout closed".into());
        }
    }
    let v = match rx.recv_timeout(HOST_TTL) {
        Ok(v) => v,
        Err(_) => {
            pending.lock().expect("pending").remove(&id);
            return Err("host timeout".into());
        }
    };
    match v.get("error") {
        Some(e) => Err(e
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("host error")
            .to_string()),
        None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
    }
}

fn main_transform() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_transform());
        return Ok(());
    }
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(Mutex::new(0u64));

    // Reader thread: `host/*` replies (string id, no method) route by id into
    // `pending`; everything else queues for the work loop.
    let (work_tx, work_rx) = mpsc::channel::<Value>();
    let reader_pending = pending.clone();
    let _reader = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("method").is_none() {
                if let Some(id) = v.get("id").and_then(Value::as_str)
                    && let Some(tx) = reader_pending.lock().expect("pending").remove(id)
                {
                    let _ = tx.send(v);
                }
                continue;
            }
            if work_tx.send(v).is_err() {
                break;
            }
        }
    });

    for req in work_rx {
        let rules = load_rules();
        let mut say = |t: &str| {
            let _ = host_call(
                &stdout,
                &pending,
                &counter,
                "host/say",
                json!({ "text": t }),
            );
        };
        let (reply, exit) = handle_transform(&req, &mut say, &rules);
        if let Some(reply) = reply {
            let mut o = stdout.lock().expect("stdout");
            let _ = writeln!(o, "{reply}");
            let _ = o.flush();
        }
        if exit {
            break;
        }
    }
    // The reader thread is a detached stdin pump; returning from main
    // ends the process with it, so there is nothing to join.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Says(Vec<String>);
    impl Says {
        fn f(&mut self) -> impl FnMut(&str) + '_ {
            |t| self.0.push(t.to_string())
        }
    }

    fn call(method: &str, params: Value, says: &mut Says) -> Value {
        let rules = vec![];
        let mut f = says.f();
        handle_transform(
            &json!({ "id": 1, "method": method, "params": params }),
            &mut f,
            &rules,
        )
        .0
        .unwrap()
    }

    #[test]
    fn manifest_claims_input_submit_at_2_0() {
        let mut s = Says(vec![]);
        let m = call("plugin/manifest", Value::Null, &mut s)["result"].clone();
        assert_eq!(m["name"], "transform");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["input/submit"]));
        assert_eq!(m["capabilities"], json!(["host.say"]));
        assert_eq!(m["commands"], json!(["/transform"]));
    }

    #[test]
    fn quick_rewrites() {
        let mut s = Says(vec![]);
        let r = call(
            "input/submit",
            json!({"text": "?quick what is rust?"}),
            &mut s,
        );
        assert_eq!(
            r["result"]["text"],
            "Respond briefly in 1-2 sentences: what is rust?"
        );
    }

    #[test]
    fn verbose_rewrites() {
        let mut s = Says(vec![]);
        let r = call(
            "input/submit",
            json!({"text": "?verbose explain lifetimes"}),
            &mut s,
        );
        assert_eq!(
            r["result"]["text"],
            "Answer thoroughly with examples: explain lifetimes"
        );
    }

    #[test]
    fn ping_says_pong_and_swallows() {
        let mut s = Says(vec![]);
        let r = call("input/submit", json!({"text": "ping"}), &mut s);
        assert_eq!(r["result"]["handled"], true);
        assert_eq!(s.0, vec!["pong".to_string()]);
    }

    #[test]
    fn time_says_and_swallows() {
        let mut s = Says(vec![]);
        let r = call("input/submit", json!({"text": "TIME"}), &mut s);
        assert_eq!(r["result"]["handled"], true);
        assert_eq!(s.0.len(), 1);
    }

    #[test]
    fn bare_prefix_says_usage() {
        let mut s = Says(vec![]);
        let r = call("input/submit", json!({"text": "?quick"}), &mut s);
        assert_eq!(r["result"]["handled"], true);
        assert!(s.0[0].contains("Usage: ?quick"));
    }

    #[test]
    fn plain_input_passes_through() {
        let mut s = Says(vec![]);
        let r = call("input/submit", json!({"text": "hello there"}), &mut s);
        assert_eq!(r["result"], json!({}));
        assert!(s.0.is_empty());
    }

    #[test]
    fn custom_rule_appends_rest() {
        let rules = vec![Rule {
            prefix: "?pep8".into(),
            rewrite: "Check this against PEP8:".into(),
        }];
        match classify("?pep8 my_func", &rules) {
            Act::Rewrite(t) => assert_eq!(t, "Check this against PEP8: my_func"),
            _ => panic!("expected rewrite"),
        }
    }

    #[test]
    fn custom_rule_rest_placeholder() {
        let rules = vec![Rule {
            prefix: "?both".into(),
            rewrite: "A: {rest} — again: {rest}".into(),
        }];
        match classify("?both hi", &rules) {
            Act::Rewrite(t) => assert_eq!(t, "A: hi — again: hi"),
            _ => panic!("expected rewrite"),
        }
    }

    #[test]
    fn rules_parse_skips_junk() {
        let rules = parse_rules(r#"[{"prefix":"?a","rewrite":"b"},{"prefix":""},{"nope":1}]"#);
        assert_eq!(rules.len(), 1);
        assert_eq!(parse_rules("not json"), Vec::<Rule>::new());
    }

    #[test]
    fn notifications_are_silent() {
        let mut s = Says(vec![]);
        let rules = vec![];
        let mut f = s.f();
        let (reply, exit) = handle_transform(
            &json!({"method": "event/notify", "params": {"type": "turn_end"}}),
            &mut f,
            &rules,
        );
        assert!(reply.is_none() && !exit);
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let mut s = Says(vec![]);
        let rules = vec![];
        let mut f = s.f();
        let (reply, exit) = handle_transform(
            &json!({ "id": 2, "method": "plugin/shutdown" }),
            &mut f,
            &rules,
        );
        assert!(reply.is_some() && exit);
    }
}
