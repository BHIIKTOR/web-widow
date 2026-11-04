//! Production CDP-over-pipe client for the assisted-login browser (v0.9).
//!
//! chromiumoxide 0.9.1 has no pipe transport (item 3), so the login
//! browser speaks the DevTools protocol as **NUL-delimited JSON over fd 3
//! (commands in) and fd 4 (replies out)**. The browser is a dedicated **headed**
//! process with **no `--remote-debugging-port`**, so it publishes no
//! `DevToolsActivePort` and no TCP DevTools listener: a same-user attach attempt
//! has nothing to connect to. This is the isolation the renderer and the
//! interaction loop must never share.
//!
//! The pipe ends are bound to fd 3/4 by a fixed `/bin/sh` prelude rather than by
//! `pre_exec`: the crate has no direct `libc` dependency, and the constant
//! script keeps the child's fd table exactly as Chrome expects (`3` read end,
//! `4` write end, `0`/`1` on `/dev/null`).
//!
//! The optional `extra_args` on [`LaunchOptions`] exists for the hermetic
//! integration fixture only; the CLI never sets it, and
//! [`LaunchOptions::cli`] is the constructor the login flow uses.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::core::error::{Result, WidowError};

/// Constant prelude: duplicate the child's stdin/stdout onto fd 3/4 (a shell
/// redirection is not close-on-exec, so Chrome inherits them), blank fd 0/1,
/// then exec the browser with the caller's arguments.
const LAUNCH_SCRIPT: &str = r#"exec 3<&0 4>&1 0</dev/null 1>/dev/null; exec "$0" "$@""#;

/// The only environment names the auth browser inherits. Anything else — a
/// manager token, a webhook secret, the HTTP bearer — stays in the operator's
/// shell and never reaches the browser tree.
pub const LAUNCH_ENV_ALLOW: [&str; 6] = [
    "HOME",
    "XDG_RUNTIME_DIR",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "TMPDIR",
];
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// How to launch the login browser.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub chrome: PathBuf,
    /// Dedicated, wiped-at-exit profile directory.
    pub profile: PathBuf,
    /// Where Chrome's stderr goes (mode 0600; never a credential).
    pub stderr_log: PathBuf,
    /// Test-only extra arguments (the hermetic fixture's trust anchor).
    pub extra_args: Vec<String>,
}

impl LaunchOptions {
    /// The production launch line: headed, pipe-only, no TCP DevTools endpoint.
    pub fn cli(chrome: PathBuf, profile: PathBuf, stderr_log: PathBuf) -> Self {
        Self {
            chrome,
            profile,
            stderr_log,
            extra_args: Vec::new(),
        }
    }

    /// The argv Chrome is launched with (also the assertion surface for tests).
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--remote-debugging-pipe".to_string(),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "--disable-background-networking".to_string(),
            "--disable-sync".to_string(),
            "--disable-component-update".to_string(),
            "--password-store=basic".to_string(),
            "--user-data-dir=".to_string() + &self.profile.display().to_string(),
        ];
        args.extend(self.extra_args.iter().cloned());
        args.push("about:blank".to_string());
        args
    }
}

/// The protocol-event sink invoked on the reader thread.
pub type EventHandler = Arc<dyn Fn(&Control, &Value) + Send + Sync>;

/// Reply/event multiplexer. Cloned into the event handler so a paused request
/// can be answered from the reader thread without re-entering the browser.
#[derive(Clone)]
pub struct Control {
    writer: Arc<Mutex<BufWriter<std::io::PipeWriter>>>,
    next_id: Arc<AtomicU64>,
}

impl Control {
    /// Fire-and-forget command; returns the request id (0 when the pipe is gone).
    pub fn send(&self, method: &str, params: Value, session: Option<&str>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = Value::String(session.to_string());
        }
        let mut bytes = message.to_string().into_bytes();
        bytes.push(0);
        if let Ok(mut writer) = self.writer.lock() {
            if writer
                .write_all(&bytes)
                .and_then(|_| writer.flush())
                .is_err()
            {
                return 0;
            }
        } else {
            return 0;
        }
        id
    }
}

/// What a target attachment needs.
#[derive(Debug, Clone)]
pub struct Target {
    pub id: String,
    pub session: String,
}

/// A running, pipe-only browser.
pub struct Browser {
    control: Control,
    child: Child,
    pending: Arc<Mutex<HashMap<u64, Sender<Value>>>>,
    closed: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    profile: PathBuf,
}

