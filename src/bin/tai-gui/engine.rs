//! The engine link (ADR 0010): tai starts `ppxai-server --owned` as its
//! own child and talks to it over loopback HTTP with a bearer token.
//!
//! `--owned` (ppxai ADR 0017, unreleased, ships in 1.19.5) binds 127.0.0.1
//! on a port it picks, prints `{"host","port","pid","version"}` as its
//! first stdout line, requires the token on every route, and on POSIX stops
//! when its stdin closes. A server without the flag exits 2 with argparse's
//! "unrecognized arguments: --owned"; ADR 0010 falls back to plain mode
//! then, which this prototype doesn't do yet: it says so instead.
//!
//! The server and everything it starts end with tai. A one-file build is a
//! bootloader that starts the real server as its child, so stopping only
//! the process tai started would leave the server running:
//! - Windows: the child starts suspended, goes into a job object with
//!   kill-on-close, and only then runs, so its children are in the job
//!   too. Closing the job's handle, or tai dying, ends them all.
//! - Unix: the child leads its own process group, which tai signals; its
//!   stdin closing also stops it.

use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use eframe::egui;

/// How long to wait for the "bound" line. It took about 17 s on the
/// Windows host with debug logging on (ADR 0010, notes), so be generous.
const BIND_TIMEOUT: Duration = Duration::from_secs(90);
/// How long the server may take to answer /health once it's bound.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// stderr lines kept for an error message.
const STDERR_TAIL: usize = 40;

