use herdr_git_panel::host::{available_agents, send_selection};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

fn agent(pane: &str, kind: &str, status: &str) -> Value {
    json!({"pane_id":pane,"agent":kind,"agent_status":status,"name":pane,
        "workspace_id":"w1","tab_id":"w1:t1","terminal_id":format!("term-{pane}"),"revision":3})
}

fn agents() -> Vec<Value> {
    let mut other_workspace = agent("w2:elsewhere", "codex", "idle");
    other_workspace["workspace_id"] = json!("w2");
    let mut not_ready = agent("w1:not-ready", "codex", "idle");
    not_ready["interactive_ready"] = json!(false);
    let mut launching = agent("w1:launching", "codex", "idle");
    launching["launch_pending"] = json!(true);
    vec![
        agent("w1:codex", "codex", "idle"),
        agent("w1:claude", "claude", "done"),
        agent("w1:pi", "pi", "idle"),
        agent("w1:gemini", "gemini", "idle"),
        agent("w1:busy", "codex", "working"),
        agent("w1:blocked", "codex", "blocked"),
        agent("w1:unknown", "codex", "unknown"),
        agent("w1:plugin", "codex", "idle"),
        agent("w1:suspended", "codex", "idle"),
        other_workspace,
        not_ready,
        launching,
    ]
}

fn reply(mode: &str, request: &Value, process_reads: &mut HashMap<String, usize>) -> Value {
    let method = request["method"].as_str().unwrap();
    let result = match method {
        "agent.list" => json!({"agents": agents()}),
        "agent.get" => {
            if mode == "closed" {
                return json!({"error":{"code":"pane_not_found","message":"Pane closed"}});
            }
            let mut target = agent("w1:codex", "codex", "idle");
            match mode {
                "replaced" => {
                    target["terminal_id"] = json!("replacement");
                }
                "revision" => {
                    target["revision"] = json!(4);
                }
                "blocked" => {
                    target["agent_status"] = json!("blocked");
                }
                _ => (),
            }
            json!({"agent":target})
        }
        "pane.process_info" => {
            let pane = request["params"]["pane_id"].as_str().unwrap();
            let reads = process_reads.entry(pane.to_string()).or_default();
            *reads += 1;
            let process = if pane == "w1:suspended"
                || (mode == "background" && pane == "w1:codex" && *reads > 1)
            {
                json!({"name":"zsh","argv0":"zsh","argv":["zsh","-c","echo codex"]})
            } else if pane == "w1:claude" {
                json!({"name":"node","argv0":"node","argv":["node","/lib/node_modules/@anthropic-ai/claude-code/cli.js"]})
            } else if pane == "w1:pi" {
                json!({"name":"node","argv0":"node","argv":["node","/lib/node_modules/@mariozechner/pi-coding-agent/dist/cli.js"]})
            } else if pane == "w1:gemini" {
                json!({"name":"node","argv0":"node","argv":["node","/lib/node_modules/@google/gemini-cli/dist/index.js"]})
            } else {
                json!({"name":"codex","argv0":"codex","argv":["/vendor/codex-darwin-arm64/codex"]})
            };
            json!({"process_info":{"foreground_processes":[process]}})
        }
        "pane.send_text" if mode == "uncertain" => {
            return json!({"error":{"code":"write_failed","message":"write failed"}});
        }
        "pane.send_text" => json!({"type":"pane_text_sent"}),
        _ => panic!("Unexpected Herdr request: {request}"),
    };
    json!({"result": result})
}

#[test]
fn delivery_uses_ready_workspace_agents_and_revalidates_before_pasting() {
    for mode in [
        "success",
        "replaced",
        "revision",
        "closed",
        "blocked",
        "background",
        "unsafe",
        "oversize",
        "uncertain",
        "outside",
    ] {
        let root = tempfile::Builder::new()
            .prefix("agent-delivery-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = root.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::clone(&stop);
        let requests = Arc::clone(&recorded);
        let server = thread::spawn(move || {
            let mut process_reads = HashMap::new();
            while !running.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(3)))
                            .unwrap();
                        let mut line = String::new();
                        BufReader::new(socket.try_clone().unwrap())
                            .read_line(&mut line)
                            .unwrap();
                        let request: Value = serde_json::from_str(&line).unwrap();
                        requests.lock().unwrap().push(request.clone());
                        writeln!(socket, "{}", reply(mode, &request, &mut process_reads)).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        });
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "agent_delivery_probe", "--nocapture"])
            .env("HERDR_TEST_DELIVERY_MODE", mode)
            .env("HERDR_ENV", if mode == "outside" { "0" } else { "1" })
            .env("HERDR_WORKSPACE_ID", "w1")
            .env("HERDR_PANE_ID", "w1:plugin")
            .env("HERDR_SOCKET_PATH", &socket)
            .output()
            .unwrap();
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let requests = recorded.lock().unwrap();
        let sends: Vec<_> = requests
            .iter()
            .filter(|request| request["method"] == "pane.send_text")
            .collect();
        if matches!(mode, "success" | "uncertain") {
            assert_eq!(sends.len(), 1, "{mode}: no automatic retries");
            assert_eq!(
                sends[0]["params"],
                json!({"pane_id":"w1:codex", "text": payload()})
            );
        } else {
            assert!(sends.is_empty(), "{mode}: unexpected delivery");
        }
        assert!(!requests.iter().any(|request| matches!(
            request["method"].as_str(),
            Some("pane.send_keys" | "agent.prompt" | "pane.focus" | "agent.focus")
        )));
        if mode == "outside" {
            assert!(requests.is_empty());
        }
        // The harness writes only its temporary socket and fixture directory.
        fs::remove_file(socket).unwrap();
    }
}

fn payload() -> &'static str {
    "Repository: /repo/中文\nFile: src/[x].rs\nold:12 new:12  let value = `literal`;\nold:- new:13 +\tprintln!(\"$(literal)\");\n"
}

#[test]
fn agent_delivery_probe() {
    let Ok(mode) = std::env::var("HERDR_TEST_DELIVERY_MODE") else {
        return;
    };
    if mode == "outside" {
        assert!(available_agents()
            .unwrap_err()
            .to_string()
            .contains("Herdr-managed"));
        return;
    }
    let targets = available_agents().unwrap();
    assert_eq!(
        targets
            .iter()
            .map(|target| target.pane_id.as_str())
            .collect::<Vec<_>>(),
        vec!["w1:claude", "w1:codex", "w1:gemini", "w1:pi"]
    );
    let target = targets
        .iter()
        .find(|target| target.pane_id == "w1:codex")
        .unwrap();
    let oversized = "x".repeat(128 * 1024 + 1);
    let text = match mode.as_str() {
        "unsafe" => "unsafe\u{1b}[201~\r",
        "oversize" => oversized.as_str(),
        _ => payload(),
    };
    let result = send_selection(target, text);
    if mode == "success" {
        result.unwrap();
    } else {
        let error = format!("{:#}", result.unwrap_err());
        match mode.as_str() {
            "replaced" | "revision" => assert!(error.contains("changed occupants"), "{error}"),
            "closed" => assert!(error.contains("no longer available"), "{error}"),
            "blocked" => assert!(error.contains("no longer ready"), "{error}"),
            "background" => assert!(error.contains("foreground"), "{error}"),
            "unsafe" => assert!(error.contains("control characters"), "{error}"),
            "oversize" => assert!(error.contains("128 KiB"), "{error}"),
            "uncertain" => assert!(error.contains("may have reached"), "{error}"),
            _ => panic!("unknown test mode"),
        }
    }
}
