use crate::git::{command, DiffSource, Repo};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const PLUGIN: &str = "zeppelinpp.git-panel";

pub fn toggle_command() -> String {
    format!("{PLUGIN}.toggle")
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub direction: String,
    pub push_key: String,
    pub history_limit: usize,
    pub auto_refresh: bool,
    pub default_remote: String,
    pub hosts: HashMap<String, String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            direction: "right".into(),
            push_key: "ctrl+p".into(),
            history_limit: 150,
            auto_refresh: true,
            default_remote: "origin".into(),
            hosts: HashMap::new(),
        }
    }
}
impl Config {
    pub fn load() -> Result<Self> {
        let path = config_dir()?.join("config.json");
        let cfg: Self = if path.exists() {
            serde_json::from_str(&fs::read_to_string(path)?)?
        } else {
            Self::default()
        };
        if cfg.history_limit == 0 {
            bail!("history_limit must be greater than zero");
        }
        if !matches!(cfg.direction.as_str(), "right" | "down") {
            bail!("direction must be right or down (supported by Herdr)");
        }
        Ok(cfg)
    }
}
fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_default())
}
pub fn state_dir() -> Result<PathBuf> {
    let path = env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state/herdr-git-panel"));
    fs::create_dir_all(&path)?;
    Ok(path)
}
pub fn config_dir() -> Result<PathBuf> {
    Ok(env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config/herdr/plugins/config").join(PLUGIN)))
}
fn herdr(args: &[&str]) -> Result<String> {
    command(
        &env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into()),
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        &env::current_dir()?,
        15,
    )
}
pub fn cli(args: &[&str]) -> Result<Value> {
    let value: Value = serde_json::from_str(&herdr(args)?)?;
    value
        .get("result")
        .cloned()
        .context("Herdr returned no result")
}
pub fn rpc(method: &str, params: Value) -> Result<Value> {
    let mut socket =
        UnixStream::connect(env::var("HERDR_SOCKET_PATH").context("No Herdr socket context")?)?;
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    socket.set_write_timeout(Some(Duration::from_secs(15)))?;
    writeln!(
        socket,
        "{}",
        json!({"id":"git-panel", "method": method, "params":params})
    )?;
    let mut line = String::new();
    BufReader::new(socket).read_line(&mut line)?;
    let value: Value = serde_json::from_str(&line)?;
    if let Some(error) = value.get("error") {
        bail!("{error}");
    }
    value
        .get("result")
        .cloned()
        .context("Herdr returned no result")
}

/// A ready agent in the caller's workspace. Pane identity is rechecked before
/// delivery because a terminal may have closed or changed occupants meanwhile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPane {
    pub pane_id: String,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub(crate) terminal_id: String,
    pub(crate) revision: u64,
}

fn agent_context() -> Result<(String, String)> {
    if env::var("HERDR_ENV").as_deref() != Ok("1") {
        bail!("Sending a selection requires a Herdr-managed pane");
    }
    let workspace = env::var("HERDR_WORKSPACE_ID")
        .context("No Herdr workspace context; reopen the Git panel inside Herdr")?;
    let pane = env::var("HERDR_PANE_ID")
        .context("No Herdr pane context; reopen the Git panel inside Herdr")?;
    if workspace.is_empty()
        || pane.is_empty()
        || env::var("HERDR_SOCKET_PATH").unwrap_or_default().is_empty()
    {
        bail!("Incomplete Herdr context; reopen the Git panel inside Herdr");
    }
    Ok((workspace, pane))
}

