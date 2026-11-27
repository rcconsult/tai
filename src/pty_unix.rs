// Unfortunately, there will be some unsafe shenanigans here.
#![allow(unsafe_code)]
use std::{
    ffi::{CString, OsStr, OsString, c_char},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
    },
    time::{Duration, Instant},
};

use nix::{
    errno::Errno,
    fcntl::{self, OFlag},
    pty::ForkptyResult,
    sys::{signal, wait},
    unistd::{self, Pid},
};

use crate::{Dimensions, pty_input::Input};

type Chunk = Result<Vec<u8>, PtyError>;

/// The reader thread's wake-up call, set after the thread starts.
type Wake = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

fn wake(wake: &Wake) {
    if let Ok(w) = wake.lock()
        && let Some(f) = w.as_ref()
    {
        f();
    }
}

/// Chunks the reader thread may queue before it stops reading, which
/// makes a flooding child wait instead of tai's memory growing.
const QUEUED_CHUNKS: usize = 64;

/// Handle to a pseudo-terminal (PTY).
///
/// A reader thread drains the master into a channel as soon as output
/// arrives, as Ghostty's read thread does (`src/termio/Exec.zig`,
/// `ReadThread`, at a887df42). Reading only once per frame left the
/// child blocked on a full pty buffer while tai drew, which capped a
/// full-screen TUI at tai's frame rate. The terminal stays on the main
/// thread: libghostty-vt's types are `!Send`.
///
/// Input goes through a writer thread too (`src/pty_input.rs`).
pub struct Pty {
    fd: OwnedFd,
    /// The writer thread's queue.
    input: Input,
    output: Receiver<Chunk>,
    /// A chunk wait_readable took from `output`, for the next read.
    waited: Mutex<Option<Chunk>>,
    /// Called by the reader thread after it queues output (tai-gui).
    wake: Wake,
}

