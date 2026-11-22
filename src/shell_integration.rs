//! Shell integration: the shells tai starts report their working
//! directory to the terminal with OSC 7, at each prompt and on each `cd`,
//! so the app can read it (`Terminal::pwd`). Neither zsh nor bash does
//! that by default.
//!
//! The technique is Ghostty's (`src/shell-integration/README.md` at
//! 108cacb1): zsh starts with `ZDOTDIR` pointing at a directory whose
//! `.zshenv` puts the user's `ZDOTDIR` back and adds a hook; bash starts
//! with its own rc file. The scripts in `shell-integration/` are tai's
//! own: Ghostty's (from Kitty) are GPLv3.
//!
//! `TAI_SHELL_INTEGRATION=0` turns it off. Other shells start as before.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

const ZSHENV: &str = include_str!("../shell-integration/zsh/.zshenv");
const BASH_RC: &str = include_str!("../shell-integration/bash/tai.bash");

/// How to start a shell so it reports its working directory.
#[derive(Debug, Default, PartialEq)]
pub struct Launch {
    pub env: Vec<(OsString, OsString)>,
    pub args: Vec<OsString>,
    /// Start it as a login shell (argv[0] "-name").
    pub login: bool,
}

/// The way to start `shell`, which tai would start as a login shell when
/// `login` is set. The scripts are written to tai's cache folder first;
/// when that fails, the shell starts as before.
pub fn launch(shell: &Path, login: bool) -> Launch {
    let plain = Launch {
        login,
        ..Launch::default()
    };
    if std::env::var_os("TAI_SHELL_INTEGRATION").is_some_and(|v| v == "0") {
        return plain;
    }
    let Some(dir) = scripts_dir() else {
        return plain;
    };
    let name = shell
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let result = match name.as_str() {
        "zsh" => write(&dir.join("zsh"), ".zshenv", ZSHENV).map(|zdotdir| {
            let mut env = vec![(OsString::from("ZDOTDIR"), zdotdir.into_os_string())];
            if let Some(user) = std::env::var_os("ZDOTDIR") {
                env.push(("TAI_ZSH_ZDOTDIR".into(), user));
            }
            Launch {
                env,
                args: Vec::new(),
                login,
            }
        }),
        // A login bash ignores --rcfile, so the rc file reads the login
        // files itself.
        "bash" => write(&dir.join("bash"), "tai.bash", BASH_RC).map(|rc| Launch {
            env: if login {
                vec![("TAI_BASH_LOGIN".into(), "1".into())]
            } else {
                Vec::new()
            },
            args: vec!["--rcfile".into(), rc.join("tai.bash").into_os_string()],
            login: false,
        }),
        _ => return plain,
    };
    result.unwrap_or_else(|e| {
        tracing::warn!("shell integration: {e}; starting {name} without it");
        plain
    })
}

/// The user's own cache folder for tai's scripts: never a shared temp
/// folder, where another user could plant a script.
fn scripts_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty());
    let cache = if cfg!(target_os = "macos") {
        PathBuf::from(home?).join("Library/Caches")
    } else {
        match std::env::var_os("XDG_CACHE_HOME").filter(|d| Path::new(d).is_absolute()) {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(home?).join(".cache"),
        }
    };
    Some(cache.join("tai/shell-integration"))
}

/// Write `contents` to `dir/name` when it differs, making `dir` private
/// to the user. Returns `dir`.
#[cfg(unix)]
fn write(dir: &Path, name: &str, contents: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let path = dir.join(name);
    if std::fs::read(&path).ok().as_deref() != Some(contents.as_bytes()) {
        let staged = dir.join(format!(".{name}.{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&staged)?;
        std::io::Write::write_all(&mut file, contents.as_bytes())?;
        std::fs::rename(&staged, &path)?;
    }
    Ok(dir.to_path_buf())
}

#[cfg(not(unix))]
fn write(_dir: &Path, _name: &str, _contents: &str) -> std::io::Result<PathBuf> {
    Err(std::io::Error::other("not on this platform"))
}

/// The folder a shell reported, as libghostty-vt keeps it: from OSC 7,
/// a `kitty-shell-cwd://host/path` URL with the path as is (tai's scripts
/// send these) or a `file://host/path` URL percent-encoded; from iTerm2's
/// `OSC 1337;CurrentDir=` or ConEmu's `OSC 9;9`, a bare path (Ghostty
/// `src/terminal/osc/parsers/iterm2.zig` and `osc9.zig` at 108cacb1).
/// iTerm2's shell integration, which some users load, sends its report
/// after tai's. None for anything else, or a path that isn't absolute.
pub fn reported_dir(pwd: &[u8]) -> Option<PathBuf> {
    let text = std::str::from_utf8(pwd).ok()?;
    let (raw, rest) = if let Some(rest) = text.strip_prefix("kitty-shell-cwd://") {
        (true, rest)
    } else if let Some(rest) = text.strip_prefix("file://") {
        (false, rest)
    } else {
        let path = PathBuf::from(text);
        return path.is_absolute().then_some(path);
    };
    // Skip the host: the path starts at the first slash after it.
    let path = &rest[rest.find('/')?..];
    let path = if raw {
        path.to_string()
    } else {
        percent_decode(path)?
    };
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reported_folder_is_read_from_either_url() {
        assert_eq!(
            reported_dir(b"kitty-shell-cwd://host/tmp/a b%20c"),
            Some(PathBuf::from("/tmp/a b%20c"))
        );
        assert_eq!(
            reported_dir(b"file://host/tmp/a%20b"),
            Some(PathBuf::from("/tmp/a b"))
        );
        assert_eq!(reported_dir(b"file:///srv"), Some(PathBuf::from("/srv")));
        assert_eq!(reported_dir(b"file://host"), None);
        assert_eq!(reported_dir(b"file://host/bad%2"), None);
        // iTerm2's CurrentDir and ConEmu's OSC 9;9 carry a bare path.
        assert_eq!(
            reported_dir(b"/usr/local"),
            Some(PathBuf::from("/usr/local"))
        );
        assert_eq!(reported_dir(b"relative/dir"), None);
        assert_eq!(reported_dir(b"https://host/x"), None);
        assert_eq!(reported_dir(b""), None);
    }

    /// The scripts must parse: a syntax error would break the user's shell.
    #[cfg(unix)]
    #[test]
    fn the_scripts_parse() {
        for (shell, script) in [("zsh", ZSHENV), ("bash", BASH_RC)] {
            let Ok(out) = std::process::Command::new(shell)
                .args(["-n", "-c", script])
                .output()
            else {
                eprintln!("{shell} not found; not checked");
                continue;
            };
            assert!(
                out.status.success(),
                "{shell}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
