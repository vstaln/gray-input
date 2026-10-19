//! Ported wholesale from gray-commands. Entry points renamed: manifest/handle/main -> *_commands.
//! gray-commands — prompt-template slash commands.
//!
//! Markdown files under
//! `~/.gray/commands/*.md` become slash commands. `/cmd <name> [args…]`
//! reads the file, substitutes `$ARGUMENTS` (all args) and `$1`…`$n`
//! (positional), and replies `{prompt}` so the harness runs the expanded
//! text as a real user turn. Bare `/cmd` asks interactively via `host/ask`
//! (capability `host.ask`) which command to run — the picked template is
//! expanded with empty `$ARGUMENTS`; without an ask channel it falls back
//! to the text list.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

/// Internal cap on `host/ask` waits — under the host's 330s outer TTL.
const ASK_TTL: Duration = Duration::from_secs(300);
const PICK_QID: &str = "cmd-pick";
const LABEL_CANCEL: &str = "cancel";

pub fn manifest_commands() -> Value {
    json!({
        "name": "commands",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [],
        "commands": ["/cmd"],
        "capabilities": ["host.ask"],
    })
}

pub fn commands_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("commands")
}

/// Command names come from the user and land in a path — keep them tight.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Sorted (name, first line) for every `*.md` in `dir`.
fn list_commands(dir: &Path) -> Vec<(String, String)> {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "md") {
            continue;
        }
        let Some(name) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let first = std::fs::read_to_string(&p)
            .ok()
            .and_then(|c| c.lines().next().map(str::to_string))
            .unwrap_or_default();
        out.push((name, first));
    }
    out.sort();
    out
}

