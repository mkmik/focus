//! Just enough of herdr's socket API for the notes: newline-delimited JSON over a Unix socket.
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

/// Whether the workspace named `name` is in use: it's the one herdr shows, or an agent in it is at
/// work. Its screens can't tell: dashboards like `ccwt ws` redraw with nobody there.
pub fn in_use(socket: &Path, name: &str) -> io::Result<bool> {
    let mut ids = Vec::new();
    for w in list(socket, "workspace.list", "workspaces")? {
        if w["label"] == name {
            // ponytail: shown isn't watched (herdr may sit behind another app); gate on recent
            // input (CGEventSourceSecondsSinceLastEventType) if that matters.
            if w["focused"] == true {
                return Ok(true);
            }
            ids.push(w["workspace_id"].clone());
        }
    }
    // Each pane's status, not the workspace's: that says "blocked" if any agent waits on you.
    Ok(list(socket, "pane.list", "panes")?
        .iter()
        .any(|p| ids.contains(&p["workspace_id"]) && p["agent_status"] == "working"))
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
fn tells_workspaces_in_use() {
    use std::os::unix::net::UnixListener;
    let path = std::env::temp_dir().join(format!("focus-test-{}-in-use.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    // A herdr that answers any number of requests: "shown" is on screen, "idle" has an agent done
    // working, and "busy" one at work, next to one that makes the workspace say "blocked".
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let mut conn = conn.unwrap();
            let mut line = String::new();
            BufReader::new(&conn).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let result = match request["method"].as_str().unwrap() {
                "workspace.list" => json!({"workspaces": [
                    {"workspace_id": "w1", "label": "shown", "focused": true},
                    {"workspace_id": "w2", "label": "idle", "focused": false},
                    {"workspace_id": "w3", "label": "busy", "focused": false},
                ]}),
                "pane.list" => json!({"panes": [
                    {"workspace_id": "w1", "agent_status": "unknown"},
                    {"workspace_id": "w2", "agent_status": "idle"},
                    {"workspace_id": "w3", "agent_status": "blocked"},
                    {"workspace_id": "w3", "agent_status": "working"},
                ]}),
                method => panic!("{method}"),
            };
            writeln!(conn, "{}", json!({"id": "1", "result": result})).unwrap();
        }
    });

    for (name, used) in [
        ("shown", true),
        ("idle", false),
        ("busy", true),
        ("gone", false),
    ] {
        assert_eq!(in_use(&path, name).unwrap(), used, "{name}");
    }
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
