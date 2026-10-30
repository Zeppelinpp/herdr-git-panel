//! Follow the last focused terminal in this panel's tab without polling Git.
use crate::git::Repo;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env,
    io::{BufRead, BufReader, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

pub(crate) struct Follow {
    latest: Arc<Mutex<Option<Repo>>>,
    stopped: Arc<AtomicBool>,
    socket: Arc<Mutex<Option<UnixStream>>>,
    stop: mpsc::Sender<()>,
}

impl Follow {
    pub(crate) fn start(wake: SyncSender<()>) -> Result<Option<Self>> {
        if env::var("HERDR_ENV").as_deref() != Ok("1") {
            return Ok(None);
        }
        let (Ok(path), Ok(tab), Ok(own)) = (
            env::var("HERDR_SOCKET_PATH"),
            env::var("HERDR_TAB_ID"),
            env::var("HERDR_PANE_ID"),
        ) else {
            return Ok(None);
        };
        let source = env::var("GIT_PANEL_SOURCE_PANE").ok();
        Self::connect(path, tab, own, source, wake).map(Some)
    }

    fn connect(
        path: String,
        tab: String,
        own: String,
        source: Option<String>,
        wake: SyncSender<()>,
    ) -> Result<Self> {
        let latest = Arc::new(Mutex::new(None));
        let stopped = Arc::new(AtomicBool::new(false));
        let socket = Arc::new(Mutex::new(None));
        let (stop, receiver) = mpsc::channel();
        let follower = Self {
            latest: latest.clone(),
            stopped: stopped.clone(),
            socket: socket.clone(),
            stop,
        };
        thread::Builder::new()
            .name("git-panel-follow".into())
            .spawn(move || {
                let mut selection = Selection::new(tab, own, source);
                let mut published = None;
                while !stopped.load(Ordering::Acquire) {
                    let result = (|| -> Result<()> {
                        let stream = UnixStream::connect(&path)?;
                        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                        *socket.lock().unwrap() = Some(stream.try_clone()?);
                        let mut reader = BufReader::new(stream);
                        let subscriptions: Vec<_> = [
                            "pane.updated",
                            "pane.focused",
                            "pane.created",
                            "pane.closed",
                            "pane.moved",
                            "tab.focused",
                            "worktree.opened",
                        ]
                        .into_iter()
                        .map(|kind| json!({"type": kind}))
                        .collect();
                        writeln!(
                            reader.get_mut(),
                            "{}",
                            json!({
                                "id":"git-panel-follow", "method":"events.subscribe",
                                "params":{"subscriptions": subscriptions}
                            })
                        )?;
                        let response = read(&mut reader)?;
                        anyhow::ensure!(
                            response.get("error").is_none(),
                            "Herdr refused directory tracking"
                        );
                        // Subscribe before reading initial state so intervening updates stay queued.
                        let snapshot = call(&path, "session.snapshot", json!({}))?;
                        selection.snapshot(&snapshot["snapshot"]);
                        publish(&selection, &mut published, &latest, &wake);
                        reader.get_ref().set_read_timeout(None)?;
                        while !stopped.load(Ordering::Acquire) {
                            let event = read(&mut reader)?;
                            if event.get("error").is_some() {
                                anyhow::bail!("Herdr directory event stream lost synchronization");
                            }
                            if matches!(
                                event["event"].as_str(),
                                Some("pane.focused" | "tab.focused" | "worktree.opened")
                            ) {
                                // Focus events have no revision and may precede
                                // the snapshot already read on this connection.
                                let snapshot = call(&path, "session.snapshot", json!({}))?;
                                selection.snapshot(&snapshot["snapshot"]);
                            } else {
                                selection.event(&event);
                            }
                            publish(&selection, &mut published, &latest, &wake);
                        }
                        Ok(())
                    })();
                    *socket.lock().unwrap() = None;
                    if result.is_err() && receiver.recv_timeout(Duration::from_secs(2)).is_err() {
                        continue;
                    }
                    break;
                }
            })?;
        Ok(follower)
    }

    pub(crate) fn take_latest(&self) -> Option<Repo> {
        self.latest.lock().unwrap().take()
    }
}

impl Drop for Follow {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.stop.send(());
        if let Some(socket) = self.socket.lock().unwrap().take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

fn read(reader: &mut BufReader<UnixStream>) -> Result<Value> {
    let mut line = String::new();
    anyhow::ensure!(
        reader.read_line(&mut line)? != 0,
        "Herdr closed directory tracking"
    );
    Ok(serde_json::from_str(&line)?)
}

fn call(path: &str, method: &str, params: Value) -> Result<Value> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    writeln!(
        stream,
        "{}",
        json!({"id":"git-panel-follow-state", "method":method, "params":params})
    )?;
    read(&mut BufReader::new(stream))?
        .get("result")
        .cloned()
        .context("Missing Herdr directory state")
}

fn publish(
    selection: &Selection,
    published: &mut Option<PathBuf>,
    latest: &Mutex<Option<Repo>>,
    wake: &SyncSender<()>,
) {
    let Some(cwd) = selection.cwd() else { return };
    if published.as_ref() == Some(&cwd) {
        return;
    }
    // Remember non-repository paths too; repeated terminal metadata changes must
    // not run Git again until its directory actually changes.
    *published = Some(cwd.clone());
    if let Ok(repo) = Repo::discover(cwd) {
        *latest.lock().unwrap() = Some(repo);
        let _ = wake.try_send(());
    }
}

struct Selection {
    tab: String,
    own: String,
    source: Option<String>,
    panes: HashMap<String, Value>,
}

impl Selection {
    fn new(tab: String, own: String, source: Option<String>) -> Self {
        Self {
            tab,
            own,
            source,
            panes: HashMap::new(),
        }
    }
    fn eligible(&self, pane: &Value) -> bool {
        pane["pane_id"].as_str() != Some(&self.own)
            && pane["tab_id"].as_str() == Some(&self.tab)
            // Popups belong to the tab too, but must not become directory sources.
            && pane["title"].as_str() != Some("Git diff")
    }
    fn snapshot(&mut self, snapshot: &Value) {
        self.panes.clear();
        if let Some(panes) = snapshot["panes"].as_array() {
            for pane in panes {
                if let Some(id) = pane["pane_id"].as_str() {
                    self.panes.insert(id.to_string(), pane.clone());
                }
            }
        }
        let candidate = self
            .panes
            .values()
            .find(|pane| self.eligible(pane) && pane["focused"].as_bool() == Some(true));
        if let Some(pane) = candidate {
            self.source = pane["pane_id"].as_str().map(str::to_string);
        } else if self.source.is_none() {
            let mut candidates = self.panes.values().filter(|pane| self.eligible(pane));
            if let (Some(pane), None) = (candidates.next(), candidates.next()) {
                self.source = pane["pane_id"].as_str().map(str::to_string);
            }
        }
    }
    fn event(&mut self, event: &Value) {
        let data = &event["data"];
        if let Some(pane) = data.get("pane") {
            if let Some(id) = pane["pane_id"].as_str() {
                if let (Some(incoming), Some(current)) = (
                    pane["revision"].as_u64(),
                    self.panes.get(id).and_then(|old| old["revision"].as_u64()),
                ) {
                    if incoming < current {
                        return;
                    }
                }
                self.panes.insert(id.to_string(), pane.clone());
                if self.eligible(pane) && pane["focused"].as_bool() == Some(true) {
                    self.source = Some(id.to_string());
                }
            }
        }
        if event["event"] == "pane.focused" {
            if let Some(id) = data["pane_id"].as_str() {
                if self.panes.get(id).is_some_and(|pane| self.eligible(pane)) {
                    self.source = Some(id.to_string());
                }
            }
        }
        if event["event"] == "pane.closed" {
            if let Some(id) = data["pane_id"].as_str() {
                self.panes.remove(id);
            }
        }
    }
    fn cwd(&self) -> Option<PathBuf> {
        let pane = self.panes.get(self.source.as_ref()?)?;
        if !self.eligible(pane) {
            return None;
        }
        pane["foreground_cwd"]
            .as_str()
            .filter(|path| !path.is_empty())
            .or_else(|| pane["cwd"].as_str().filter(|path| !path.is_empty()))
            .map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn events_queued_before_snapshot_cannot_restore_an_old_directory() {
        let mut selection = Selection::new("t1".into(), "panel".into(), Some("shell".into()));
        let mut current = pane("shell", "t1", "/linked", true);
        current["revision"] = json!(5);
        selection.snapshot(&json!({"panes":[current]}));
        let mut old = pane("shell", "t1", "/main", true);
        old["revision"] = json!(4);
        selection.event(&json!({"event":"pane.updated","data":{"pane":old}}));
        assert_eq!(selection.cwd(), Some("/linked".into()));
    }
    fn pane(id: &str, tab: &str, cwd: &str, focused: bool) -> Value {
        json!({"pane_id":id,"tab_id":tab,"cwd":"/old","foreground_cwd":cwd,"focused":focused})
    }
    #[test]
    fn directory_updates_follow_last_terminal_while_panel_has_focus() {
        let mut selection = Selection::new("t1".into(), "panel".into(), Some("shell".into()));
        selection.snapshot(
            &json!({"panes":[pane("shell","t1","/main",false),pane("panel","t1","/main",true)]}),
        );
        assert_eq!(selection.cwd(), Some("/main".into()));
        selection.event(
            &json!({"event":"pane.updated","data":{"pane":pane("shell","t1","/linked",false)}}),
        );
        assert_eq!(selection.cwd(), Some("/linked".into()));
    }
    #[test]
    fn focus_follows_siblings_and_ignores_other_tabs_and_diff_popup() {
        let mut selection = Selection::new("t1".into(), "panel".into(), Some("shell".into()));
        let mut popup = pane("diff", "t1", "/popup", true);
        popup["title"] = json!("Git diff");
        selection.snapshot(&json!({"panes":[pane("shell","t1","/main",false),pane("other","t1","/linked",false),pane("foreign","t2","/foreign",false),popup]}));
        for (id, expected) in [
            ("other", "/linked"),
            ("foreign", "/linked"),
            ("diff", "/linked"),
            ("panel", "/linked"),
            ("shell", "/main"),
        ] {
            selection.event(&json!({"event":"pane.focused","data":{"pane_id":id}}));
            assert_eq!(selection.cwd(), Some(expected.into()));
        }
    }
    #[test]
    fn socket_events_discover_linked_worktree_off_the_ui_thread() {
        use std::{fs, os::unix::net::UnixListener, process::Command};
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        let linked = root.path().join("linked");
        fs::create_dir(&main).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        git(&["worktree", "add", "-b", "linked", linked.to_str().unwrap()]);
        let socket_path = root.path().join("host.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let main_path = main.to_str().unwrap().to_string();
        let linked_path = linked.to_str().unwrap().to_string();
        let (advance, proceed) = mpsc::channel();
        let (finished, finish) = mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut events = BufReader::new(stream);
            let request = read(&mut events).unwrap();
            assert_eq!(request["method"], "events.subscribe");
            assert!(request["params"]["subscriptions"]
                .as_array()
                .unwrap()
                .contains(&json!({"type":"pane.updated"})));
            writeln!(
                events.get_mut(),
                "{}",
                json!({"id":"git-panel-follow","result":{"type":"subscription_started"}})
            )
            .unwrap();
            let (stream, _) = listener.accept().unwrap();
            let mut snapshot = BufReader::new(stream);
            assert_eq!(read(&mut snapshot).unwrap()["method"], "session.snapshot");
            writeln!(snapshot.get_mut(), "{}", json!({"result":{"snapshot":{"panes":[pane("shell","tab",&main_path,false),pane("panel","tab",&main_path,true)]}}})).unwrap();
            proceed.recv_timeout(Duration::from_secs(5)).unwrap();
            writeln!(events.get_mut(), "{}", json!({"event":"pane.updated","data":{"type":"pane_updated","pane":pane("shell","tab",&linked_path,false)}})).unwrap();
            // No further RPC or periodic Git read is needed while idle.
            finish.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let (wake, receiver) = mpsc::sync_channel(1);
        let follower = Follow::connect(
            socket_path.to_str().unwrap().into(),
            "tab".into(),
            "panel".into(),
            Some("shell".into()),
            wake,
        )
        .unwrap();
        receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            follower.take_latest().unwrap().path,
            fs::canonicalize(&main).unwrap()
        );
        advance.send(()).unwrap();
        receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        let repo = follower.take_latest().unwrap();
        assert_eq!(repo.path, fs::canonicalize(&linked).unwrap());
        assert_eq!(repo.snapshot("main", 10).unwrap().branch, "linked");
        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
        drop(follower);
        finished.send(()).unwrap();
        server.join().unwrap();
    }
}