/// `$ARGUMENTS` → all args joined by a space; `$1`…`$n` → positional arg
/// (missing → empty). A `$` followed by anything else passes through.
fn expand(body: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        if let Some(t) = tail.strip_prefix("ARGUMENTS") {
            out.push_str(&args.join(" "));
            rest = t;
            continue;
        }
        let digits: usize = tail.chars().take_while(|c| c.is_ascii_digit()).count();
        let n: usize = tail[..digits].parse().unwrap_or(0);
        if digits > 0 {
            if n >= 1 {
                out.push_str(args.get(n - 1).copied().unwrap_or(""));
            }
            rest = &tail[digits..];
        } else {
            out.push('$');
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn render_list(dir: &Path) -> String {
    let cmds = list_commands(dir);
    if cmds.is_empty() {
        return format!("no commands yet — drop .md files in {}", dir.display());
    }
    cmds.iter()
        .map(|(n, first)| format!("{n} — {first}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Expand `<dir>/<name>.md` with `args` into a `{prompt}` result.
fn expand_named(dir: &Path, name: &str, args: &[&str]) -> Value {
    let path = dir.join(format!("{name}.md"));
    match std::fs::read_to_string(&path) {
        Ok(body) => json!({ "prompt": expand(&body, args) }),
        Err(_) => json!({ "text": format!("no command '{name}'\n{}", render_list(dir)) }),
    }
}

/// First picked option label for question `qid` in a host/ask result.
fn picked_label<'a>(result: &'a Value, qid: &str) -> Option<&'a str> {
    result
        .get("answers")?
        .get(qid)?
        .get("answers")?
        .as_array()?
        .first()?
        .as_str()
}

/// Bare `/cmd` — ask which command via `host/ask`, expand the pick with
/// empty `$ARGUMENTS`. No ask channel → the plain text list.
fn pick_command(dir: &Path, ask: &mut dyn FnMut(&[Value]) -> Option<Value>) -> Value {
    let cmds = list_commands(dir);
    if cmds.is_empty() {
        return json!({ "text": render_list(dir) });
    }
    let options: Vec<Value> = cmds
        .iter()
        .map(|(n, first)| json!({ "label": format!("/{n}"), "description": first }))
        .chain(std::iter::once(
            json!({"label": LABEL_CANCEL, "description": "don't run a command"}),
        ))
        .collect();
    let questions = [json!({
        "id": PICK_QID,
        "header": "Run command",
        "question": "Run which command?",
        "options": options,
    })];
    let picked = ask(&questions).and_then(|r| picked_label(&r, PICK_QID).map(str::to_string));
    match picked.as_deref() {
        None => json!({ "text": render_list(dir) }),
        Some(LABEL_CANCEL) => json!({ "text": "cancelled" }),
        Some(label) => expand_named(dir, label.trim_start_matches('/'), &[]),
    }
}

/// `/cmd …` → the command/run result body: `{prompt}` runs it as a user
/// turn, `{text}` just speaks back.
fn run_command(dir: &Path, argv: &[&str], ask: &mut dyn FnMut(&[Value]) -> Option<Value>) -> Value {
    let Some(raw) = argv.first() else {
        return pick_command(dir, ask);
    };
    let name = raw.trim_end_matches(".md");
    if !valid_name(name) {
        return json!({ "text": format!("bad command name: {raw}") });
    }
    expand_named(dir, name, &argv[1..])
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply. `ask` is the host/ask channel.
pub fn handle_commands(
    req: &Value,
    dir: &Path,
    ask: &mut dyn FnMut(&[Value]) -> Option<Value>,
) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    if method.is_empty() {
        return (None, false);
    }
    let result = match method {
        "plugin/manifest" => manifest_commands(),
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            run_command(dir, &argv, ask)
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

fn next_id(counter: &Mutex<u64>) -> String {
    let mut n = counter.lock().expect("id counter");
    *n += 1;
    format!("q{n}")
}

/// `host/ask` round-trip: write the request, wait on the pending channel.
/// None on timeout or a dead channel — callers degrade to the text list.
fn ask_host(
    out: &Mutex<std::io::Stdout>,
    pending: &Pending,
    counter: &Mutex<u64>,
    questions: &[Value],
) -> Option<Value> {
    let id = next_id(counter);
    let (tx, rx) = mpsc::channel();
    pending.lock().expect("pending").insert(id.clone(), tx);
    let req = json!({
        "id": id,
        "method": "host/ask",
        "params": { "questions": questions, "blocking": true },
    });
    {
        let mut o = out.lock().expect("stdout");
        let _ = writeln!(o, "{req}");
        let _ = o.flush();
    }
    let reply = rx.recv_timeout(ASK_TTL).ok();
    pending.lock().expect("pending").remove(&id);
    reply
}

fn main_commands() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_commands());
        return Ok(());
    }
    let dir = commands_dir();
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Mutex::new(0u64);

    // Reader thread: host answers (string ids, no method) go to `pending`;
    // everything else — requests and notifications — goes to the work loop.
    let (work_tx, work_rx) = mpsc::channel::<Value>();
    let reader_pending = pending.clone();
    let _reader = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(id) = v.get("id").and_then(Value::as_str)
                && v.get("method").is_none()
                && let Some(tx) = reader_pending.lock().expect("pending").remove(id)
            {
                let _ = tx.send(v.get("result").cloned().unwrap_or(Value::Null));
                continue;
            }
            if work_tx.send(v).is_err() {
                break;
            }
        }
    });

    for req in work_rx {
        let mut asker = |questions: &[Value]| ask_host(&stdout, &pending, &counter, questions);
        let (reply, exit) = handle_commands(&req, &dir, &mut asker);
        if let Some(reply) = reply {
            let mut o = stdout.lock().expect("stdout");
            writeln!(o, "{reply}")?;
            o.flush()?;
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

    fn no_ask() -> impl FnMut(&[Value]) -> Option<Value> {
        |_: &[Value]| None
    }

    fn call(dir: &Path, argv: Value) -> Value {
        call_with(dir, argv, &mut no_ask())
    }

    fn call_with(dir: &Path, argv: Value, ask: &mut dyn FnMut(&[Value]) -> Option<Value>) -> Value {
        handle_commands(
            &json!({ "id": 1, "method": "command/run",
                     "params": {"name": "/cmd", "argv": argv} }),
            dir,
            ask,
        )
        .0
        .unwrap()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gray-cmd-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn manifest_has_one_command_no_tools() {
        let m = manifest_commands();
        assert_eq!(m["commands"], json!(["/cmd"]));
        assert_eq!(m["tools"], json!([]));
        assert_eq!(m["capabilities"], json!(["host.ask"]));
    }

    #[test]
    fn expand_substitutes_arguments_and_positionals() {
        let args = vec!["fix", "the bug"];
        assert_eq!(
            expand("do $1 then $2; all: $ARGUMENTS; gone $3; keep $x $$", &args),
            "do fix then the bug; all: fix the bug; gone ; keep $x $$"
        );
    }

    #[test]
    fn bare_cmd_lists_commands() {
        let dir = tmpdir("list");
        std::fs::write(dir.join("review.md"), "Review the diff\nbody line").unwrap();
        std::fs::write(dir.join("ship.md"), "Ship it").unwrap();
        let r = call(&dir, json!([]));
        let text = r["result"]["text"].as_str().unwrap();
        assert!(text.contains("review — Review the diff"));
        assert!(text.contains("ship — Ship it"));
    }

    #[test]
    fn bare_cmd_picker_runs_picked_template() {
        let dir = tmpdir("pick");
        std::fs::write(dir.join("review.md"), "Review the diff of $1\nbody").unwrap();
        std::fs::write(dir.join("ship.md"), "Ship it").unwrap();
        let mut ask = |qs: &[Value]| -> Option<Value> {
            // The ask carries every command as an option plus cancel.
            let opts = qs[0]["options"].as_array().unwrap();
            assert_eq!(opts.len(), 3);
            assert_eq!(opts[0]["label"], "/review");
            assert_eq!(opts[0]["description"], "Review the diff of $1");
            assert_eq!(opts[2]["label"], "cancel");
            Some(json!({"answers": {PICK_QID: {"answers": ["/ship"]}}}))
        };
        let r = call_with(&dir, json!([]), &mut ask);
        assert_eq!(r["result"]["prompt"], "Ship it");
    }

    #[test]
    fn bare_cmd_picker_cancel_and_no_channel() {
        let dir = tmpdir("pick-cancel");
        std::fs::write(dir.join("a.md"), "Do A").unwrap();
        let mut cancel = |_: &[Value]| -> Option<Value> {
            Some(json!({"answers": {PICK_QID: {"answers": ["cancel"]}}}))
        };
        assert_eq!(
            call_with(&dir, json!([]), &mut cancel)["result"]["text"],
            "cancelled"
        );
        // No ask channel → falls back to the text list.
        let r = call(&dir, json!([]));
        assert!(r["result"]["text"].as_str().unwrap().contains("a — Do A"));
    }

    #[test]
    fn bare_cmd_empty_dir_skips_picker() {
        let dir = tmpdir("pick-empty");
        let mut ask = |_: &[Value]| -> Option<Value> { panic!("no commands → no host/ask") };
        let r = call_with(&dir, json!([]), &mut ask);
        assert!(
            r["result"]["text"]
                .as_str()
                .unwrap()
                .contains("no commands yet")
        );
    }

    #[test]
    fn known_command_returns_prompt() {
        let dir = tmpdir("prompt");
        std::fs::write(dir.join("hi.md"), "say hi to $1 ($ARGUMENTS)").unwrap();
        let r = call(&dir, json!(["hi", "bob", "x"]));
        assert_eq!(r["result"]["prompt"], "say hi to bob (bob x)");
    }

    #[test]
    fn missing_command_lists_what_exists() {
        let dir = tmpdir("missing");
        std::fs::write(dir.join("only.md"), "The one").unwrap();
        let r = call(&dir, json!(["nope"]));
        let text = r["result"]["text"].as_str().unwrap();
        assert!(text.contains("no command 'nope'"));
        assert!(text.contains("only — The one"));
        assert!(r["result"].get("prompt").is_none());
    }

    #[test]
    fn bad_name_rejected() {
        let dir = tmpdir("badname");
        let r = call(&dir, json!(["../etc/passwd"]));
        assert!(
            r["result"]["text"]
                .as_str()
                .unwrap()
                .contains("bad command name")
        );
    }
}