impl Browser {
    /// Spawn the browser and start the reply reader. `handler` receives every
    /// protocol event on the reader thread; it must only use [`Control::send`].
    pub fn launch(options: &LaunchOptions, handler: EventHandler) -> Result<Self> {
        if !options.chrome.is_file() {
            return Err(WidowError::AuthLoginBrowserFailed {
                detail: format!("chrome binary not found at {}", options.chrome.display()),
            });
        }
        std::fs::create_dir_all(&options.profile).map_err(|error| {
            WidowError::AuthLoginBrowserFailed {
                detail: format!("profile directory could not be created: {error}"),
            }
        })?;
        let (command_read, command_write) = std::io::pipe().map_err(browser_error)?;
        let (reply_read, reply_write) = std::io::pipe().map_err(browser_error)?;

        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&options.stderr_log)
            .map_err(browser_error)?;

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(LAUNCH_SCRIPT)
            .arg(&options.chrome)
            .args(options.args())
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(command_read)))
            .stdout(Stdio::from(std::os::fd::OwnedFd::from(reply_write)))
            .stderr(Stdio::from(stderr))
            // A minimal environment. The browser (and the shell
            // prelude) see only these names plus the two descriptors.
            .env_clear();
        for name in LAUNCH_ENV_ALLOW {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let child = command
            .spawn()
            .map_err(|error| WidowError::AuthLoginBrowserFailed {
                detail: format!("login browser could not be started: {error}"),
            })?;

        let control = Control {
            writer: Arc::new(Mutex::new(BufWriter::new(command_write))),
            next_id: Arc::new(AtomicU64::new(0)),
        };
        let pending: Arc<Mutex<HashMap<u64, Sender<Value>>>> = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let reader = {
            let control = control.clone();
            let pending = Arc::clone(&pending);
            let closed = Arc::clone(&closed);
            std::thread::spawn(move || {
                read_loop(
                    BufReader::new(reply_read),
                    control,
                    pending,
                    handler,
                    closed,
                )
            })
        };
        Ok(Self {
            control,
            child,
            pending,
            closed,
            reader: Some(reader),
            profile: options.profile.clone(),
        })
    }

    pub fn control(&self) -> &Control {
        &self.control
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn profile(&self) -> &Path {
        &self.profile
    }

    /// Send a command and wait for its reply.
    pub fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        self.call_timeout(method, params, session, DEFAULT_TIMEOUT)
    }

    pub fn call_timeout(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        let (sender, receiver) = mpsc::channel();
        let id = {
            let mut next = self.control.next_id.fetch_add(1, Ordering::Relaxed);
            next += 1;
            if let Ok(mut pending) = self.pending.lock() {
                pending.insert(next, sender);
            }
            let mut message = json!({"id": next, "method": method, "params": params});
            if let Some(session) = session {
                message["sessionId"] = Value::String(session.to_string());
            }
            let mut bytes = message.to_string().into_bytes();
            bytes.push(0);
            let mut writer = self
                .control
                .writer
                .lock()
                .map_err(|_| browser_error("pipe writer poisoned"))?;
            writer.write_all(&bytes).map_err(browser_error)?;
            writer.flush().map_err(browser_error)?;
            next
        };
        match receiver.recv_timeout(timeout) {
            Ok(message) => {
                if let Some(error) = message.get("error") {
                    return Err(WidowError::AuthLoginBrowserFailed {
                        detail: format!("{method} refused: {error}"),
                    });
                }
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            }
            Err(_) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&id);
                }
                Err(WidowError::AuthLoginBrowserFailed {
                    detail: format!("{method} timed out after {} ms", timeout.as_millis()),
                })
            }
        }
    }

    pub fn version(&self) -> Result<String> {
        let result = self.call("Browser.getVersion", json!({}), None)?;
        Ok(result
            .get("product")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    pub fn create_target(&self, url: &str) -> Result<String> {
        let result = self.call("Target.createTarget", json!({"url": url}), None)?;
        result
            .get("targetId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| browser_error("Target.createTarget returned no id"))
    }

    pub fn attach(&self, target_id: &str) -> Result<String> {
        let result = self.call(
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
            None,
        )?;
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| browser_error("Target.attachToTarget returned no session"))
    }

    pub fn enable_page(&self, session: &str) -> Result<()> {
        self.call("Page.enable", json!({}), Some(session))?;
        self.call("Runtime.enable", json!({}), Some(session))?;
        self.call("Network.enable", json!({}), Some(session))?;
        Ok(())
    }

    /// Pause every request so the navigation gate can refuse documents outside
    /// the declared login origins, and answer HTTP auth challenges with a
    /// failure rather than a credential.
    pub fn enable_fetch(&self, session: &str) -> Result<()> {
        self.call(
            "Fetch.enable",
            json!({
                "patterns": [{"urlPattern": "*", "requestStage": "Request"}],
                "handleAuthRequests": true
            }),
            Some(session),
        )?;
        Ok(())
    }

    /// Attach future page targets paused, so a popup cannot navigate before the
    /// gate is installed on its session.
    pub fn auto_attach_pages(&self, session: Option<&str>) -> Result<()> {
        let params = json!({
            "autoAttach": true,
            "waitForDebuggerOnStart": true,
            "flatten": true,
            "filter": [{"type": "page", "exclude": false}, {"type": "iframe", "exclude": false}]
        });
        self.call("Target.setAutoAttach", params, session)?;
        Ok(())
    }

    /// Browser-session auto-attach for popups and SharedWorkers: page-level
    /// attach does not discover either of them.
    pub fn auto_attach_browser(&self) -> Result<()> {
        let params = json!({
            "autoAttach": true,
            "waitForDebuggerOnStart": true,
            "flatten": true,
            "filter": [
                {"type": "page", "exclude": false},
                {"type": "shared_worker", "exclude": false}
            ]
        });
        self.call("Target.setAutoAttach", params, None)?;
        Ok(())
    }

    pub fn navigate(&self, session: &str, url: &str) -> Result<()> {
        self.call("Page.navigate", json!({"url": url}), Some(session))?;
        Ok(())
    }

    /// Fire-and-forget navigation. A gated navigation may be paused by the gate
    /// (or aborted by a refused challenge), so waiting for the command reply
    /// would turn a correct refusal into a timeout; the caller observes
    /// `Page.loadEventFired` / the gate's own signals instead.
    pub fn navigate_async(&self, session: &str, url: &str) {
        self.control
            .send("Page.navigate", json!({"url": url}), Some(session));
    }

    pub fn evaluate(&self, session: &str, expression: &str) -> Result<Value> {
        let result = self.call(
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true}),
            Some(session),
        )?;
        Ok(result.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Every cookie in the browser's jar. Values are secrets: they are only ever
    /// filtered, encrypted and wiped, never logged.
    pub fn cookies(&self, session: Option<&str>) -> Result<Vec<Value>> {
        self.cookies_for(session, &[])
    }

    /// Cookies matching `urls` (empty means the whole jar).
    pub fn cookies_for(&self, session: Option<&str>, urls: &[&str]) -> Result<Vec<Value>> {
        let params = if urls.is_empty() {
            json!({})
        } else {
            json!({ "urls": urls })
        };
        let result = self.call("Network.getCookies", params, session)?;
        Ok(result
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub fn close_target(&self, target_id: &str) -> Result<()> {
        let _ = self.call("Target.closeTarget", json!({"targetId": target_id}), None);
        Ok(())
    }

    /// The login launch facts: no TCP listener in the pid tree and no
    /// `DevToolsActivePort` file.
    pub fn facts(&self) -> BrowserFacts {
        BrowserFacts {
            pid: self.pid(),
            pids: pid_tree(self.pid()),
            tcp_listeners: listening_sockets(&pid_tree(self.pid())),
            devtools_active_port_absent: !self.profile.join("DevToolsActivePort").exists(),
        }
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if !self.closed.load(Ordering::Relaxed) {
            self.stop();
        }
    }
}

/// Observable launch facts (evidence, and the CLI's own assertion).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BrowserFacts {
    pub pid: u32,
    pub pids: Vec<u32>,
    pub tcp_listeners: Vec<String>,
    pub devtools_active_port_absent: bool,
}

impl BrowserFacts {
    pub fn no_tcp_listener(&self) -> bool {
        self.tcp_listeners.is_empty()
    }

    pub fn is_isolated(&self) -> bool {
        self.no_tcp_listener() && self.devtools_active_port_absent
    }
}

fn browser_error(error: impl std::fmt::Display) -> WidowError {
    WidowError::AuthLoginBrowserFailed {
        detail: error.to_string(),
    }
}

fn read_loop(
    mut reader: BufReader<std::io::PipeReader>,
    control: Control,
    pending: Arc<Mutex<HashMap<u64, Sender<Value>>>>,
    handler: EventHandler,
    closed: Arc<AtomicBool>,
) {
    let mut buffer: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        if closed.load(Ordering::Relaxed) {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        while let Some(position) = buffer.iter().position(|byte| *byte == 0) {
            let raw: Vec<u8> = buffer.drain(..=position).collect();
            let text = String::from_utf8_lossy(&raw[..raw.len().saturating_sub(1)]);
            if text.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(id) = message.get("id").and_then(Value::as_u64) {
                if let Ok(mut pending) = pending.lock()
                    && let Some(sender) = pending.remove(&id)
                {
                    let _ = sender.send(message);
                }
            } else if message.get("method").is_some() {
                handler(&control, &message);
            }
        }
    }
}

/// The pid and all its descendants, from `/proc`.
fn pid_tree(root: u32) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|value| value.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            // `comm` can contain spaces and parentheses; the parent pid is the
            // field after the closing parenthesis.
            let Some(close) = stat.rfind(')') else {
                continue;
            };
            let mut rest = stat[close + 1..].split_whitespace();
            let _state = rest.next();
            let Some(ppid) = rest.next().and_then(|value| value.parse::<u32>().ok()) else {
                continue;
            };
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        out.push(current);
        if let Some(next) = children.get(&current) {
            stack.extend(next.iter().copied());
        }
    }
    out
}

