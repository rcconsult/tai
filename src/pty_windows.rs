//! The pty module on Windows, on ConPTY through `portable-pty` (ADR 0002).
//!
//! It offers the same interface as the Unix `mod pty` in `main.rs`, so the
//! frame loop doesn't change. ConPTY's pipes block, so a reader thread feeds
//! a channel that `read` drains each frame, and a waiter thread records the
//! exit status. ConPTY keeps its output pipe open after the shell exits, so
//! `read` reports end of stream once the waiter has seen the exit.

use std::{
    ffi::OsString,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        mpsc::{self, Receiver, TryRecvError},
    },
};

use libghostty_vt::Terminal;
use portable_pty::{
    ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system,
};

use crate::Dimensions;

type Chunk = std::io::Result<Vec<u8>>;

/// Handle to a pseudo console.
pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    output: Receiver<Chunk>,
    exit: Arc<OnceLock<ExitStatus>>,
}

impl Pty {
    /// Spawn the user's default shell in a new pseudo console.
    ///
    /// The shell is chosen by checking, in order:
    ///   1. pwsh.exe on PATH
    ///   2. %COMSPEC%
    ///   3. cmd.exe
    pub fn new(dims: Dimensions) -> std::io::Result<(Self, Child)> {
        let pair = native_pty_system()
            .openpty(pty_size(dims))
            .map_err(std::io::Error::other)?;

        let mut cmd = CommandBuilder::new(default_shell());
        // portable-pty starts in %USERPROFILE% unless told otherwise; inherit
        // tai's directory as forkpty does on Unix.
        if let Ok(dir) = std::env::current_dir() {
            cmd.cwd(dir);
        }
        cmd.env("COLORTERM", "truecolor");
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(std::io::Error::other)?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(std::io::Error::other)?;
        let writer = pair.master.take_writer().map_err(std::io::Error::other)?;

        let (tx, output) = mpsc::channel::<Chunk>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(len) => {
                        if tx.send(Ok(buf[..len].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });

        let exit = Arc::new(OnceLock::new());
        let killer = child.clone_killer();
        let waiter_exit = Arc::clone(&exit);
        std::thread::spawn(move || {
            if let Ok(status) = child.wait() {
                _ = waiter_exit.set(status);
            }
        });

        let pty = Self {
            master: pair.master,
            writer: Mutex::new(writer),
            output,
            exit: Arc::clone(&exit),
        };
        Ok((pty, Child::Active(Handle { killer, exit })))
    }

    /// Feed all output received so far into the ghostty terminal.
    pub fn read(&self, term: &mut Terminal) -> Result<(), PtyError> {
        loop {
            match self.output.try_recv() {
                Ok(Ok(data)) => term.vt_write(&data),
                Ok(Err(e)) => return Err(PtyError::OtherError(e)),
                Err(TryRecvError::Disconnected) => return Err(PtyError::EndOfStream),
                Err(TryRecvError::Empty) => {
                    return match self.exit.get() {
                        Some(_) => Err(PtyError::EndOfStream),
                        None => Ok(()),
                    };
                }
            }
        }
    }

    /// Best-effort write to the pseudo console. Errors drop the data, as on
    /// Unix.
    pub fn write(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if let Ok(mut writer) = self.writer.lock() {
            _ = writer.write_all(data).and_then(|()| writer.flush());
        }
    }

    /// Resize the pseudo console.
    pub fn resize(&self, dims: Dimensions) {
        _ = self.master.resize(pty_size(dims));
    }
}

/// What the frame loop keeps of a running child.
pub struct Handle {
    killer: Box<dyn ChildKiller + Send + Sync>,
    exit: Arc<OnceLock<ExitStatus>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        // If the child is still running, end it. The waiter thread reaps it.
        if self.exit.get().is_none() {
            _ = self.killer.kill();
        }
    }
}

/// The child process within a pseudo console: active, exited (its output
/// has ended), or reaped (its exit status is known).
pub enum Child {
    Active(Handle),
    Exited(Handle),
    Reaped(ExitStatus),
}

impl Child {
    /// Record that the child's output has ended.
    pub fn mark_exited(&mut self) {
        // Swap in a placeholder to move the handle out of `self`.
        let placeholder = Child::Reaped(ExitStatus::with_exit_code(0));
        *self = match std::mem::replace(self, placeholder) {
            Child::Active(handle) => Child::Exited(handle),
            other => other,
        };
    }

    /// Take the exit status if the waiter thread has seen it.
    pub fn try_reap(&mut self) {
        if let Child::Exited(handle) = self
            && let Some(status) = handle.exit.get()
        {
            *self = Child::Reaped(status.clone());
        }
    }
}

#[derive(Debug)]
pub enum PtyError {
    EndOfStream,
    OtherError(std::io::Error),
}

fn pty_size(dims: Dimensions) -> PtySize {
    let (cols, rows) = dims.grid_size();
    PtySize {
        rows,
        cols,
        pixel_width: (dims.window_width * dims.dpi_scale) as u16,
        pixel_height: (dims.window_height * dims.dpi_scale) as u16,
    }
}

fn default_shell() -> OsString {
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("pwsh.exe").is_file()));
    if on_path {
        return "pwsh.exe".into();
    }
    match std::env::var_os("COMSPEC") {
        Some(comspec) if !comspec.is_empty() => comspec,
        _ => PathBuf::from("cmd.exe").into_os_string(),
    }
}