impl Pty {
    /// Spawn the user's default shell in a new pseudo-terminal.
    ///
    /// Creates a pty pair via forkpty(), sets the initial window size, execs the
    /// shell in the child, and puts the master fd into non-blocking mode so we
    /// can poll it each frame without stalling the render loop.
    ///
    /// The shell is chosen by checking, in order:
    ///   1. $SHELL environment variable
    ///   2. The pw_shell field from the passwd database
    ///   3. /bin/sh as a last resort
    ///
    /// It starts in `cwd` when that's a directory, else in tai's own.
    pub fn new(dims: Dimensions, cwd: Option<&std::path::Path>) -> std::io::Result<(Self, Child)> {
        // Checked before forking: a bad directory would make exec fail.
        let cwd = cwd.filter(|dir| dir.is_dir());
        // Work out the shell's locale before forking: it may run
        // `defaults` on macOS, and it logs (ADR 0003).
        let locale = crate::locale::child_env();
        // Other terminals' identity variables to drop (ADR 0005).
        let foreign = crate::identity::foreign_vars(std::env::vars_os());
        // Determine the user's preferred shell. We try $SHELL first (the
        // standard convention), then fall back to the passwd entry, and
        // finally to /bin/sh if nothing else is available. Chosen before
        // forking, so the shell integration's scripts can be written.
        let shell = match std::env::var_os("SHELL") {
            Some(shell) if !shell.is_empty() => PathBuf::from(shell),
            _ => match unistd::User::from_uid(unistd::getuid()) {
                Ok(Some(user)) => user.shell,
                _ => PathBuf::from("/bin/sh"),
            },
        };
        // On macOS, start a login shell ("-zsh") like Terminal.app does,
        // so ~/.zprofile and path_helper set up PATH. The shell
        // integration may start it otherwise (src/shell_integration.rs).
        let launch = crate::shell_integration::launch(&shell, cfg!(target_os = "macos"));

        // Everything the child needs is built here, before the fork: a
        // forked child of a threaded process may only make
        // async-signal-safe calls until exec, so it mustn't allocate (a
        // malloc lock another thread held at the fork would hang it). As
        // Ghostty's src/Command.zig does (at 108cacb1): argv and envp are
        // prepared first, and the child only resets state and execs.
        let exec = Exec::new(&shell, &launch, cwd, &foreign, &locale)?;

        // forkpty() combines openpty + fork + login_tty into one call.
        // In the child it sets up the slave side as stdin/stdout/stderr.
        // SAFETY: the child arm below makes only async-signal-safe calls
        // (close, signal, sigprocmask, chdir, execve, _exit) on memory
        // built before the fork, and allocates nothing.
        match unsafe { nix::pty::forkpty(&dims.to_winsize(), None)? } {
            // Child process -- replace ourselves with the shell.
            ForkptyResult::Child => {
                // Close every descriptor the shell would otherwise inherit
                // from tai beyond stdin, stdout and stderr: other terminals'
                // pty masters, which would keep their ptys from hanging up
                // when tai closes them (so their shells never get SIGHUP and
                // a wait for them never ends; popos-tai), and the GPU
                // driver's. forkpty's masters aren't close-on-exec, and
                // setting that in the parent would race a fork on another
                // thread.
                close_inherited_fds();
                // Give the shell default signal handling. Ignored signals
                // survive exec, so if whatever launched tai ignored SIGINT
                // (a background job does), Ctrl+C would never interrupt
                // anything in the shell, and under nohup (SIGHUP ignored) a
                // command would outlive its closed terminal (popos-tai).
                // Ghostty resets the same ones (src/pty.zig `childPreExec`
                // at 108cacb1), plus the job-control signals here.
                for sig in [
                    signal::Signal::SIGHUP,
                    signal::Signal::SIGINT,
                    signal::Signal::SIGQUIT,
                    signal::Signal::SIGTERM,
                    signal::Signal::SIGALRM,
                    signal::Signal::SIGABRT,
                    signal::Signal::SIGBUS,
                    signal::Signal::SIGFPE,
                    signal::Signal::SIGILL,
                    signal::Signal::SIGSEGV,
                    signal::Signal::SIGTRAP,
                    signal::Signal::SIGTSTP,
                    signal::Signal::SIGTTIN,
                    signal::Signal::SIGTTOU,
                    signal::Signal::SIGPIPE,
                    signal::Signal::SIGCHLD,
                ] {
                    // SAFETY: SigDfl installs no handler of ours, and no
                    // other thread exists in the child to race it.
                    _ = unsafe { signal::signal(sig, signal::SigHandler::SigDfl) };
                }
                _ = signal::sigprocmask(
                    signal::SigmaskHow::SIG_SETMASK,
                    Some(&signal::SigSet::empty()),
                    None,
                );
                exec.run()
            }

            // Parent -- make the master fd non-blocking so read() returns EAGAIN
            // instead of blocking when there's no data, letting us poll each frame.
            ForkptyResult::Parent { child, master: fd } => {
                let raw_flags = fcntl::fcntl(&fd, fcntl::F_GETFL)?;
                let flags = OFlag::from_bits_retain(raw_flags) | OFlag::O_NONBLOCK;
                _ = fcntl::fcntl(&fd, fcntl::F_SETFL(flags))?;

                let reader = fd.try_clone()?;
                let (tx, output) = mpsc::sync_channel::<Chunk>(QUEUED_CHUNKS);
                let wake: Wake = Arc::default();
                let reader_wake = Arc::clone(&wake);
                std::thread::Builder::new()
                    .name("pty-reader".into())
                    .spawn(move || read_loop(&reader, &tx, &reader_wake))?;

                let input = Input::spawn(MasterWriter(fd.try_clone()?))?;

                let pty = Self {
                    fd,
                    input,
                    output,
                    waited: Mutex::new(None),
                    wake,
                };
                Ok((pty, Child::Active(child)))
            }
        }
    }

