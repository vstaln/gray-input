//! Ported wholesale from gray-inline-bash. Entry points renamed: manifest/handle/main -> *_inlinebash.
//! gray-inline-bash — run shell commands inline, no model turn needed.
//!
//! Claims `input/submit` (protocol 2.0): a submitted `!<cmd>` or a line starting with `$ ` runs in
//! the session cwd via `timeout 10 sh -c`, the output (capped at 4 KB) is
//! shown via `host/say`, and the input is swallowed (`{handled: true}`).
//!
//! If `host/say` fails (capability ungranted, host gone) the input is
//! rewritten to `The user ran `<cmd>` and saw: <output>` instead — a turn
//! that shows output beats a silent swallow. `/inline on|off` toggles.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

/// Cap on command output shown to the user.
const OUT_CAP: usize = 4 * 1024;
/// How long an `input/submit` handler waits on `host/say`.
const HOST_TTL: Duration = Duration::from_secs(10);

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

pub fn manifest_inlinebash() -> Value {
    json!({
        "name": "inline-bash",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "commands": ["/inline"],
        "hooks": ["input/submit"],
        "capabilities": ["host.say"],
    })
}

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("inline-bash")
}

fn enabled() -> bool {
    !state_dir().join("disabled").exists()
}

/// Pull the shell command out of a submission: `!cmd`, `!{cmd}`, `$ cmd`.
/// Returns `None` for anything else (pass through) and `Some("")` for a
/// bare `!`/`$` (pass through — nothing to run).
fn extract_command(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(rest) = t.strip_prefix('!') {
        let cmd = rest.trim();
        let inner = cmd
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .map(str::trim);
        return Some(inner.unwrap_or(cmd).to_string());
    }
    if let Some(rest) = t.strip_prefix("$ ") {
        return Some(rest.trim().to_string());
    }
    None
}

/// Truncate on a char boundary, with a marker when output was cut.
fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &s[..end])
}

/// Run `timeout 10 sh -c <cmd>` in `cwd` (falls back to plain `sh -c` when
/// `timeout` is missing). Returns combined stdout+stderr and the exit note.
fn run_command(cmd: &str, cwd: Option<&str>) -> String {
    let spawn = |with_timeout: bool| {
        let mut c = if with_timeout {
            let mut c = std::process::Command::new("timeout");
            c.args(["10", "sh", "-c", cmd]);
            c
        } else {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", cmd]);
            c
        };
        if let Some(d) = cwd.filter(|d| Path::new(d).is_dir()) {
            c.current_dir(d);
        }
        c.output()
    };
    let out = spawn(true).or_else(|_| spawn(false));
    match out {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(err.trim_end());
            }
            let text = text.trim_end();
            let mut shown = if text.is_empty() {
                String::from("(no output)")
            } else {
                cap(text, OUT_CAP)
            };
            if let Some(code) = o.status.code().filter(|c| *c != 0) {
                shown.push_str(&format!("\n(exit {code})"));
            } else if o.status.code().is_none() {
                shown.push_str("\n(killed by signal)");
            }
            shown
        }
        Err(e) => format!("couldn't run `{cmd}`: {e}"),
    }
}

fn on_submit(params: &Value, say: &mut dyn FnMut(&str) -> Result<(), String>) -> Value {
    if !enabled() {
        return json!({});
    }
    let text = params.get("text").and_then(Value::as_str).unwrap_or("");
    let Some(cmd) = extract_command(text) else {
        return json!({});
    };
    if cmd.is_empty() {
        return json!({});
    }
    let cwd = params
        .get("session")
        .and_then(|s| s.get("cwd"))
        .and_then(Value::as_str);
    let output = run_command(&cmd, cwd);
    let msg = format!("$ {cmd}\n{output}");
    match say(&cap(&msg, OUT_CAP)) {
        Ok(()) => json!({ "handled": true }),
        Err(_) => json!({
            "text": format!("The user ran `{cmd}` and saw:\n```\n{output}\n```")
        }),
    }
}

/// `/inline …` — `argv` excludes the command name.
fn run_slash(argv: &[&str]) -> String {
    match argv.first().copied() {
        Some("on") | Some("off") => {
            let dir = state_dir();
            let flag = dir.join("disabled");
            let res = if argv[0] == "off" {
                std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&flag, b""))
            } else {
                std::fs::remove_file(&flag).or_else(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }
                })
            };
            match res {
                Ok(()) => format!("inline bash {}", if argv[0] == "off" { "off" } else { "on" }),
                Err(e) => format!("couldn't flip state: {e}"),
            }
        }
        _ => format!(
            "gray-inline-bash {} — {} · !<cmd> or `$ <cmd>` runs in the session cwd \
             (10s timeout, 4KB cap). /inline on|off",
            env!("CARGO_PKG_VERSION"),
            if enabled() { "enabled" } else { "disabled" },
        ),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_inlinebash(
    req: &Value,
    say: &mut dyn FnMut(&str) -> Result<(), String>,
) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_inlinebash(),
        "input/submit" => on_submit(&params, say),
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_slash(&argv) })
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

