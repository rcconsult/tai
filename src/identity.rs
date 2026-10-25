//! What tai tells its child about the terminal it runs in (ADR 0005,
//! decision 1).
//!
//! tai always identifies itself with `TERM_PROGRAM` and
//! `TERM_PROGRAM_VERSION`, and removes the identity variables of the
//! terminal or multiplexer that started it. Otherwise programs inside tai
//! believe they're in iTerm2, Windows Terminal or tmux, and send that
//! terminal's escape sequences (iTerm2's OSC 1337 images, Sixel by
//! `WT_SESSION`), which tai doesn't speak.
//!
//! Ghostty sets `TERM_PROGRAM` and `TERM_PROGRAM_VERSION` and removes only
//! `VTE_VERSION` (`src/termio/Exec.zig`, ghostty-org/ghostty a887df42
//! "terminal: bound page map probe lengths without tombstones (#13294)").
//! tai removes more, because it's a new terminal, not a pane of whatever
//! started it.

use std::ffi::{OsStr, OsString};

/// Variables tai sets on its child, overwriting inherited values.
pub fn vars() -> [(&'static str, &'static str); 2] {
    [
        ("TERM_PROGRAM", "tai"),
        ("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION")),
    ]
}

/// Single variables that name another terminal or multiplexer.
const FOREIGN: [&str; 13] = [
    "VTE_VERSION",
    "TMUX",
    "TMUX_PANE",
    "STY",
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "WT_SESSION",
    "WT_PROFILE_ID",
    "TERM_SESSION_ID",
    "ITERM_SESSION_ID",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "TERMINAL_EMULATOR",
];

/// Prefixes of whole families of another terminal's variables.
const FOREIGN_PREFIXES: [&str; 4] = ["WEZTERM_", "KONSOLE_", "GNOME_TERMINAL_", "ALACRITTY_"];

fn is_foreign(name: &OsStr) -> bool {
    // Windows variable names are case-insensitive.
    let name = name.to_string_lossy().to_ascii_uppercase();
    FOREIGN.contains(&name.as_str()) || FOREIGN_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The inherited variables to remove from the child's environment.
pub fn foreign_vars(env: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<OsString> {
    env.into_iter()
        .map(|(name, _)| name)
        .filter(|name| is_foreign(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{foreign_vars, vars};
    use std::ffi::OsString;

    fn env(names: &[&str]) -> Vec<(OsString, OsString)> {
        names.iter().map(|n| (n.into(), "x".into())).collect()
    }

    #[test]
    fn identifies_tai() {
        let vars = vars();
        assert_eq!(vars[0], ("TERM_PROGRAM", "tai"));
        assert_eq!(vars[1].0, "TERM_PROGRAM_VERSION");
        assert!(!vars[1].1.is_empty());
    }

    #[test]
    fn removes_other_terminals_identity() {
        let removed = foreign_vars(env(&[
            "PATH",
            "WT_SESSION",
            "TERM_PROGRAM",
            "TMUX",
            "WEZTERM_PANE",
            "LC_TERMINAL",
            "LC_ALL",
            "KONSOLE_VERSION",
            "HOME",
        ]));
        assert_eq!(
            removed,
            [
                "WT_SESSION",
                "TMUX",
                "WEZTERM_PANE",
                "LC_TERMINAL",
                "KONSOLE_VERSION"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn matches_windows_names_in_any_case() {
        assert_eq!(
            foreign_vars(env(&["Wt_Session"])),
            [OsString::from("Wt_Session")]
        );
    }
}