    /// Wait until output is available, or `timeout` passes. The reader
    /// thread's channel has no peek, so a chunk that arrives is kept for
    /// the next read.
    pub fn wait_readable(&self, timeout: Duration) {
        let Ok(mut waited) = self.waited.lock() else {
            return;
        };
        if waited.is_some() {
            return;
        }
        match self.output.recv_timeout(timeout) {
            Ok(chunk) => *waited = Some(chunk),
            // read() reports a closed channel.
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
    }

    /// Pass the output received so far to `sink`, until `deadline`; the
    /// rest waits for the next frame.
    pub fn read(&self, sink: &mut dyn FnMut(&[u8]), deadline: Instant) -> Result<(), PtyError> {
        if let Some(chunk) = self.waited.lock().ok().and_then(|mut w| w.take()) {
            sink(&chunk?);
        }
        while Instant::now() < deadline {
            match self.output.try_recv() {
                Ok(chunk) => sink(&chunk?),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Err(PtyError::EndOfStream),
            }
        }
        Ok(())
    }

    /// Call `wake` from the reader thread whenever output arrives or ends,
    /// so a UI that sleeps until something happens (tai-gui) wakes for it.
    #[allow(dead_code)]
    pub fn wake_on_output(&self, wake: impl Fn() + Send + 'static) {
        if let Ok(mut w) = self.wake.lock() {
            *w = Some(Box::new(wake));
        }
    }

    /// Queue what the user typed or pasted (`src/pty_input.rs`).
    pub fn write(&self, data: &[u8]) {
        self.input.send(data);
    }

    /// Queue a reply tai generated for the child; capped
    /// (`src/pty_input.rs`).
    pub fn reply(&self, data: &[u8]) {
        self.input.send_reply(data);
    }

    /// Send a resize event to the pty via the TIOCSWINSZ ioctl.
    ///
    /// ioctl is a way to control a resource without inventing a dedicated
    /// system call for it, and in this case we have to generate bindings
    /// for this ioctl and feed the new window size to it.
    pub fn resize(&self, dims: Dimensions) {
        nix::ioctl_write_ptr_bad!(tiocswinsz, nix::libc::TIOCSWINSZ, nix::pty::Winsize);
        // SAFETY: the fd is our open master, and the pointer is to a
        // Winsize that lives for the call, which TIOCSWINSZ only reads.
        _ = unsafe { tiocswinsz(self.fd.as_raw_fd(), &dims.to_winsize()) };
    }
}

/// The reader thread: wait for output on the (non-blocking) master, read
/// all of it, and queue it for the frame loop. It ends at end of stream,
/// on an error, or when the Pty is dropped.
///
/// Reads are gathered into one chunk until the master has nothing more
/// (EAGAIN) or the buffer is full. A program writing to a tty often
/// writes a line at a time (seq does), so sending each read() on its own
/// filled the QUEUED_CHUNKS slots with ~170-byte chunks, the reader then
/// waited for the next frame to drain them, and flood throughput tracked
/// the frame rate: coalescing made `seq 1 3000000` about 2x faster in tai
/// and 3x in tai-gui on Linux (popos-tai). The bound stays QUEUED_CHUNKS
/// full buffers, 4 MB.
fn read_loop(fd: &OwnedFd, tx: &mpsc::SyncSender<Chunk>, wake_up: &Wake) {
    let mut buf = vec![0u8; 64 * 1024];
    let send = |chunk: Chunk| {
        let sent = tx.send(chunk).is_ok();
        wake(wake_up);
        sent
    };
    loop {
        let mut fds = [nix::libc::pollfd {
            fd: fd.as_raw_fd(),
            events: nix::libc::POLLIN,
            revents: 0,
        }];
        // Errors other than EINTR show up on the read below.
        // SAFETY: fds is a live array of one pollfd, matching nfds = 1.
        _ = unsafe { nix::libc::poll(fds.as_mut_ptr(), 1, -1) };
        let mut filled = 0;
        loop {
            match unistd::read(fd, &mut buf[filled..]) {
                Ok(len) if len > 0 => {
                    filled += len;
                    if filled == buf.len() {
                        if !send(Ok(buf.clone())) {
                            return;
                        }
                        filled = 0;
                    }
                }
                Err(Errno::EAGAIN) => {
                    if filled > 0 && !send(Ok(buf[..filled].to_vec())) {
                        return;
                    }
                    break;
                }
                Err(Errno::EINTR) => {}
                // EOF -- the child closed its side of the pty. On Linux,
                // the slave closing often produces EIO rather than a
                // clean EOF (read returning 0). Treat it the same. Output
                // gathered before it is sent first.
                end => {
                    if filled > 0 && !send(Ok(buf[..filled].to_vec())) {
                        return;
                    }
                    _ = send(Err(match end {
                        Ok(_) | Err(Errno::EIO) => PtyError::EndOfStream,
                        Err(err) => PtyError::OtherError(err),
                    }));
                    return;
                }
            }
        }
    }
}

/// The master fd for the writer thread. It's non-blocking (the reader
/// shares it), so a write that finds the pty buffer full waits for room
/// instead of failing.
struct MasterWriter(OwnedFd);

impl std::io::Write for MasterWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        loop {
            match unistd::write(&self.0, buf) {
                Ok(len) => return Ok(len),
                Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN) => {
                    let mut fds = [nix::libc::pollfd {
                        fd: self.0.as_raw_fd(),
                        events: nix::libc::POLLOUT,
                        revents: 0,
                    }];
                    // Errors other than EINTR show up on the next write.
                    // SAFETY: fds is a live array of one pollfd, matching nfds = 1.
                    _ = unsafe { nix::libc::poll(fds.as_mut_ptr(), 1, -1) };
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Read,
        time::{Duration, Instant},
    };