/// Listening TCP sockets owned by `pids`, rendered as `addr:port` strings.
fn listening_sockets(pids: &[u32]) -> Vec<String> {
    let mut inodes: HashMap<String, ()> = HashMap::new();
    for pid in pids {
        let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let Some(text) = target.to_str() else {
                continue;
            };
            if let Some(inode) = text
                .strip_prefix("socket:[")
                .and_then(|rest| rest.strip_suffix(']'))
            {
                inodes.insert(inode.to_string(), ());
            }
        }
    }
    if inodes.is_empty() {
        return Vec::new();
    }
    let mut listeners = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 {
                continue;
            }
            // st == 0A is LISTEN.
            if fields[3] != "0A" {
                continue;
            }
            if !inodes.contains_key(fields[9]) {
                continue;
            }
            let local = fields[1];
            let (address, port) = local.split_once(':').unwrap_or((local, ""));
            let port = u16::from_str_radix(port, 16).unwrap_or(0);
            listeners.push(format!("{address}:{port}"));
        }
    }
    listeners
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launch_line_is_headed_and_pipe_only() {
        let options = LaunchOptions::cli(
            PathBuf::from("/usr/bin/chrome"),
            PathBuf::from("/tmp/profile"),
            PathBuf::from("/tmp/chrome.log"),
        );
        let args = options.args();
        assert!(args.contains(&"--remote-debugging-pipe".to_string()));
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("--remote-debugging-port"))
        );
        assert!(!args.iter().any(|arg| arg.contains("--headless")));
        assert!(
            args.iter()
                .any(|arg| arg.starts_with("--user-data-dir=/tmp/profile"))
        );
    }

    #[test]
    fn the_cli_launch_line_has_no_test_only_arguments() {
        let options = LaunchOptions::cli(
            PathBuf::from("/usr/bin/chrome"),
            PathBuf::from("/tmp/profile"),
            PathBuf::from("/tmp/chrome.log"),
        );
        assert!(options.extra_args.is_empty());
        assert!(
            !options
                .args()
                .iter()
                .any(|arg| arg.contains("ignore-certificate"))
        );
    }

    #[test]
    fn the_launch_prelude_binds_three_and_four_and_blanks_standard_io() {
        assert!(LAUNCH_SCRIPT.contains("exec 3<&0"));
        assert!(LAUNCH_SCRIPT.contains("4>&1"));
        assert!(LAUNCH_SCRIPT.contains("0</dev/null"));
        assert!(LAUNCH_SCRIPT.contains("1>/dev/null"));
    }

    #[test]
    fn the_launch_environment_allowlist_is_minimal() {
        assert_eq!(
            LAUNCH_ENV_ALLOW,
            [
                "HOME",
                "XDG_RUNTIME_DIR",
                "DISPLAY",
                "WAYLAND_DISPLAY",
                "XAUTHORITY",
                "TMPDIR"
            ]
        );
        for forbidden in [
            "PATH",
            "OP_SERVICE_ACCOUNT_TOKEN",
            "BW_SESSION",
            "WEB_WIDOW_HTTP_TOKEN",
            "DBUS_SESSION_BUS_ADDRESS",
        ] {
            assert!(!LAUNCH_ENV_ALLOW.contains(&forbidden), "{forbidden}");
        }
    }

    #[test]
    fn a_missing_chrome_is_a_typed_refusal() {
        let options = LaunchOptions::cli(
            PathBuf::from("/nonexistent/chrome"),
            PathBuf::from("/tmp/ww-profile-test"),
            PathBuf::from("/tmp/ww-chrome-test.log"),
        );
        let error = match Browser::launch(&options, Arc::new(|_, _| {})) {
            Ok(_) => panic!("a missing binary must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.code().as_str(), "auth_login_browser_failed");
    }

    #[test]
    fn a_pid_tree_and_its_listeners_are_enumerable() {
        // The test process itself: the tree contains our pid and the scan is a
        // well-formed (possibly empty) listener list.
        let pids = pid_tree(std::process::id());
        assert!(pids.contains(&std::process::id()));
        let _ = listening_sockets(&pids);
    }
}
