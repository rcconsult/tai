// Unfortunately, there will be some unsafe shenanigans here.
#![allow(unsafe_code)]
use std::{
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    path::PathBuf,
    process::Command,
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

        // forkpty() combines openpty + fork + login_tty into one call.
        // In the child it sets up the slave side as stdin/stdout/stderr.
        match unsafe { nix::pty::forkpty(&dims.to_winsize(), None)? } {
            // Child process -- replace ourselves with the shell.
            // TERM tells programs what escape sequences we understand.
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
                // anything in the shell.
                for sig in [
                    signal::Signal::SIGINT,
                    signal::Signal::SIGQUIT,
                    signal::Signal::SIGTSTP,
                    signal::Signal::SIGTTIN,
                    signal::Signal::SIGTTOU,
                    signal::Signal::SIGPIPE,
                    signal::Signal::SIGCHLD,
                ] {
                    _ = unsafe { signal::signal(sig, signal::SigHandler::SigDfl) };
                }
                _ = signal::sigprocmask(
                    signal::SigmaskHow::SIG_SETMASK,
                    Some(&signal::SigSet::empty()),
                    None,
                );

                // Extract just the program name for argv[0] (e.g. "/bin/zsh" -> "zsh").
                let name = shell.file_name().unwrap_or(shell.as_os_str());

                // "-zsh" for a login shell.
                let mut arg0 = std::ffi::OsString::new();
                if launch.login {
                    arg0.push("-");
                }
                arg0.push(name);

                // Replace the child process with the user's shell via `exec`.
                let mut command = Command::new(&shell);
                if let Some(dir) = cwd {
                    command.current_dir(dir);
                }
                for name in &foreign {
                    command.env_remove(name);
                }
                _ = command
                    .env("TERM", "xterm-256color")
                    .env("COLORTERM", "truecolor")
                    // Identify tai, not the terminal that started it.
                    .envs(crate::identity::vars())
                    // A UTF-8 character type; see src/locale.rs.
                    .envs(locale.iter().map(|(k, v)| (k, v)))
                    // The shell integration's, last.
                    .envs(launch.env.iter().map(|(k, v)| (k, v)))
                    .args(&launch.args)
                    .arg0(arg0)
                    .exec();

                // `exec` only returns on error.
                std::process::exit(127);
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

/// Close descriptors 3 and up, in a forked child before exec. Only
/// async-signal-safe calls: no allocation.
fn close_inherited_fds() {
    // SAFETY: sysconf and close are async-signal-safe and touch no memory
    // of ours.
    let max = unsafe { nix::libc::sysconf(nix::libc::_SC_OPEN_MAX) };
    let max = if max <= 0 { 1024 } else { max.min(65536) } as i32;
    for fd in 3..max {
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