    use nix::fcntl::{self, OFlag};

    use super::MasterWriter;
    use crate::pty_input::Input;

    #[test]
    fn a_non_blocking_master_waits_for_room_instead_of_dropping() {
        // A non-blocking write end, like the pty master.
        let (reader, writer) = nix::unistd::pipe().unwrap();
        let flags = fcntl::fcntl(&writer, fcntl::F_GETFL).unwrap();
        let flags = OFlag::from_bits_retain(flags) | OFlag::O_NONBLOCK;
        fcntl::fcntl(&writer, fcntl::F_SETFL(flags)).unwrap();
        let input = Input::spawn(MasterWriter(writer)).unwrap();

        // Far more than the pipe holds, to a reader that starts late.
        let sent: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
        let start = Instant::now();
        for chunk in sent.chunks(4096) {
            input.send(chunk);
        }
        assert!(start.elapsed() < Duration::from_millis(500));

        // All of it arrives, in order, where writing from the frame loop
        // dropped what didn't fit.
        std::thread::sleep(Duration::from_millis(200));
        drop(input);
        let mut got = Vec::new();
        std::fs::File::from(reader).read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), sent.len());
        assert!(got == sent);
    }

    #[test]
    fn a_bare_shell_name_is_found_on_path_before_the_fork() {
        use std::path::Path;
        let found = super::resolve(Path::new("sh"), Some("/nonexistent:/bin".as_ref()));
        assert_eq!(found, Path::new("/bin/sh"));
        // A path with a slash, or a name on no PATH entry, is left as is.
        assert_eq!(
            super::resolve(Path::new("/x/zsh"), None),
            Path::new("/x/zsh")
        );
        assert_eq!(
            super::resolve(Path::new("nosuchshell"), Some("/bin".as_ref())),
            Path::new("nosuchshell")
        );
    }

    #[test]
    fn the_exec_is_built_whole_before_the_fork() {
        use std::{ffi::OsString, path::Path};
        let launch = crate::shell_integration::Launch {
            env: vec![("TERM".into(), "from-integration".into())],
            args: vec!["-i".into()],
            login: true,
        };
        let exec = super::Exec::new(
            Path::new("/bin/sh"),
            &launch,
            Some(Path::new("/")),
            &[OsString::from("PATH")],
            &[("LC_CTYPE", "C.UTF-8".to_string())],
        )
        .unwrap();
        let args: Vec<&str> = exec._args.iter().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(args, ["-sh", "-i"]);
        let env: Vec<&str> = exec._env.iter().map(|s| s.to_str().unwrap()).collect();
        // Later settings replace earlier ones, and dropped names go.
        assert_eq!(env.iter().filter(|e| e.starts_with("TERM=")).count(), 1);
        assert!(env.contains(&"TERM=from-integration"));
        assert!(env.contains(&"COLORTERM=truecolor"));
        assert!(env.contains(&"LC_CTYPE=C.UTF-8"));
        assert!(!env.iter().any(|e| e.starts_with("PATH=")));
        // Both pointer arrays end in null, and point at the strings.
        assert_eq!(exec.argv.len(), 3);
        assert!(exec.argv[2].is_null() && exec.envp[env.len()].is_null());
        assert_eq!(exec.argv[0], exec._args[0].as_ptr());
        assert_eq!(exec.program.to_str().unwrap(), "/bin/sh");
    }
}