/// Where the engine is.
pub enum State {
    Starting(&'static str),
    Ready(Link),
    Failed(String),
}

/// A ready server: where it is and how to reach it.
#[derive(Clone)]
pub struct Link {
    pub base: String,
    pub version: String,
    pub pid: u64,
    token: String,
    agent: ureq::Agent,
}

impl Link {
    /// GET `path` with tai's bearer: the status and the body.
    #[allow(dead_code)]
    pub fn get(&self, path: &str) -> Result<(u16, String), String> {
        let mut response = self
            .agent
            .get(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .call()
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        Ok((status, body))
    }

    /// POST `body` as JSON to `path` with tai's bearer, waiting up to
    /// `timeout` for the answer: the status and the body.
    pub fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> Result<(u16, String), String> {
        let mut response = self
            .agent
            .post(format!("{}{path}", self.base))
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        Ok((status, body))
    }
}

/// The engine as the UI holds it. Dropping it stops the server.
pub struct Engine {
    pub state: Arc<Mutex<State>>,
    server: Arc<Mutex<Option<Server>>>,
}

impl Engine {
    /// Start the server on a thread of its own; `ctx` is woken on each
    /// change of state.
    pub fn start(ctx: egui::Context) -> Self {
        let state = Arc::new(Mutex::new(State::Starting("finding ppxai-server")));
        let server = Arc::new(Mutex::new(None));
        let (s, srv) = (Arc::clone(&state), Arc::clone(&server));
        let spawned = std::thread::Builder::new()
            .name("engine-start".into())
            .spawn(move || {
                let set = |next: State| {
                    if let Ok(mut st) = s.lock() {
                        *st = next;
                    }
                    ctx.request_repaint();
                };
                let result = launch(&srv, &|step| set(State::Starting(step)));
                match result {
                    Ok(link) => {
                        tracing::info!(
                            "engine: ppxai-server {} ready at {} (pid {})",
                            link.version,
                            link.base,
                            link.pid
                        );
                        set(State::Ready(link));
                    }
                    Err(why) => {
                        tracing::error!("engine: {why}");
                        // Don't leave a half-started server behind.
                        if let Ok(mut srv) = srv.lock() {
                            srv.take();
                        }
                        set(State::Failed(why));
                    }
                }
            });
        if let Err(e) = spawned
            && let Ok(mut st) = state.lock()
        {
            *st = State::Failed(format!("couldn't start a thread: {e}"));
        }
        Self { state, server }
    }
}

impl Engine {
    /// Stop the server and everything it started.
    pub fn stop(&self) {
        if let Ok(mut server) = self.server.lock() {
            server.take();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The running server process. Dropping it stops the whole tree.
struct Server {
    child: Child,
    #[cfg(windows)]
    job: windows::Job,
}

impl Drop for Server {
    fn drop(&mut self) {
        // POSIX: EOF on stdin stops an --owned server gracefully.
        drop(self.child.stdin.take());
        #[cfg(unix)]
        {
            use nix::{sys::signal, unistd::Pid};
            let group = Pid::from_raw(self.child.id() as i32);
            _ = signal::killpg(group, signal::Signal::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            _ = signal::killpg(group, signal::Signal::SIGKILL);
        }
        #[cfg(windows)]
        self.job.terminate();
        _ = self.child.wait();
        tracing::info!("engine: ppxai-server stopped");
    }
}

fn launch(server: &Mutex<Option<Server>>, step: &dyn Fn(&'static str)) -> Result<Link, String> {
    let bin = find_server()?;
    tracing::info!("engine: starting {} --owned", bin.display());
    let token = new_token()?;

    step("starting ppxai-server");
    let mut command = Command::new(&bin);
    command
        .arg("--owned")
        .env("PPXAI_API_TOKEN", &token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
        command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("couldn't start {}: {e}", bin.display()))?;
    #[cfg(windows)]
    let job = match windows::Job::adopt(&child) {
        Ok(job) => job,
        Err(e) => {
            _ = child.kill();
            return Err(format!("couldn't put ppxai-server in a job object: {e}"));
        }
    };

    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    spawn_stderr_reader(stderr, Arc::clone(&tail));
    let first_line = spawn_stdout_reader(stdout);
    let mut guard = server.lock().map_err(|_| "engine lock poisoned")?;
    *guard = Some(Server {
        child,
        #[cfg(windows)]
        job,
    });
    drop(guard);

    step("waiting for ppxai-server to bind");
    let deadline = Instant::now() + BIND_TIMEOUT;
    let line = loop {
        if let Some(status) = exited(server) {
            return Err(exit_reason(status, &tail));
        }
        match first_line.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => break line,
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "ppxai-server didn't report its port within {} s",
                    BIND_TIMEOUT.as_secs()
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(300));
                return Err(match exited(server) {
                    Some(status) => exit_reason(status, &tail),
                    None => "ppxai-server closed its stdout".into(),
                });
            }
        }
    };
    let bound = parse_bound(&line)?;
    let base = format!("http://127.0.0.1:{}", bound.port);

    step("waiting for ppxai-server to answer");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let link = Link {
        base,
        version: bound.version,
        pid: bound.pid,
        token,
        agent,
    };
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(status) = exited(server) {
            return Err(exit_reason(status, &tail));
        }
        match link.get("/health") {
            Ok((200, _)) => break,
            Ok((401, _)) => {
                return Err("ppxai-server refused tai's token (401 from /health)".into());
            }
            Ok((status, _)) => tracing::debug!("engine: /health answered {status}"),
            Err(e) => tracing::debug!("engine: /health: {e}"),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "ppxai-server didn't answer /health within {} s",
                READY_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // --owned requires the token everywhere; a 200 here would mean the
    // server isn't in that mode.
    let tokenless = link
        .agent
        .get(format!("{}/health", link.base))
        .call()
        .map(|r| r.status().as_u16());
    match tokenless {
        Ok(401) => {}
        Ok(status) => {
            return Err(format!(
                "ppxai-server answered {status} to a request without the token; \
                 expected 401 in --owned mode"
            ));
        }
        Err(e) => return Err(format!("checking the token requirement: {e}")),
    }
    Ok(link)
}

/// `TAI_PPXAI_SERVER`, else `ppxai-server` on PATH, else ppxai's own
/// install folder (`~/.ppxai/bin`).
fn find_server() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("TAI_PPXAI_SERVER") {
        return Ok(PathBuf::from(path));
    }
    let name = if cfg!(windows) {
        "ppxai-server.exe"
    } else {
        "ppxai-server"
    };
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|p| p.is_file());
    if let Some(path) = on_path {
        return Ok(path);
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    if let Some(path) = home
        .map(|h| Path::new(&h).join(".ppxai").join("bin").join(name))
        .filter(|p| p.is_file())
    {
        return Ok(path);
    }
    Err(format!(
        "no {name} on PATH or in ~/.ppxai/bin; set TAI_PPXAI_SERVER to its path"
    ))
}

/// A fresh random token, 32 bytes as hex.
fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("no randomness for the token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

struct Bound {
    port: u16,
    pid: u64,
    version: String,
}

/// The "bound" line: one JSON object with host, port, pid and version.
fn parse_bound(line: &str) -> Result<Bound, String> {
    let bad = || format!("ppxai-server's first line isn't the --owned JSON: {line:?}");
    let v: serde_json::Value = serde_json::from_str(line.trim()).map_err(|_| bad())?;
    if v["host"].as_str() != Some("127.0.0.1") {
        return Err(bad());
    }
    let port = v["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0)
        .ok_or_else(bad)?;
    Ok(Bound {
        port,
        pid: v["pid"].as_u64().unwrap_or(0),
        version: v["version"].as_str().unwrap_or("?").to_string(),
    })
}

fn exited(server: &Mutex<Option<Server>>) -> Option<std::process::ExitStatus> {
    let mut guard = server.lock().ok()?;
    guard.as_mut()?.child.try_wait().ok().flatten()
}

/// Why the server exited, from its status and the end of its stderr.
fn exit_reason(status: std::process::ExitStatus, tail: &Mutex<VecDeque<String>>) -> String {
    // Let the stderr reader catch up with what the process wrote last.
    std::thread::sleep(Duration::from_millis(200));
    let tail: Vec<String> = tail
        .lock()
        .map(|t| t.iter().cloned().collect())
        .unwrap_or_default();
    let text = tail.join("\n");
    if text.contains("unrecognized arguments") && text.contains("--owned") {
        return "this ppxai-server has no --owned flag (it comes with ppxai 1.19.5). \
                ADR 0010's plain-mode fallback isn't in the prototype"
            .into();
    }
    let last = tail
        .iter()
        .rev()
        .take(5)
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join(" | ");
    format!("ppxai-server exited ({status}): {last}")
}

fn spawn_stderr_reader(stderr: impl Read + Send + 'static, tail: Arc<Mutex<VecDeque<String>>>) {
    _ = std::thread::Builder::new()
        .name("engine-stderr".into())
        .spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                tracing::debug!(target: "ppxai_server", "{line}");
                if let Ok(mut t) = tail.lock() {
                    if t.len() == STDERR_TAIL {
                        t.pop_front();
                    }
                    t.push_back(line);
                }
            }
        });
}

/// Sends the first stdout line, then keeps draining stdout so the server
/// never blocks on a full pipe.
fn spawn_stdout_reader(stdout: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    _ = std::thread::Builder::new()
        .name("engine-stdout".into())
        .spawn(move || {
            let mut lines = BufReader::new(stdout).lines().map_while(Result::ok);
            if let Some(first) = lines.next() {
                _ = tx.send(first);
            }
            drop(tx);
            for line in lines {
                tracing::debug!(target: "ppxai_server", "{line}");
            }
        });
    rx
}

#[cfg(windows)]
mod windows {
    #![allow(unsafe_code)]
    //! A kill-on-close job object holding the server's process tree.

