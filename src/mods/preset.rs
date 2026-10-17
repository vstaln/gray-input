//! Ported wholesale from gray-preset. Entry points renamed: manifest/handle/main -> *_preset.
//! gray-preset — named working-preference bundles.
//!
//! A preset is a markdown file of working preferences at
//! `~/.gray/presets/<name>.md`. `/preset use <name>` returns `{prompt}` so
//! the harness applies it as a real instruction turn.
//!
//! Bare `/preset` is interactive when the host grants `host.ask`: a "Use
//! which preset?" picker lists every preset plus "none"; picking one
//! returns the same `{prompt}` as `/preset use`. With no ask channel it
//! falls back to the text list.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

pub fn manifest_preset() -> Value {
    json!({
        "name": "preset",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [],
        "commands": ["/preset"],
        "capabilities": ["host.ask"],
    })
}

/// host/ask round-trip signature — the real one lives in `main` (writes the
/// request to stdout, blocks on a pending channel); tests inject canned
/// answers. Params are the full `host/ask` params object; returns the reply's
/// `result`, or None on timeout.
pub type Ask<'a> = dyn FnMut(&Value) -> Option<Value> + 'a;

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;
/// Internal cap on host/ask waits — under the host's outer TTL.
const ASK_TTL: Duration = Duration::from_secs(300);
const Q_PRESET: &str = "preset-use";
const LABEL_NONE: &str = "none";

/// First picked label out of a host/ask reply — None on absent/timeout/empty.
fn picked_label(result: Option<&Value>, qid: &str) -> Option<String> {
    result?
        .pointer(&format!("/answers/{qid}/answers/0"))?
        .as_str()
        .map(String::from)
        .filter(|s| !s.is_empty())
}

fn presets_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("presets")
}

/// Normalize a preset name to a bare file stem inside `presets/` — rejects
/// separators, dots-only names and anything that could escape the dir.
fn normalize(name: &str) -> Option<String> {
    let stem = name.trim().strip_suffix(".md").unwrap_or(name.trim());
    if stem.is_empty()
        || stem.starts_with('.')
        || stem.contains(['/', '\\'])
        || !stem.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return None;
    }
    Some(stem.to_string())
}

fn list_names() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(presets_dir()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("md") {
                p.file_stem()?.to_str().map(String::from)
            } else {
                None
            }
        })
        .collect();
    names.sort();
    names
}

/// `/preset …` → the command/run result: `{text}` to say, `{prompt}` to run
/// as a user instruction turn. `argv` excludes the command name.
/// `/preset` (or `/preset list`) text render: the name list, or the
/// "no presets yet" hint.
fn list_result() -> Value {
    let names = list_names();
    if names.is_empty() {
        json!({"text": format!(
            "No presets yet. `/preset new <name>` creates {}; fill it in, then `/preset use <name>`.",
            presets_dir().join("<name>.md").display()
        )})
    } else {
        let mut out = format!("{} preset(s) in {}:\n", names.len(), presets_dir().display());
        for n in &names {
            out.push_str(&format!("  {n}\n"));
        }
        out.push_str("use one with `/preset use <name>`");
        json!({"text": out})
    }
}

/// `/preset use <name>` → `{prompt}` carrying the file's contents, or a text
/// answer for empty/unknown presets.
fn use_preset(name: &str) -> Value {
    let path = presets_dir().join(format!("{name}.md"));
    match std::fs::read_to_string(&path) {
        Ok(contents) if !contents.trim().is_empty() => json!({
            "prompt": format!(
                "Apply these working preferences for the rest of the session:\n{}",
                contents.trim()
            )
        }),
        Ok(_) => json!({"text": format!(
            "Preset \"{name}\" is empty — fill in {} first.",
            path.display()
        )}),
        Err(_) => {
            let available = list_names();
            json!({"text": format!(
                "Unknown preset \"{name}\". Available: {}",
                if available.is_empty() { "(none)".into() } else { available.join(", ") }
            )})
        }
    }
}

/// First non-empty line of a preset file — the picker's option description.
fn preset_preview(name: &str) -> String {
    std::fs::read_to_string(presets_dir().join(format!("{name}.md")))
        .ok()
        .and_then(|c| c.lines().map(str::trim).find(|l| !l.is_empty()).map(String::from))
        .map(|l| l.chars().take(60).collect())
        .unwrap_or_else(|| "empty preset".into())
}