/// The child process within a pseudo-terminal.
///
/// The child can be in one of three states: active (default), exited
/// and reaped. The parent process has to wait for the child to fully
/// exit, which is why it tries to "reap" the child process by waiting
/// for it to exit via `waitpid`. Once it has exited fully, it will be
/// considered "reaped".
pub enum Child {
    Active(Pid),
    Exited(Pid),
    Reaped(wait::WaitStatus),
}

impl Child {
    /// Record that the child's side of the pty has closed.
    pub fn mark_exited(&mut self) {
        if let Child::Active(pid) = *self {
            *self = Child::Exited(pid);
        }
    }

    /// Reap an exited child without blocking.
    pub fn try_reap(&mut self) {
        if let Child::Exited(pid) = *self
            && let Ok(wp) = wait::waitpid(pid, Some(wait::WaitPidFlag::WNOHANG))
        {
            *self = Child::Reaped(wp);
        }
    }
}

/// The shell's exec, built before the fork so the child allocates nothing.
struct Exec {
    program: CString,
    cwd: Option<CString>,
    // The pointer arrays point into these; a Vec's heap buffer doesn't
    // move when the Vec does.
    _args: Vec<CString>,
    _env: Vec<CString>,
    argv: Vec<*const c_char>,
    envp: Vec<*const c_char>,
}

