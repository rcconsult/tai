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

/// DirectWrite's system font fallback, as Windows Terminal maps characters
/// to fonts (`AtlasEngine::_mapCharacters`, at the commit above): the text
/// is handed to `IDWriteFontFallback::MapCharacters` through a text
/// analysis source that reports the user's default locale, and the font
/// it maps to is traced back to its file.
#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use std::{os::windows::ffi::OsStringExt, path::PathBuf};

    use windows::{
        Win32::{
            Globalization::GetUserDefaultLocaleName,
            Graphics::DirectWrite::{
                DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_WEIGHT_NORMAL, DWRITE_READING_DIRECTION,
                DWRITE_READING_DIRECTION_LEFT_TO_RIGHT, DWriteCreateFactory, IDWriteFactory2,
                IDWriteFont, IDWriteFontCollection, IDWriteFontFallback,
                IDWriteLocalFontFileLoader, IDWriteNumberSubstitution, IDWriteTextAnalysisSource,
                IDWriteTextAnalysisSource_Impl,
            },
        },
        core::{Interface, OutRef, Result, implement, w},
    };

    use super::FontFile;

    /// The text DirectWrite maps: one character, in UTF-16, in one locale.
    #[implement(IDWriteTextAnalysisSource)]
    struct Source {
        text: Vec<u16>,
        /// NUL-terminated.
        locale: Vec<u16>,
    }

    impl Source {
        fn len(&self) -> u32 {
            self.text.len() as u32
        }
    }

    // SAFETY, for every method: DirectWrite passes valid out-pointers, and
    // the pointers handed back point into `self`, which outlives the
    // `MapCharacters` call that reads them.
    impl IDWriteTextAnalysisSource_Impl for Source_Impl {
        fn GetTextAtPosition(&self, at: u32, text: *mut *mut u16, len: *mut u32) -> Result<()> {
            unsafe {
                if at >= self.len() {
                    *text = std::ptr::null_mut();
                    *len = 0;
                } else {
                    *text = self.text.as_ptr().add(at as usize).cast_mut();
                    *len = self.len() - at;
                }
            }
            Ok(())
        }

        fn GetTextBeforePosition(&self, at: u32, text: *mut *mut u16, len: *mut u32) -> Result<()> {
            unsafe {
                if at == 0 || at > self.len() {
                    *text = std::ptr::null_mut();
                    *len = 0;
                } else {
                    *text = self.text.as_ptr().cast_mut();
                    *len = at;
                }
            }
            Ok(())
        }

        fn GetParagraphReadingDirection(&self) -> DWRITE_READING_DIRECTION {
            DWRITE_READING_DIRECTION_LEFT_TO_RIGHT
        }

        fn GetLocaleName(&self, at: u32, len: *mut u32, locale: *mut *mut u16) -> Result<()> {
            unsafe {
                *len = self.len().saturating_sub(at);
                *locale = self.locale.as_ptr().cast_mut();
            }
            Ok(())
        }

        fn GetNumberSubstitution(
            &self,
            at: u32,
            len: *mut u32,
            substitution: OutRef<IDWriteNumberSubstitution>,
        ) -> Result<()> {
            unsafe { *len = self.len().saturating_sub(at) };
            substitution.write(None)
        }
    }

    thread_local! {
        /// The system fallback, made once per thread: DirectWrite's COM
        /// objects aren't `Send`. `None` if DirectWrite couldn't give one.
        static FALLBACK: Option<IDWriteFontFallback> = unsafe {
            DWriteCreateFactory::<IDWriteFactory2>(DWRITE_FACTORY_TYPE_SHARED)
                .and_then(|factory| factory.GetSystemFontFallback())
                .inspect_err(|e| tracing::warn!("DirectWrite font fallback: {e}"))
                .ok()
        };
    }

    /// The user's default locale, such as "cs-CZ", NUL-terminated.
    fn user_locale() -> Vec<u16> {
        // LOCALE_NAME_MAX_LENGTH, including the NUL.
        let mut name = [0u16; 85];
        let len = unsafe { GetUserDefaultLocaleName(&mut name) };
        if len <= 0 {
            return vec![0];
        }
        name[..len as usize].to_vec()
    }

    pub fn find(ch: char) -> Option<FontFile> {
        find_in(ch, user_locale())
    }

    /// The font file DirectWrite maps `ch` to in `locale` (NUL-terminated).
    pub(super) fn find_in(ch: char, locale: Vec<u16>) -> Option<FontFile> {
        let mut utf16 = [0u16; 2];
        let text = ch.encode_utf16(&mut utf16).to_vec();
        let source: IDWriteTextAnalysisSource = Source { text, locale }.into();
        let len = ch.len_utf16() as u32;
        let font = FALLBACK.with(|fallback| {
            let fallback = fallback.as_ref()?;
            let mut mapped_len = 0;
            let mut font: Option<IDWriteFont> = None;
            let mut scale = 0.0;
            unsafe {
                fallback.MapCharacters(
                    &source,
                    0,
                    len,
                    None::<&IDWriteFontCollection>,
                    w!(""),
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    &mut mapped_len,
                    &mut font,
                    &mut scale,
                )
            }
            .inspect_err(|e| tracing::debug!("MapCharacters U+{:04X}: {e}", ch as u32))
            .ok()?;
            font
        })?;
        // MapCharacters falls back to some font even for a character no
        // font has.
        if !unsafe { font.HasCharacter(ch as u32) }.ok()?.as_bool() {
            return None;
        }
        file_of(&font)
    }

    /// The file and face index behind `font`. `None` for a font that isn't
    /// a local file, such as one DirectWrite would download.
    fn file_of(font: &IDWriteFont) -> Option<FontFile> {
        unsafe {
            let face = font.CreateFontFace().ok()?;
            let mut count = 0;
            face.GetFiles(&mut count, None).ok()?;
            if count == 0 {
                return None;
            }
            let mut files = vec![None; count as usize];
            face.GetFiles(&mut count, Some(files.as_mut_ptr())).ok()?;
            let file = files.into_iter().next()??;
            let mut key = std::ptr::null_mut();
            let mut key_size = 0;
            file.GetReferenceKey(&mut key, &mut key_size).ok()?;
            let loader = file
                .GetLoader()
                .ok()?
                .cast::<IDWriteLocalFontFileLoader>()
                .ok()?;
            let len = loader.GetFilePathLengthFromKey(key, key_size).ok()?;
            let mut path = vec![0u16; len as usize + 1];
            loader.GetFilePathFromKey(key, key_size, &mut path).ok()?;
            path.truncate(len as usize);
            Some(FontFile {
                path: PathBuf::from(std::ffi::OsString::from_wide(&path)),
                index: face.GetIndex(),
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::find_in;

        fn locale(name: &str) -> Vec<u16> {
            name.encode_utf16().chain([0]).collect()
        }

        fn file_name(ch: char, loc: &str) -> String {
            let file = find_in(ch, locale(loc)).unwrap_or_else(|| panic!("{ch} in {loc}"));
            let name = file
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_lowercase();
            eprintln!("{ch} {loc}: {name} #{}", file.index);
            name
        }

        /// Han follows the locale; kana and Hangul go to Japanese and
        /// Korean fonts whatever it is. Font names are Windows 11's.
        #[test]
        fn han_follows_the_locale() {
            assert!(file_name('水', "zh-CN").starts_with("msyh"));
            assert!(file_name('水', "zh-TW").starts_with("msjh"));
            assert!(file_name('水', "ja-JP").starts_with("yugoth"));
            assert!(file_name('水', "ko-KR").starts_with("malgun"));
            assert!(file_name('あ', "en-US").starts_with("yugoth"));
            assert!(file_name('한', "en-US").starts_with("malgun"));
        }

        #[test]
        fn nothing_for_a_codepoint_no_font_has() {
            assert_eq!(find_in('\u{10fffd}', locale("en-US")), None);
        }
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