fn run_command(argv: &[&str], ask: &mut Ask) -> Value {
    match argv {
        [] => {
            let names = list_names();
            if names.is_empty() {
                return list_result();
            }
            let mut options: Vec<Value> = names
                .iter()
                .map(|n| json!({"label": n, "description": preset_preview(n)}))
                .collect();
            options.push(json!({
                "label": LABEL_NONE,
                "description": "don't apply a preset — just list them",
            }));
            let picked = ask(&json!({
                "questions": [{
                    "id": Q_PRESET,
                    "header": "Presets",
                    "question": "Use which preset?",
                    "options": options,
                }],
                "blocking": true,
            }));
            let label = picked_label(picked.as_ref(), Q_PRESET).unwrap_or_default();
            // A real preset named "none" still wins over the sentinel label.
            if names.iter().any(|n| *n == label) {
                use_preset(&label)
            } else {
                list_result()
            }
        }
        ["list"] => list_result(),
        ["use", rest @ ..] => {
            let Some(name) = rest.first().and_then(|n| normalize(n)) else {
                return json!({"text": "usage: /preset use <name>"});
            };
            use_preset(&name)
        }
        ["new", rest @ ..] => {
            let Some(name) = rest.first().and_then(|n| normalize(n)) else {
                return json!({"text": "usage: /preset new <name> (letters, digits, -, _, .)"});
            };
            let dir = presets_dir();
            let path = dir.join(format!("{name}.md"));
            let existed = path.exists();
            if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| {
                if existed { Ok(()) } else { std::fs::write(&path, b"") }
            }) {
                return json!({"text": format!("couldn't create preset: {e}")});
            }
            json!({"text": format!(
                "Preset \"{name}\" {} at {} — write your working preferences there, then `/preset use {name}`.",
                if existed { "already exists" } else { "created" },
                path.display()
            )})
        }
        _ => json!({"text": "usage: /preset [list] | /preset use <name> | /preset new <name>"}),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_preset(req: &Value, ask: &mut Ask) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_preset(),
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            run_command(&argv, ask)
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_preset() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_preset());
        return Ok(());
    }
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Mutex::new(0u64);

    // Reader thread: host answers (string id, no method) go to `pending`;
    // host requests (id + method) go to the work channel. Notifications have
    // no id and are dropped here, same as `handle` returning None.
    let (work_tx, work_rx) = mpsc::channel::<Value>();
    let reader_pending = pending.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if let Some(id) = v.get("id").and_then(Value::as_str)
                && v.get("method").is_none()
            {
                if let Some(tx) = reader_pending.lock().expect("pending").remove(id) {
                    let _ = tx.send(v.get("result").cloned().unwrap_or(Value::Null));
                }
                continue;
            }
            let shutdown = v.get("method").and_then(Value::as_str) == Some("plugin/shutdown");
            if (v.get("id").is_some() || shutdown) && work_tx.send(v).is_err() {
                break;
            }
        }
    });

    // host/ask: write the request, block on the pending channel. Errors and
    // timeouts surface as None → callers degrade to a text answer.
    let mut ask = |params: &Value| -> Option<Value> {
        let id = {
            let mut n = counter.lock().expect("id counter");
            *n += 1;
            format!("q{n}")
        };
        let (tx, rx) = mpsc::channel();
        pending.lock().expect("pending").insert(id.clone(), tx);
        {
            let req = json!({"id": id, "method": "host/ask", "params": params});
            let mut o = stdout.lock().expect("stdout");
            let _ = writeln!(o, "{req}");
            let _ = o.flush();
        }
        let reply = rx.recv_timeout(ASK_TTL).ok();
        pending.lock().expect("pending").remove(&id);
        reply
    };

    for req in work_rx {
        let (reply, exit) = handle_preset(&req, &mut ask);
        if let Some(reply) = reply {
            let mut o = stdout.lock().expect("stdout");
            let _ = writeln!(o, "{reply}");
            let _ = o.flush();
        }
        if exit {
            break;
        }
    }
    // The reader thread is a detached stdin pump; returning ends the process
    // with it — nothing to join.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        call_ask(method, params, &mut |_p: &Value| None)
    }

    fn call_ask(method: &str, params: Value, ask: &mut Ask) -> Value {
        handle_preset(&json!({ "id": 1, "method": method, "params": params }), ask)
            .0
            .unwrap()
    }

    /// Test asker: answers host/ask by picking option `i` of question 0.
    fn picking(i: usize) -> impl FnMut(&Value) -> Option<Value> {
        move |p: &Value| {
            let q = &p["questions"][0];
            let qid = q["id"].as_str().unwrap_or("q").to_string();
            let label = q["options"][i]["label"].as_str()?.to_string();
            let mut answers = serde_json::Map::new();
            answers.insert(qid, json!({"answers": [label]}));
            Some(json!({"answers": answers}))
        }
    }

    /// Serialized: GRAY_HOME is process-global and tests share the process.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static CTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn with_home(body: impl FnOnce(&std::path::Path)) {
        let _g = LOCK.lock().unwrap();
        let n = CTR.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("gray-preset-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("GRAY_HOME", &dir) };
        body(&dir);
        unsafe { std::env::remove_var("GRAY_HOME") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn preset(argv: &[&str]) -> Value {
        call("command/run", json!({"name": "/preset", "argv": argv, "session": {"id": "s", "cwd": "/tmp"}}))
    }

    #[test]
    fn manifest_has_preset_command() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "preset");
        assert_eq!(m["commands"], json!(["/preset"]));
        assert_eq!(m["tools"], json!([]));
        assert_eq!(m["capabilities"], json!(["host.ask"]));
    }

    #[test]
    fn new_list_use_flow() {
        with_home(|home| {
            // Empty list hints at /preset new.
            let r = preset(&[]);
            assert!(r["result"]["text"].as_str().unwrap().contains("No presets"));

            // new creates the file and points the user at it.
            let r = preset(&["new", "plan"]);
            let text = r["result"]["text"].as_str().unwrap();
            assert!(text.contains("created") && text.contains("plan.md"));
            let file = home.join("presets/plan.md");
            assert!(file.exists());

            // new on an existing file doesn't clobber.
            std::fs::write(&file, "Read-only mode. Do not edit files.").unwrap();
            let r = preset(&["new", "plan"]);
            assert!(r["result"]["text"].as_str().unwrap().contains("already exists"));
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "Read-only mode. Do not edit files.");

            // list shows it.
            let r = preset(&["list"]);
            assert!(r["result"]["text"].as_str().unwrap().contains("plan"));

            // use returns {prompt} carrying the file's contents.
            let r = preset(&["use", "plan"]);
            assert_eq!(
                r["result"]["prompt"],
                "Apply these working preferences for the rest of the session:\nRead-only mode. Do not edit files."
            );

            // Unknown preset says what's available.
            let r = preset(&["use", "nope"]);
            assert!(r["result"]["text"].as_str().unwrap().contains("Unknown preset \"nope\""));
            assert!(r["result"]["text"].as_str().unwrap().contains("plan"));
        });
    }

    #[test]
    fn empty_preset_does_not_prompt() {
        with_home(|_| {
            preset(&["new", "blank"]);
            let r = preset(&["use", "blank"]);
            assert!(r["result"]["prompt"].is_null());
            assert!(r["result"]["text"].as_str().unwrap().contains("empty"));
        });
    }

    #[test]
    fn names_cant_escape_dir() {
        assert_eq!(normalize("plan"), Some("plan".into()));
        assert_eq!(normalize("plan.md"), Some("plan".into()));
        assert_eq!(normalize("../evil"), None);
        assert_eq!(normalize("a/b"), None);
        assert_eq!(normalize(".."), None);
        assert_eq!(normalize(""), None);
        with_home(|_| {
            let r = preset(&["new", "../evil"]);
            assert!(r["result"]["text"].as_str().unwrap().contains("usage:"));
        });
    }

    #[test]
    fn bare_preset_picker_applies_chosen() {
        with_home(|_| {
            preset(&["new", "plan"]);
            std::fs::write(
                presets_dir().join("plan.md"),
                "Read-only mode. Do not edit files.",
            )
            .unwrap();
            preset(&["new", "fast"]);
            let mut ask = picking(0); // "fast" sorts first alphabetically
            let r = call_ask(
                "command/run",
                json!({"name": "/preset", "argv": [], "session": {"id": "s", "cwd": "/tmp"}}),
                &mut ask,
            );
            // "fast" is empty → the text answer path; "plan" proves {prompt}.
            assert!(r["result"]["text"].as_str().unwrap().contains("empty"), "{r}");
            let mut ask = picking(1); // "plan"
            let r = call_ask(
                "command/run",
                json!({"name": "/preset", "argv": [], "session": {"id": "s", "cwd": "/tmp"}}),
                &mut ask,
            );
            assert_eq!(
                r["result"]["prompt"],
                "Apply these working preferences for the rest of the session:\nRead-only mode. Do not edit files."
            );
        });
    }

    #[test]
    fn bare_preset_picker_none_lists() {
        with_home(|_| {
            preset(&["new", "plan"]);
            let mut ask = picking(1); // "none" is the last option
            let r = call_ask(
                "command/run",
                json!({"name": "/preset", "argv": [], "session": {"id": "s", "cwd": "/tmp"}}),
                &mut ask,
            );
            let text = r["result"]["text"].as_str().unwrap();
            assert!(text.contains("1 preset(s)") && text.contains("plan"), "{text}");
        });
    }

    #[test]
    fn bare_preset_no_ask_falls_back_to_list() {
        with_home(|_| {
            preset(&["new", "plan"]);
            let r = preset(&[]); // ask returns None
            let text = r["result"]["text"].as_str().unwrap();
            assert!(text.contains("1 preset(s)") && text.contains("plan"), "{text}");
        });
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle_preset(&json!({ "id": 2, "method": "plugin/shutdown" }), &mut |_p: &Value| None);
        assert!(reply.is_some() && exit);
    }
}