impl Exec {
    fn new(
        shell: &Path,
        launch: &crate::shell_integration::Launch,
        cwd: Option<&Path>,
        foreign: &[OsString],
        locale: &[(&'static str, String)],
    ) -> std::io::Result<Self> {
        // tai's environment, less other terminals' identity, then tai's
        // variables, later ones replacing earlier ones as Command::env did.
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(name, _)| !foreign.contains(name))
            .collect();
        let mut set = |name: &OsStr, value: &OsStr| match env.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = value.to_owned(),
            None => env.push((name.to_owned(), value.to_owned())),
        };
        // TERM tells programs what escape sequences we understand.
        set("TERM".as_ref(), "xterm-256color".as_ref());
        set("COLORTERM".as_ref(), "truecolor".as_ref());
        // Identify tai, not the terminal that started it.
        for (name, value) in crate::identity::vars() {
            set(name.as_ref(), value.as_ref());
        }
        // A UTF-8 character type; see src/locale.rs.
        for (name, value) in locale {
            set(name.as_ref(), value.as_ref());
        }
        // The shell integration's, last.
        for (name, value) in &launch.env {
            set(name.as_os_str(), value.as_os_str());
        }

        // Command::new searched PATH for a bare name; execve doesn't.
        let path = env
            .iter()
            .find(|(n, _)| n == "PATH")
            .map(|(_, v)| v.clone());
        let program = resolve(shell, path.as_deref());

        // argv[0] is just the program name ("/bin/zsh" -> "zsh"), "-zsh"
        // for a login shell.
        let mut arg0 = OsString::new();
        if launch.login {
            arg0.push("-");
        }
        arg0.push(shell.file_name().unwrap_or(shell.as_os_str()));

        let c = |s: &OsStr| {
            CString::new(s.as_bytes()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "nul byte in the shell's command or environment",
                )
            })
        };
        let args = std::iter::once(arg0.as_os_str())
            .chain(launch.args.iter().map(OsString::as_os_str))
            .map(c)
            .collect::<std::io::Result<Vec<_>>>()?;
        let env = env
            .iter()
            .map(|(name, value)| {
                let mut pair = name.clone();
                pair.push("=");
                pair.push(value);
                c(&pair)
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let nul_terminated = |v: &[CString]| {
            v.iter()
                .map(|s| s.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect()
        };
        Ok(Self {
            program: c(program.as_os_str())?,
            cwd: cwd.map(|dir| c(dir.as_os_str())).transpose()?,
            argv: nul_terminated(&args),
            envp: nul_terminated(&env),
            _args: args,
            _env: env,
        })
    }

    /// In the forked child: change directory and exec, or exit 127 as
    /// Command::exec's caller did.
    fn run(&self) -> ! {
        // SAFETY: chdir, execve and _exit are async-signal-safe. Every
        // pointer is into a NUL-terminated string or a null-terminated
        // array that `self` owns, built before the fork.
        unsafe {
            if let Some(dir) = &self.cwd
                && nix::libc::chdir(dir.as_ptr()) != 0
            {
                nix::libc::_exit(127);
            }
            nix::libc::execve(
                self.program.as_ptr(),
                self.argv.as_ptr(),
                self.envp.as_ptr(),
            );
            nix::libc::_exit(127)
        }
    }
}

/// `shell` as execve wants it: as given when it has a slash, else the
/// first executable file of that name on `path`, as Command::new did.
fn resolve(shell: &Path, path: Option<&OsStr>) -> PathBuf {
    if shell.as_os_str().as_bytes().contains(&b'/') {
        return shell.to_path_buf();
    }
    std::env::split_paths(path.unwrap_or_default())
        .map(|dir| dir.join(shell))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .unwrap_or_else(|| shell.to_path_buf())
}

/// Close descriptors 3 and up, in a forked child before exec. Only
/// async-signal-safe calls: no allocation.
fn close_inherited_fds() {
    // SAFETY: sysconf and close are async-signal-safe and touch no memory
    // of ours.
    let max = unsafe { nix::libc::sysconf(nix::libc::_SC_OPEN_MAX) };
    let max = if max <= 0 { 1024 } else { max.min(65536) } as i32;
    for fd in 3..max {
        // SAFETY: as above. The child never drops its copies of tai's fd
        // handles: it execs or calls exit, which runs no destructors.
        unsafe { nix::libc::close(fd) };
    }
}

/// How long dropping a child waits for it to exit after SIGHUP, before
/// SIGKILL.
const EXIT_WAIT: Duration = Duration::from_secs(1);

impl Drop for Child {
    fn drop(&mut self) {
        // Ask a running child to exit, then reap it so it isn't left a
        // zombie. Never wait without end: a shell that ignores SIGHUP, or
        // whose pty another process holds open, gets SIGKILL.
        let pid = match *self {
            Child::Active(pid) => {
                let _ = signal::kill(pid, signal::SIGHUP);
                pid
            }
            Child::Exited(pid) => pid,
            Child::Reaped(_) => return,
        };
        let deadline = Instant::now() + EXIT_WAIT;
        loop {
            match wait::waitpid(pid, Some(wait::WaitPidFlag::WNOHANG)) {
                Ok(wait::WaitStatus::StillAlive) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(wait::WaitStatus::StillAlive) => {
                    let _ = signal::kill(pid, signal::SIGKILL);
                    let _ = wait::waitpid(pid, None);
                    return;
                }
                // Reaped, or not ours to wait for any more.
                _ => return,
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum PtyError {
    EndOfStream,
    OtherError(nix::errno::Errno),
}