fn main_inlinebash() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_inlinebash());
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
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
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
        let mut say = |t: &str| {
            host_call(&stdout, &pending, &counter, "host/say", json!({ "text": t }))
                .map(|_| ())
        };
        let (reply, exit) = handle_inlinebash(&req, &mut say);
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

    fn ok_say(said: &mut Vec<String>) -> impl FnMut(&str) -> Result<(), String> + '_ {
        |t| {
            said.push(t.to_string());
            Ok(())
        }
    }

    fn failing_say(_: &str) -> Result<(), String> {
        Err("ungranted".into())
    }

    fn call(method: &str, params: Value, said: &mut Vec<String>) -> Value {
        let mut f = ok_say(said);
        handle_inlinebash(&json!({ "id": 1, "method": method, "params": params }), &mut f)
            .0
            .unwrap()
    }

    #[test]
    fn manifest_claims_input_submit_at_2_0() {
        let mut s = vec![];
        let m = call("plugin/manifest", Value::Null, &mut s)["result"].clone();
        assert_eq!(m["name"], "inline-bash");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["input/submit"]));
        assert_eq!(m["capabilities"], json!(["host.say"]));
    }

    #[test]
    fn extract_bang_dollar_and_brace() {
        assert_eq!(extract_command("!ls -la").as_deref(), Some("ls -la"));
        assert_eq!(extract_command("!{pwd}").as_deref(), Some("pwd"));
        assert_eq!(extract_command("$ git status").as_deref(), Some("git status"));
        assert_eq!(extract_command("  !echo hi  ").as_deref(), Some("echo hi"));
        assert_eq!(extract_command("hello !world"), None);
        assert_eq!(extract_command("what's $HOME"), None);
        assert_eq!(extract_command("!").as_deref(), Some(""));
        assert_eq!(extract_command(""), None);
    }

    #[test]
    fn bang_runs_says_and_swallows() {
        let mut s = vec![];
        let r = call(
            "input/submit",
            json!({"text": "!echo hi-from-test", "session": {"cwd": "/tmp"}}),
            &mut s,
        );
        assert_eq!(r["result"]["handled"], true);
        assert_eq!(s.len(), 1);
        assert!(s[0].contains("hi-from-test"));
        assert!(s[0].starts_with("$ echo hi-from-test"));
    }

    #[test]
    fn dollar_runs_in_session_cwd() {
        let mut s = vec![];
        let r = call(
            "input/submit",
            json!({"text": "$ pwd", "session": {"cwd": "/tmp"}}),
            &mut s,
        );
        assert_eq!(r["result"]["handled"], true);
        assert!(s[0].contains("/tmp"));
    }

    #[test]
    fn failing_say_falls_back_to_rewrite() {
        let r = handle_inlinebash(
            &json!({
                "id": 1, "method": "input/submit",
                "params": {"text": "!echo xyz", "session": {"cwd": "/tmp"}}
            }),
            &mut failing_say,
        )
        .0
        .unwrap();
        let t = r["result"]["text"].as_str().unwrap();
        assert!(t.contains("The user ran `echo xyz` and saw:"));
        assert!(t.contains("xyz"));
    }

    #[test]
    fn non_command_passes_through() {
        let mut s = vec![];
        let r = call("input/submit", json!({"text": "just a question"}), &mut s);
        assert_eq!(r["result"], json!({}));
        assert!(s.is_empty());
    }

    #[test]
    fn exit_code_is_shown() {
        let mut s = vec![];
        let r = call(
            "input/submit",
            json!({"text": "!exit 3", "session": {"cwd": "/tmp"}}),
            &mut s,
        );
        assert_eq!(r["result"]["handled"], true);
        assert!(s[0].contains("(exit 3)"));
    }

    #[test]
    fn cap_truncates_on_boundary() {
        let long = "x".repeat(6000);
        let c = cap(&long, OUT_CAP);
        assert!(c.len() <= OUT_CAP + 20);
        assert!(c.ends_with("[truncated]"));
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle_inlinebash(
            &json!({ "id": 2, "method": "plugin/shutdown" }),
            &mut failing_say,
        );
        assert!(reply.is_some() && exit);
    }
}
