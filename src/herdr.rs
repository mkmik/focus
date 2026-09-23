//! Just enough of herdr's socket API for the notes: newline-delimited JSON over a Unix socket.
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

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
    let mut stream = UnixStream::connect(socket)?;
    // We're on the main thread: a wedged server mustn't freeze the notes.
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(b"{\"id\":\"1\",\"method\":\"workspace.list\",\"params\":{}}\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let reply: Value = serde_json::from_str(&line)?;
    let list = reply["result"]["workspaces"]
        .as_array()
        .ok_or_else(|| io::Error::other(line.trim().to_owned()))?;
    Ok(list
        .iter()
        .filter_map(|w| w["label"].as_str().map(String::from))
        .collect())
}

#[test]
fn lists_workspace_names() {
    use serde_json::json;
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
