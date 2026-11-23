//! `/edit`: ppxai's `open_editor` side effect opens the file in a terminal
//! editor, in a new terminal tab. ppxai's web app opens its own editor
//! panel and VS Code an editor tab; tai is a terminal, so it runs one.
//!
//! The editor is `$VISUAL`, else `$EDITOR`, else Helix (`hx`) on Linux and
//! macOS and Microsoft Edit (`edit`) on Windows. The command is typed into
//! the new terminal's shell and run there, so the shell's own PATH finds
//! the editor; tai's may be shorter (a macOS app started from the Dock).

use std::path::Path;

#[cfg(not(windows))]
const DEFAULT: &str = "hx";
#[cfg(windows)]
const DEFAULT: &str = "edit";

/// The editor command, as the user set it: it may carry arguments
/// (`EDITOR="code -w"`), which the shell splits.
pub fn configured() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT.to_string())
}

/// How an editor takes the place to open at.
#[derive(Debug, PartialEq)]
enum Place {
    /// `file:line:column`: Helix, Microsoft Edit.
    Suffix,
    /// `+line file`: vi and its kin, nano, emacs, micro, kak.
    Plus,
    /// Just the file: an editor tai doesn't know.
    None,
}

fn place(editor: &str) -> Place {
    let program = editor.split_whitespace().next().unwrap_or_default();
    let name = Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match name.as_str() {
        "hx" | "helix" | "edit" | "msedit" => Place::Suffix,
        "vi" | "vim" | "nvim" | "view" | "nano" | "emacs" | "micro" | "kak" => Place::Plus,
        _ => Place::None,
    }
}

/// The line typed into the new terminal: the editor on `path`, at `line`
/// and `column` when the editor takes them, then `exit`, so the tab
/// closes once the editor quits cleanly. When it can't start, the shell's
/// error stays on screen.
pub fn command(editor: &str, path: &str, line: Option<u32>, column: Option<u32>) -> String {
    let file = match (place(editor), line) {
        (Place::Suffix, Some(line)) => match column {
            Some(column) => quote(&format!("{path}:{line}:{column}")),
            None => quote(&format!("{path}:{line}")),
        },
        (Place::Plus, Some(line)) => format!("+{line} {}", quote(path)),
        _ => quote(path),
    };
    format!("{editor} {file} && exit")
}

/// One argument for the shell the terminal runs: single quotes for the
/// Unix shells; on Windows, double quotes, which PowerShell and cmd.exe
/// both take and a Windows path can't contain.
fn quote(arg: &str) -> String {
    if cfg!(windows) {
        format!("\"{arg}\"")
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_editor_gets_the_place_its_way() {
        let q = |s: &str| quote(s);
        assert_eq!(
            command("hx", "/w/a.rs", Some(12), Some(5)),
            format!("hx {} && exit", q("/w/a.rs:12:5"))
        );
        assert_eq!(
            command("/usr/local/bin/hx", "/w/a.rs", Some(12), None),
            format!("/usr/local/bin/hx {} && exit", q("/w/a.rs:12"))
        );
        assert_eq!(
            command("nvim", "/w/a.rs", Some(12), Some(5)),
            format!("nvim +12 {} && exit", q("/w/a.rs"))
        );
        assert_eq!(
            command("edit.exe", "/w/a.rs", Some(3), None),
            format!("edit.exe {} && exit", q("/w/a.rs:3"))
        );
        // An editor tai doesn't know gets just the file, arguments kept.
        assert_eq!(
            command("code -w", "/w/a.rs", Some(12), None),
            format!("code -w {} && exit", q("/w/a.rs"))
        );
        assert_eq!(
            command("hx", "/w/a.rs", None, None),
            format!("hx {} && exit", q("/w/a.rs"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn paths_are_quoted_for_the_shell() {
        assert_eq!(quote("/w/it's a $file"), r"'/w/it'\''s a $file'");
    }
}
