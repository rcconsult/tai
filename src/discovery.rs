//! Asking the OS for a font that has a codepoint tai's bundled fonts lack
//! (ADR 0012, decisions 1 and 2).
//!
//! As Ghostty's `discoverFallback` (`src/font/discovery.zig` at a887df42
//! "terminal: bound page map probe lengths without tombstones (#13294)"),
//! each platform asks its own font fallback, with the user's locale, so
//! unified Han codepoints get the forms the user's language expects:
//!
//! - Windows: DirectWrite `IDWriteFontFallback::MapCharacters` with
//!   `GetUserDefaultLocaleName`, as Windows Terminal does
//!   (`src/renderer/atlas/AtlasEngine.cpp`, `_mapCharacters`, at
//!   703ef7f969 "Settings: make clickable SettingsCards support UIA Invoke
//!   (#20871)").
//! - macOS: CoreText `CTFontCreateForString`, through `objc2-core-text`.
//! - Linux: fontconfig, with the codepoint in the charset and the locale's
//!   language in `:lang`, using the face it returns as is.
//!
//! The answer is a font file and a face index, which `fonts.rs` reads with
//! skrifa. A platform without a backend yet finds nothing, and the cell
//! keeps the primary font's missing-glyph box.

// Wired into fonts.rs in step 2 of ADR 0012.
#![allow(dead_code)]

use std::path::PathBuf;

/// A face in a font file on disk.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FontFile {
    pub path: PathBuf,
    /// The face's index in a collection (`.ttc`); 0 for a single font.
    pub index: u32,
}

/// A font file with a glyph for `ch`, as the OS's own fallback picks it
/// for the user's locale. `None` when the OS has none, or when this
/// platform has no backend yet. It may be slow (a system query), so the
/// caller remembers the answer, `None` included.
pub fn find(ch: char) -> Option<FontFile> {
    platform::find(ch)
}

#[cfg(windows)]
mod platform {
    use super::FontFile;

    /// DirectWrite. Being written by the Windows session.
    pub fn find(_ch: char) -> Option<FontFile> {
        None
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::FontFile;

    /// CoreText. For the macOS session.
    pub fn find(_ch: char) -> Option<FontFile> {
        None
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::FontFile;

    /// fontconfig. For the Linux session.
    pub fn find(_ch: char) -> Option<FontFile> {
        None
    }
}
