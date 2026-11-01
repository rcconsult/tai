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
    // fontconfig is a C library.
    #![allow(unsafe_code)]

    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    use fontconfig_sys::constants::{
        FC_CHARSET, FC_FILE, FC_INDEX, FC_LANG, FC_MONO, FC_OUTLINE, FC_SPACING,
    };
    use fontconfig_sys::*;

    use super::FontFile;

    /// fontconfig's configuration, loaded once and kept for the process.
    /// The mutex is because fontconfig only became thread-safe in 2.10,
    /// and lookups are rare (once per missing codepoint).
    struct Config(*mut FcConfig);

    // Public since fontconfig 2.9.91 but not in fontconfig-sys. The set is
    // fontconfig's own, built once and never freed.
    unsafe extern "C" {
        fn FcGetDefaultLangs() -> *mut FcStrSet;
    }

    // SAFETY: the pointer is only used with the mutex held.
    unsafe impl Send for Config {}

    fn config() -> Option<&'static Mutex<Config>> {
        static CONFIG: OnceLock<Option<Mutex<Config>>> = OnceLock::new();
        CONFIG
            .get_or_init(|| {
                // SAFETY: plain fontconfig initialisation; null on failure.
                let config = unsafe { FcInitLoadConfigAndFonts() };
                if config.is_null() {
                    tracing::warn!("fontconfig: no configuration; no system font fallback");
                    return None;
                }
                Some(Mutex::new(Config(config)))
            })
            .as_ref()
    }

    /// As Ghostty's `Fontconfig.discover` and `CodepointResolver.getIndex`
    /// (`src/font/discovery.zig`, `src/font/CodepointResolver.zig` at
    /// a887df42): a pattern with the codepoint in its charset and
    /// monospace preferred, the config's and the default substitutions,
    /// then the first sorted font whose charset really has the codepoint.
    ///
    /// Unlike Ghostty, the locale's languages (`FcGetDefaultLangs`: from
    /// `FC_LANG`, else `LC_ALL`, `LC_CTYPE` or `LANG`) go into the pattern
    /// as `:lang` before `FcConfigSubstitute`. `FcDefaultSubstitute` adds
    /// them too, but after the config has run, so distro rules keyed on the
    /// language (Ubuntu's `69-language-selector-zh-cn.conf` and the like)
    /// never see them: with `LANG=zh_CN.UTF-8`, 水 came from the JP face of
    /// Noto Sans CJK, as `fc-match` gives too. With the language in the
    /// pattern it comes from the SC face. The face returned is used as is.
    pub fn find(ch: char) -> Option<FontFile> {
        let config = config()?.lock().ok()?;
        // SAFETY: every object created here is destroyed before return,
        // and strings read from a pattern are copied out before the
        // pattern is destroyed.
        unsafe {
            let pattern = FcPatternCreate();
            if pattern.is_null() {
                return None;
            }
            let charset = FcCharSetCreate();
            if !charset.is_null() {
                FcCharSetAddChar(charset, ch as FcChar32);
                FcPatternAddCharSet(pattern, FC_CHARSET.as_ptr(), charset);
                FcCharSetDestroy(charset);
            }
            FcPatternAddInteger(pattern, FC_SPACING.as_ptr(), FC_MONO);
            let langs = FcStrListCreate(FcGetDefaultLangs());
            if !langs.is_null() {
                loop {
                    let lang = FcStrListNext(langs);
                    if lang.is_null() {
                        break;
                    }
                    FcPatternAddString(pattern, FC_LANG.as_ptr(), lang);
                }
                FcStrListDone(langs);
            }
            FcConfigSubstitute(config.0, pattern, FcMatchPattern);
            FcDefaultSubstitute(pattern);

            let mut result = FcResultNoMatch;
            let set = FcFontSort(config.0, pattern, 0, std::ptr::null_mut(), &mut result);
            let found = if set.is_null() || result != FcResultMatch {
                None
            } else {
                let fonts = std::slice::from_raw_parts((*set).fonts, (*set).nfont as usize);
                fonts.iter().find_map(|&font| {
                    let prepared = FcFontRenderPrepare(config.0, pattern, font);
                    if prepared.is_null() {
                        return None;
                    }
                    let file = usable(prepared, ch);
                    FcPatternDestroy(prepared);
                    file
                })
            };
            if !set.is_null() {
                FcFontSetDestroy(set);
            }
            FcPatternDestroy(pattern);
            match &found {
                Some(file) => tracing::info!(
                    "fontconfig: U+{:04X} from {} (face {})",
                    ch as u32,
                    file.path.display(),
                    file.index
                ),
                None => tracing::info!("fontconfig: no font for U+{:04X}", ch as u32),
            }
            found
        }
    }

    /// The font file for `font` when it can draw `ch`: its charset has the
    /// codepoint, it has outlines (skrifa doesn't read bitmap-only fonts),
    /// and it isn't a colour emoji font (Ghostty's text-presentation check
    /// via the `und-zsye` language). tai's bundled Noto Emoji covers emoji.
    ///
    /// SAFETY: `font` must be a valid pattern.
    unsafe fn usable(font: *mut FcPattern, ch: char) -> Option<FontFile> {
        unsafe {
            let mut charset = std::ptr::null_mut();
            if FcPatternGetCharSet(font, FC_CHARSET.as_ptr(), 0, &mut charset) != FcResultMatch
                || FcCharSetHasChar(charset, ch as FcChar32) == 0
            {
                return None;
            }
            let mut outline = 0;
            if FcPatternGetBool(font, FC_OUTLINE.as_ptr(), 0, &mut outline) == FcResultMatch
                && outline == 0
            {
                return None;
            }
            let mut langs = std::ptr::null_mut();
            if FcPatternGetLangSet(font, FC_LANG.as_ptr(), 0, &mut langs) == FcResultMatch
                && FcLangSetHasLang(langs, c"und-zsye".as_ptr().cast()) == FcLangEqual
            {
                return None;
            }
            let mut file = std::ptr::null_mut();
            if FcPatternGetString(font, FC_FILE.as_ptr(), 0, &mut file) != FcResultMatch {
                return None;
            }
            let path = PathBuf::from(OsStr::from_bytes(CStr::from_ptr(file.cast()).to_bytes()));
            let mut index = 0;
            FcPatternGetInteger(font, FC_INDEX.as_ptr(), 0, &mut index);
            // The high 16 bits name a variable font's named instance; the
            // face in the collection is the low 16.
            Some(FontFile {
                path,
                index: index as u32 & 0xFFFF,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use skrifa::{FontRef, MetadataProvider};

    use super::*;

    /// Whatever the OS answers must be a face skrifa opens and that maps
    /// the codepoint. A host without such a font may answer `None`.
    #[test]
    fn a_found_face_has_the_codepoint() {
        for ch in ['A', '水', 'あ', '한'] {
            let Some(file) = find(ch) else { continue };
            let bytes = std::fs::read(&file.path).unwrap();
            let font = FontRef::from_index(&bytes, file.index).unwrap();
            assert!(font.charmap().map(ch).is_some(), "{ch:?} in {file:?}");
        }
    }
}
