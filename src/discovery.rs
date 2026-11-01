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
    // CoreText is a C API.
    #![allow(unsafe_code)]

    use super::FontFile;
    use objc2_core_foundation::{CFArray, CFRange, CFRetained, CFString, CFURL};
    use objc2_core_text::{
        CTFont, CTFontDescriptor, CTFontManagerCreateFontDescriptorsFromURL, kCTFontNameAttribute,
        kCTFontURLAttribute,
    };

    /// CoreText's own fallback for `ch`, as Ghostty's `discoverCodepoint`
    /// (`src/font/discovery.zig` at a887df42): `CTFontCreateForString`,
    /// which follows the user's preferred languages for unified Han.
    /// Ghostty starts from the user's font; tai's fonts aren't registered
    /// with CoreText, so it starts from Menlo, the system monospace font.
    pub fn find(ch: char) -> Option<FontFile> {
        let mut buf = [0u8; 4];
        let string = CFString::from_str(ch.encode_utf8(&mut buf));
        let range = CFRange {
            location: 0,
            // The range counts UTF-16 units.
            length: ch.len_utf16() as isize,
        };
        // SAFETY: plain CoreText calls on objects that live for the call.
        let font = unsafe {
            let base =
                CTFont::with_name(&CFString::from_static_str("Menlo"), 12.0, std::ptr::null());
            base.for_string(&string, range)
        };
        // SAFETY: as above.
        let name = unsafe { font.post_script_name() }.to_string();
        // CoreText's font of last resort only has replacement glyphs.
        if name == "LastResort" {
            return None;
        }
        // SAFETY: as above.
        let url = unsafe { font.attribute(kCTFontURLAttribute) }?
            .downcast::<CFURL>()
            .ok()?;
        let path = url.to_file_path()?;
        let index = face_index(&url, &name)?;
        Some(FontFile { path, index })
    }

    /// The index of the face named `name` (PostScript) in the file at
    /// `url`. CoreText lists a collection's faces in file order.
    fn face_index(url: &CFURL, name: &str) -> Option<u32> {
        // SAFETY: as above; the array holds font descriptors.
        let faces: CFRetained<CFArray<CTFontDescriptor>> =
            unsafe { CFRetained::cast_unchecked(CTFontManagerCreateFontDescriptorsFromURL(url)?) };
        faces
            .iter()
            .position(|face| {
                // SAFETY: as above.
                unsafe { face.attribute(kCTFontNameAttribute) }
                    .and_then(|n| n.downcast::<CFString>().ok())
                    .is_some_and(|n| n.to_string() == name)
            })
            .map(|i| i as u32)
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

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::find;
    use skrifa::{FontRef, MetadataProvider};

    /// Every Mac has CJK, Hangul and Thai faces, some in collections; the
    /// answer must open with skrifa at its index and map the codepoint.
    #[test]
    fn coretext_finds_a_face_that_has_the_codepoint() {
        for ch in ['水', '한', 'ก', 'あ'] {
            let file = find(ch).unwrap_or_else(|| panic!("no font for {ch}"));
            let data = std::fs::read(&file.path).unwrap();
            let font = FontRef::from_index(&data, file.index).unwrap();
            assert!(
                font.charmap().map(ch).is_some(),
                "{ch}: {file:?} has no glyph"
            );
            eprintln!("{ch}: {file:?}");
        }
    }

    #[test]
    fn coretext_last_resort_is_no_answer() {
        // A private-use codepoint no system font has.
        assert_eq!(find('\u{10FFFD}'), None);
    }
}
