//! Just enough of herdr's socket API for the notes: newline-delimited JSON over a Unix socket.
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// herdr exports HERDR_SOCKET_PATH into its panes; elsewhere, the default session's socket.
pub fn socket() -> PathBuf {
    std::env::var_os("HERDR_SOCKET_PATH").map_or_else(
        || {
            std::env::home_dir()
                .expect("no home dir")
                .join(".config/herdr/herdr.sock")
        },
        PathBuf::from,
    )
}

/// The workspace names (labels), in herdr's order.
pub fn workspaces(socket: &Path) -> io::Result<Vec<String>> {
    Ok(list(socket, "workspace.list", "workspaces")?
        .iter()
        .filter_map(|w| w["label"].as_str().map(String::from))
        .collect())
}

/// Whether any pane in the workspace named `name` shows something else than at the last call with
/// the same `screens`, which it keeps up to date (pane id → text on screen). herdr doesn't say when
/// a pane last changed, so we look: anyone typing, or an agent at work, changes what's on screen.
pub fn changed(
    socket: &Path,
    name: &str,
    screens: &mut HashMap<String, String>,
) -> io::Result<bool> {
    let ids: Vec<Value> = list(socket, "workspace.list", "workspaces")?
        .into_iter()
        .filter(|w| w["label"] == name)
        .map(|w| w["workspace_id"].clone())
        .collect();
    let mut now = HashMap::new();
    let mut changed = false;
    for pane in list(socket, "pane.list", "panes")? {
        if !ids.contains(&pane["workspace_id"]) {
            continue;
        }
        let Some(id) = pane["pane_id"].as_str() else {
            continue;
        };
        let params = json!({"pane_id": id, "source": "visible", "format": "text"});
        // One that can't be read (closed since the list?) sits this look out.
        let Ok(read) = call(socket, "pane.read", params) else {
            continue;
        };
        let text = read["read"]["text"].as_str().unwrap_or_default();
        // A pane new to us (just opened, or we just started looking) has nothing to compare with.
        changed |= screens.get(id).is_some_and(|was| was != text);
        now.insert(id.to_owned(), text.to_owned());
    }
    *screens = now;
    Ok(changed)
}

/// Calls `method`, which takes no params, for the `key` list in its result.
fn list(socket: &Path, method: &str, key: &str) -> io::Result<Vec<Value>> {
    call(socket, method, json!({}))?[key]
        .as_array()
        .cloned()
        .ok_or_else(|| io::Error::other(format!("no {key} from {method}")))
}

/// Calls `method` with `params`, for its result.
fn call(socket: &Path, method: &str, params: Value) -> io::Result<Value> {
    let mut stream = UnixStream::connect(socket)?;
    // We're on the main thread: a wedged server mustn't freeze the notes.
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    let request = json!({"id": "1", "method": method, "params": params});
    stream.write_all(format!("{request}\n").as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let mut reply: Value = serde_json::from_str(&line)?;
    reply
        .get_mut("result")
        .map(Value::take)
        .ok_or_else(|| io::Error::other(line.trim().to_owned()))
}

#[test]
fn spots_changed_screens() {
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};
    let path = std::env::temp_dir().join(format!("focus-test-{}-screens.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    // What's on each pane's screen, by pane id: workspace a (w1) has two tabs, b (w2) one.
    let shown = Arc::new(Mutex::new(HashMap::from([
        ("w1:p1", ("w1", "$")),
        ("w1:p2", ("w1", "$")),
        ("w2:p1", ("w2", "$")),
    ])));
    // A herdr that answers any number of requests, from `shown`.
    let panes = Arc::clone(&shown);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let mut conn = conn.unwrap();
            let mut line = String::new();
            BufReader::new(&conn).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let panes = panes.lock().unwrap();
            let result = match request["method"].as_str().unwrap() {
                "workspace.list" => json!({"workspaces": [
                    {"workspace_id": "w1", "label": "a"},
                    {"workspace_id": "w2", "label": "b"},
                ]}),
                "pane.list" => {
                    let list = panes
                        .iter()
                        .map(|(id, (w, _))| json!({"pane_id": id, "workspace_id": w}));
                    json!({"panes": list.collect::<Vec<_>>()})
                }
                "pane.read" => {
                    let (_, text) = panes[request["params"]["pane_id"].as_str().unwrap()];
                    json!({"read": {"text": text}})
                }
                method => panic!("{method}"),
            };
            writeln!(conn, "{}", json!({"id": "1", "result": result})).unwrap();
        }
    });

    let mut screens = HashMap::new();
    // Nothing to compare with at first, then nothing new.
    assert!(!changed(&path, "a", &mut screens).unwrap());
    assert!(!changed(&path, "a", &mut screens).unwrap());
    // Typing in b is none of a's business, and a new tab in a is only new.
    shown.lock().unwrap().insert("w2:p1", ("w2", "$ ls"));
    shown.lock().unwrap().insert("w1:p3", ("w1", "$"));
    assert!(!changed(&path, "a", &mut screens).unwrap());
    // Typing in a's second tab is.
    shown.lock().unwrap().insert("w1:p2", ("w1", "$ ls"));
    assert!(changed(&path, "a", &mut screens).unwrap());
    assert!(!changed(&path, "a", &mut screens).unwrap());
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn lists_workspace_names() {
    use std::os::unix::net::UnixListener;
    // A one-shot herdr: checks the request, then sends `reply`.
    let path = std::env::temp_dir().join(format!("focus-test-{}.sock", std::process::id()));
    let serve = |reply: Value| {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&conn).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "workspace.list");
            writeln!(conn, "{reply}").unwrap();
        })
    };

    let herdr = serve(
        json!({"id": "1", "result": {"type": "workspace_list", "workspaces": [
            {"workspace_id": "w1", "label": "focus"},
            {"workspace_id": "w2", "label": "say \"hi\"", "worktree": {"repo_name": "x"}},
        ]}}),
    );
    assert_eq!(workspaces(&path).unwrap(), ["focus", "say \"hi\""]);
    herdr.join().unwrap();

    let herdr = serve(json!({"id": "1", "error": {"code": "x", "message": "nope"}}));
    let err = workspaces(&path).unwrap_err().to_string();
    assert!(err.contains("nope"), "{err}");
    herdr.join().unwrap();
    std::fs::remove_file(&path).unwrap();
}