    use std::{os::windows::io::AsRawHandle, process::Child};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    pub struct Job(HANDLE);

    // SAFETY: a job handle can be used and closed from any thread.
    unsafe impl Send for Job {}

    impl Job {
        /// Put a child started with CREATE_SUSPENDED into a new
        /// kill-on-close job, then let it run. Processes it starts from
        /// then on are in the job too.
        pub fn adopt(child: &Child) -> std::io::Result<Self> {
            // SAFETY: plain Win32 calls on handles this function owns or
            // borrows from `child`; every handle opened here is closed.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let job = Self(job);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    std::mem::size_of_val(&info) as u32,
                ) == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                resume_threads(child.id())?;
                Ok(job)
            }
        }

        pub fn terminate(&self) {
            // SAFETY: the handle is open until drop.
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closing the last handle kills what's left in the job.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Resume the threads of a process started suspended. std's Command
    /// doesn't return the main thread's handle, so find it by process id.
    ///
    /// SAFETY: plain Win32 calls; every handle opened here is closed.
    unsafe fn resume_threads(pid: u32) -> std::io::Result<()> {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut resumed = 0;
            let mut more = Thread32First(snapshot, &mut entry) != 0;
            while more {
                if entry.th32OwnerProcessID == pid {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        if ResumeThread(thread) != u32::MAX {
                            resumed += 1;
                        }
                        CloseHandle(thread);
                    }
                }
                more = Thread32Next(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
            if resumed == 0 {
                return Err(std::io::Error::other("no thread of the child to resume"));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_bound;

    #[test]
    fn the_bound_line_gives_port_pid_and_version() {
        let b = parse_bound(
            r#"{"host": "127.0.0.1", "port": 54001, "pid": 4242, "version": "1.19.5"}"#,
        )
        .unwrap();
        assert_eq!((b.port, b.pid, b.version.as_str()), (54001, 4242, "1.19.5"));
    }

    #[test]
    fn a_bound_line_off_loopback_or_without_a_port_is_refused() {
        assert!(
            parse_bound(r#"{"host": "0.0.0.0", "port": 1, "pid": 1, "version": "x"}"#).is_err()
        );
        assert!(
            parse_bound(r#"{"host": "127.0.0.1", "port": 0, "pid": 1, "version": "x"}"#).is_err()
        );
        assert!(parse_bound("INFO: Started server process").is_err());
    }
}