fn ready_agent(value: &Value) -> Option<AgentPane> {
    let kind = value.get("agent")?.as_str()?.trim();
    let status = value.get("agent_status")?.as_str()?;
    if kind.is_empty()
        || !matches!(status, "idle" | "done")
        || value.get("launch_pending").and_then(Value::as_bool) == Some(true)
        || value.get("interactive_ready").and_then(Value::as_bool) == Some(false)
    {
        return None;
    }
    let string = |key| {
        value
            .get(key)?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let name = ["name", "label", "title", "terminal_title_stripped"]
        .into_iter()
        .find_map(string)
        .unwrap_or_else(|| kind.to_string());
    let name = name
        .chars()
        .flat_map(|character| {
            if character.is_control() {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect();
    Some(AgentPane {
        pane_id: string("pane_id")?,
        name,
        kind: kind.to_string(),
        status: status.to_string(),
        workspace_id: string("workspace_id")?,
        tab_id: string("tab_id")?,
        terminal_id: string("terminal_id")?,
        revision: value.get("revision")?.as_u64()?,
    })
}

fn foreground_agent(target: &AgentPane) -> Result<bool> {
    let result = rpc("pane.process_info", json!({"pane_id": target.pane_id}))?;
    let processes = result
        .pointer("/process_info/foreground_processes")
        .and_then(Value::as_array)
        .context("Herdr returned no foreground process information")?;
    Ok(processes
        .iter()
        .any(|process| process_matches_agent(process, &target.kind)))
}

fn process_matches_agent(process: &Value, kind: &str) -> bool {
    let kind = kind.to_ascii_lowercase().replace([' ', '_'], "-");
    let aliases: &[&str] = match kind.as_str() {
        "claude" | "claude-code" => &["claude", "claude-code"],
        "pi" => &["pi", "pi-coding-agent"],
        "gemini" => &["gemini", "gemini-cli"],
        "cursor" => &["cursor", "cursor-agent"],
        "agy" => &["agy", "antigravity"],
        "qwen" => &["qwen", "qwen-code"],
        "kilo" => &["kilo", "kilocode"],
        "omp" => &["omp", "oh-my-pi"],
        _ => &[kind.as_str()],
    };
    let executable_matches = |path: &str| {
        let name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let name = name
            .trim_end_matches(".exe")
            .trim_end_matches(".js")
            .trim_end_matches(".mjs");
        aliases
            .iter()
            .any(|alias| name == *alias || name.starts_with(&format!("{alias}-")))
    };
    if ["name", "argv0"]
        .into_iter()
        .filter_map(|key| process.get(key).and_then(Value::as_str))
        .any(executable_matches)
    {
        return true;
    }
    let Some(argv) = process.get("argv").and_then(Value::as_array) else {
        return false;
    };
    let Some(executable) = argv.first().and_then(Value::as_str) else {
        return false;
    };
    if executable_matches(executable) {
        return true;
    }
    // Package CLIs can run directly under a JavaScript/Python interpreter.
    // Inspect its script path, never arbitrary later command arguments.
    let launcher = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if !matches!(launcher, "node" | "bun" | "deno" | "python" | "python3") {
        return false;
    }
    argv.get(1).and_then(Value::as_str).is_some_and(|script| {
        executable_matches(script)
            || Path::new(script).components().any(|component| {
                component.as_os_str().to_str().is_some_and(|part| {
                    let part = part.trim_start_matches('@');
                    aliases.contains(&part)
                })
            })
    })
}

pub fn available_agents() -> Result<Vec<AgentPane>> {
    let (workspace, own_pane) = agent_context()?;
    let result = rpc("agent.list", json!({}))?;
    let agents = result
        .get("agents")
        .and_then(Value::as_array)
        .context("Herdr returned no agent list")?;
    let mut targets = Vec::new();
    for agent in agents {
        let Some(target) = ready_agent(agent) else {
            continue;
        };
        if target.workspace_id == workspace
            && target.pane_id != own_pane
            && foreground_agent(&target).unwrap_or(false)
        {
            targets.push(target);
        }
    }
    targets.sort_by(|a, b| (&a.name, &a.pane_id).cmp(&(&b.name, &b.pane_id)));
    Ok(targets)
}

/// Paste into a revalidated agent's existing input. Submission remains a user
/// action in that pane; this function never sends keys, focuses or clears it.
pub fn send_selection(target: &AgentPane, text: &str) -> Result<()> {
    let (workspace, own_pane) = agent_context()?;
    if text.len() > 128 * 1024 {
        bail!("Selection exceeds the 128 KiB delivery limit; select fewer lines");
    }
    if text.trim().is_empty()
        || text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        bail!("Selection is empty or contains unsupported control characters");
    }
    if target.workspace_id != workspace || target.pane_id == own_pane {
        bail!("The target is outside this workspace or is the current pane");
    }
    let result = rpc("agent.get", json!({"target": target.pane_id}))
        .context("The selected agent is no longer available; reopen the target list")?;
    let current = result
        .get("agent")
        .and_then(ready_agent)
        .context("The selected agent is no longer ready; reopen the target list")?;
    if current.pane_id != target.pane_id
        || current.workspace_id != target.workspace_id
        || current.terminal_id != target.terminal_id
        || current.revision != target.revision
        || current.kind != target.kind
    {
        bail!("The selected pane changed occupants; reopen the target list");
    }
    if !foreground_agent(&current)? {
        bail!("The agent is not in the foreground; reopen the target list when it is ready");
    }
    rpc(
        "pane.send_text",
        json!({"pane_id": target.pane_id, "text": text}),
    )
    .context(
        "Delivery failed and may have reached the target; inspect its input before retrying",
    )?;
    Ok(())
}
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Missing Herdr {key}"))
}
fn marker() -> Result<PathBuf> {
    // Encode opaque context, do not assume any particular Herdr ID spelling.
    let key = format!(
        "{}\0{}",
        env::var("HERDR_SOCKET_PATH")?,
        env::var("HERDR_TAB_ID")?
    );
    let key: String = key.bytes().map(|b| format!("{b:02x}")).collect();
    // Use a stable short checksum; verify every stored pane's live process before closing it.
    let hash = key.bytes().fold(0xcbf29ce484222325u64, |a, b| {
        (a ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    Ok(state_dir()?.join(format!("{hash:016x}.json")))
}
pub fn register() -> Result<()> {
    if let (Ok(pane), Ok(_)) = (env::var("HERDR_PANE_ID"), env::var("HERDR_TAB_ID")) {
        atomic_write(&marker()?, json!({"pane_id":pane}).to_string().as_bytes())?;
    }
    Ok(())
}
fn owns_process(process: &Value) -> bool {
    let argv: Vec<_> = process
        .get("argv")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    argv.contains(&"panel")
        && (argv
            .first()
            .and_then(|p| Path::new(p).file_name())
            .is_some_and(|p| p == "herdr-git-panel")
            || argv.contains(&"herdr_git_panel"))
}
pub fn toggle() -> Result<()> {
    if env::var("HERDR_ENV").as_deref() != Ok("1") {
        bail!("Run this action inside Herdr");
    }
    let ctx: Value = serde_json::from_str(
        &env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_else(|_| "{}".into()),
    )?;
    let fallback = env::var("HERDR_PANE_ID").ok();
    let pane = ctx
        .get("focused_pane_id")
        .and_then(Value::as_str)
        .or(fallback.as_deref())
        .context("No invoking pane")?;
    let result = cli(&["pane", "get", pane])?;
    let current = &result["pane"];
    let tab = field(current, "tab_id")?;
    let workspace = field(current, "workspace_id")?;
    // Actions have authoritative focus context, including when invoked from the panel itself.
    env::set_var("HERDR_TAB_ID", tab);
    let path = marker()?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    lock.lock_exclusive()?;
    let panes = cli(&["pane", "list", "--workspace", workspace])?;
    // Discover our live UI in this tab, including an old process surviving an upgrade.
    for candidate in panes["panes"].as_array().context("No pane listing")? {
        if candidate.get("tab_id").and_then(Value::as_str) != Some(tab) {
            continue;
        }
        let id = field(candidate, "pane_id")?;
        let info = cli(&["pane", "process-info", "--pane", id])?;
        let processes = info["process_info"]["foreground_processes"]
            .as_array()
            .context("No process listing")?;
        if processes.iter().any(owns_process) {
            cli(&["pane", "close", id])?;
            let _ = fs::remove_file(path);
            return Ok(());
        }
    }
    let cwd = current
        .get("foreground_cwd")
        .and_then(Value::as_str)
        .or_else(|| ctx.get("focused_pane_cwd").and_then(Value::as_str))
        .or_else(|| current.get("cwd").and_then(Value::as_str))
        .context("No repository working directory")?;
    let repo = Repo::discover(cwd)?;
    let config = Config::load()?;
    cli(&[
        "plugin",
        "pane",
        "open",
        "--plugin",
        PLUGIN,
        "--entrypoint",
        "panel",
        "--placement",
        "split",
        "--direction",
        &config.direction,
        "--target-pane",
        pane,
        "--cwd",
        repo.path.to_str().context("Non-UTF8 repository path")?,
        "--env",
        &format!("GIT_PANEL_REPO={}", repo.path.display()),
        "--env",
        &format!("GIT_PANEL_SOURCE_PANE={pane}"),
        "--focus",
    ])?;
    Ok(())
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiffData {
    pub title: String,
    pub patch: String,
    #[serde(default)]
    pub source: Option<DiffSource>,
}
pub fn open_diff(data: &DiffData) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(state_dir()?)?;
    serde_json::to_writer(&mut file, data)?;
    file.flush()?;
    let (_, path) = file.keep()?;
    let result = rpc(
        "plugin.pane.open",
        json!({"plugin_id":PLUGIN,"entrypoint":"diff", "placement":"popup", "width":"90%", "height":"88%", "focus":true, "env":{"GIT_PANEL_DIFF":path}}),
    );
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result.map(|_| ())
}
pub fn take_diff() -> Result<DiffData> {
    let path = PathBuf::from(env::var("GIT_PANEL_DIFF")?);
    let data = fs::read_to_string(&path)?;
    fs::remove_file(path)?;
    Ok(serde_json::from_str(&data)?)
}
pub fn root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("HERDR_PLUGIN_ROOT") {
        return Ok(root.into());
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(path) = exe
            .ancestors()
            .find(|p| p.join("herdr-plugin.toml").exists())
        {
            return Ok(path.to_path_buf());
        }
    }
    let path = env::current_dir()?;
    if !path.join("herdr-plugin.toml").exists() {
        bail!("Run setup from the plugin checkout");
    }
    Ok(path)
}
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().context("No parent directory")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(data)?;
    temporary.persist(path)?;
    Ok(())
}
pub fn install_binding(config_path: &Path) -> Result<bool> {
    let path = if config_path.exists() {
        config_path.canonicalize()?
    } else {
        config_path.to_path_buf()
    };
    let original = fs::read_to_string(&path).unwrap_or_default();
    let doc = original.parse::<toml_edit::DocumentMut>()?;
    let command = toggle_command();
    let mut present = false;
    if let Some(commands) = doc
        .get("keys")
        .and_then(|k| k.get("command"))
        .and_then(|v| v.as_array_of_tables())
    {
        for cmd in commands {
            let Some(key) = cmd.get("key") else {
                continue;
            };
            let matches = key.as_str() == Some("cmd+shift+r")
                || key
                    .as_array()
                    .is_some_and(|a| a.iter().any(|s| s.as_str() == Some("cmd+shift+r")));
            if matches {
                if cmd.get("command").and_then(|v| v.as_str()) != Some(command.as_str()) {
                    bail!("cmd+shift+r is already assigned in {}", path.display());
                }
                present = true;
            }
        }
    }
    if present {
        return Ok(false);
    }
    if path.exists() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        fs::copy(
            &path,
            path.with_extension(format!("toml.git-panel-{stamp}.bak")),
        )?;
    }
    let updated = format!("{}\n\n[[keys.command]]\nkey = \"cmd+shift+r\"\ntype = \"plugin_action\"\ncommand = \"{command}\"\ndescription = \"Toggle Git panel\"\n", original.trim_end());
    updated.parse::<toml_edit::DocumentMut>()?;
    atomic_write(&path, updated.as_bytes())?;
    Ok(true)
}
pub fn setup() -> Result<()> {
    let cfg = env::var_os("HERDR_CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config/herdr/config.toml"));
    install_binding(&cfg)?;
    cli(&[
        "plugin",
        "link",
        root()?.to_str().context("Non-UTF8 plugin path")?,
    ])?;
    let directory = PathBuf::from(herdr(&["plugin", "config-dir", PLUGIN])?.trim());
    fs::create_dir_all(&directory)?;
    let path = directory.join("config.json");
    if !path.exists() {
        atomic_write(
            &path,
            serde_json::to_string_pretty(&Config::default())?.as_bytes(),
        )?;
    }
    cli(&["server", "reload-config"])?;
    println!(
        "Ready: Cmd+Shift+R opens Git Panel. Config: {}",
        path.display()
    );
    Ok(())
}
