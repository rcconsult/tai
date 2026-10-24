//! The locale tai gives its children (ADR 0003, decision 2).
//!
//! tai only speaks UTF-8, so a child must run with a UTF-8 character type.
//! A tai started from Finder or the Dock gets no locale variables at all,
//! and a user may have chosen a locale that isn't UTF-8. This works out
//! which variables to set on the child, changing only `LC_CTYPE` where it
//! can so the user's number, date and collation formats stay as they are.
//!
//! Ported from Ghostty's `src/os/locale.zig` (`ensureLocale` and
//! `setLangFromCocoa`; ghostty-org/ghostty a887df42 "terminal: bound page
//! map probe lengths without tombstones (#13294)"). Like Ghostty it builds
//! `LANG` from the macOS system locale when none is set, and validates
//! every candidate, because a value such as `en_CH.UTF-8` can name a
//! locale macOS doesn't have. Unlike Ghostty it never calls `setlocale`
//! on tai itself: it checks candidates with `newlocale` and changes only
//! the child's environment.
//!
//! Windows needs none of this (ConPTY always delivers UTF-8), but the
//! decision logic and its tests are built there too.
#![cfg_attr(not(unix), allow(dead_code))]

/// What to do about the child's locale.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// The inherited locale is UTF-8 already.
    Keep,
    /// Set these variables on the child.
    Set(Vec<(&'static str, String)>),
    /// Nothing usable to set; log why.
    Warn(String),
}

/// UTF-8 locales to fall back on, most neutral first.
const FALLBACKS: [&str; 3] = ["C.UTF-8", "en_US.UTF-8", "UTF-8"];

/// Decide the child's locale from the environment (`env`), the system
/// locale on macOS (`system`, such as "en_CH"), and which locale names the
/// system has (`valid`).
pub fn decide(
    env: impl Fn(&str) -> Option<String>,
    system: impl FnOnce() -> Option<String>,
    valid: impl Fn(&str) -> bool,
) -> Decision {
    let set = |name: &str| env(name).filter(|v| !v.is_empty());

    // LC_ALL overrides every category. The user chose it explicitly, and
    // overriding it would change their formats too, so only warn.
    if let Some(all) = set("LC_ALL") {
        return if is_utf8(&all) && valid(&all) {
            Decision::Keep
        } else {
            Decision::Warn(format!(
                "LC_ALL={all} is not a usable UTF-8 locale; programs in tai may show wrong characters"
            ))
        };
    }

    match set("LC_CTYPE").or_else(|| set("LANG")) {
        Some(ctype) if is_utf8(&ctype) && valid(&ctype) => Decision::Keep,
        // A non-UTF-8 or missing locale: prefer the UTF-8 form of the same
        // one, so its language still applies, and set only LC_CTYPE.
        Some(ctype) => {
            let same = format!("{}.UTF-8", base(&ctype));
            first_valid(std::iter::once(same.as_str()).chain(FALLBACKS), &valid)
                .map(|name| Decision::Set(vec![("LC_CTYPE", name)]))
                .unwrap_or_else(|| no_utf8_locale(&ctype))
        }
        // Nothing set, as when started from Finder or the Dock. On macOS,
        // build LANG from the system locale as Ghostty does. Elsewhere
        // there is no system locale to ask, so set only LC_CTYPE.
        None => match system() {
            Some(sys) => {
                let from_system = format!("{}.UTF-8", base(&sys));
                first_valid(
                    std::iter::once(from_system.as_str()).chain(FALLBACKS),
                    &valid,
                )
                .map(|name| Decision::Set(vec![("LANG", name)]))
                .unwrap_or_else(|| no_utf8_locale(&sys))
            }
            None => first_valid(FALLBACKS, &valid)
                .map(|name| Decision::Set(vec![("LC_CTYPE", name)]))
                .unwrap_or_else(|| no_utf8_locale("(none)")),
        },
    }
}

fn is_utf8(locale: &str) -> bool {
    let lower = locale.to_ascii_lowercase();
    lower.contains("utf-8") || lower.contains("utf8")
}

/// "de_CH.ISO8859-1" and "en_CH@currency=CHF" give "de_CH" and "en_CH".
fn base(locale: &str) -> &str {
    locale.split(['.', '@']).next().unwrap_or(locale)
}

fn first_valid<'a>(
    names: impl IntoIterator<Item = &'a str>,
    valid: &impl Fn(&str) -> bool,
) -> Option<String> {
    names
        .into_iter()
        .find(|name| valid(name))
        .map(str::to_owned)
}

fn no_utf8_locale(had: &str) -> Decision {
    Decision::Warn(format!(
        "no UTF-8 locale available (had {had}); programs in tai may show wrong characters"
    ))
}

/// The variables to set on a child of tai, logging the decision.
#[cfg(unix)]
pub fn child_env() -> Vec<(&'static str, String)> {
    let decision = decide(|name| std::env::var(name).ok(), system_locale, is_available);
    match decision {
        Decision::Keep => Vec::new(),
        Decision::Set(vars) => {
            tracing::info!("child locale: {vars:?}");
            vars
        }
        Decision::Warn(why) => {
            tracing::warn!("{why}");
            Vec::new()
        }
    }
}

/// Whether the system has this locale, checked without changing tai's own.
#[cfg(unix)]
fn is_available(name: &str) -> bool {
    use nix::libc;
    let Ok(cname) = std::ffi::CString::new(name) else {
        return false;
    };
    // SAFETY: cname is a valid C string. newlocale only reads it, and the
    // locale it returns is freed right away.
    #[allow(unsafe_code)]
    unsafe {
        let loc = libc::newlocale(libc::LC_CTYPE_MASK, cname.as_ptr(), std::ptr::null_mut());
        if loc.is_null() {
            return false;
        }
        libc::freelocale(loc);
    }
    true
}

/// The macOS system locale, such as "en_CH". Ghostty asks NSLocale; tai
/// has no Objective-C binding, so it asks `defaults`, which reads the same
/// preference. Only consulted when no locale variable is set.
#[cfg(target_os = "macos")]
fn system_locale() -> Option<String> {
    let out = std::process::Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()?;
    let locale = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.status.success() && !locale.is_empty()).then_some(locale)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn system_locale() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::{Decision, decide};

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn has<'a>(names: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |name| names.contains(&name)
    }

    #[test]
    fn utf8_locale_is_kept() {
        let d = decide(
            env(&[("LANG", "en_US.UTF-8")]),
            || None,
            has(&["en_US.UTF-8"]),
        );
        assert_eq!(d, Decision::Keep);
        let d = decide(
            env(&[("LC_CTYPE", "fr_CH.utf8")]),
            || None,
            has(&["fr_CH.utf8"]),
        );
        assert_eq!(d, Decision::Keep);
    }

    #[test]
    fn no_locale_on_linux_sets_only_lc_ctype() {
        let d = decide(env(&[]), || None, has(&["C.UTF-8", "en_US.UTF-8"]));
        assert_eq!(d, Decision::Set(vec![("LC_CTYPE", "C.UTF-8".into())]));
    }

    #[test]
    fn no_locale_on_macos_uses_the_system_locale() {
        let d = decide(
            env(&[]),
            || Some("de_CH".into()),
            has(&["de_CH.UTF-8", "en_US.UTF-8"]),
        );
        assert_eq!(d, Decision::Set(vec![("LANG", "de_CH.UTF-8".into())]));
    }

    #[test]
    fn missing_system_locale_falls_back() {
        // macOS 12 has no en_CH locale, so en_CH.UTF-8 is not usable.
        let d = decide(
            env(&[]),
            || Some("en_CH@currency=CHF".into()),
            has(&["en_US.UTF-8"]),
        );
        assert_eq!(d, Decision::Set(vec![("LANG", "en_US.UTF-8".into())]));
    }

    #[test]
    fn non_utf8_locale_gets_its_utf8_form_in_lc_ctype() {
        let d = decide(
            env(&[("LANG", "de_CH.ISO8859-1")]),
            || None,
            has(&["de_CH.UTF-8"]),
        );
        assert_eq!(d, Decision::Set(vec![("LC_CTYPE", "de_CH.UTF-8".into())]));
        let d = decide(env(&[("LANG", "C")]), || None, has(&["C.UTF-8"]));
        assert_eq!(d, Decision::Set(vec![("LC_CTYPE", "C.UTF-8".into())]));
    }

    #[test]
    fn invalid_utf8_locale_is_replaced() {
        let d = decide(env(&[("LANG", "en_CH.UTF-8")]), || None, has(&["C.UTF-8"]));
        assert_eq!(d, Decision::Set(vec![("LC_CTYPE", "C.UTF-8".into())]));
    }

    #[test]
    fn non_utf8_lc_all_only_warns() {
        let d = decide(
            env(&[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")]),
            || None,
            has(&["C.UTF-8"]),
        );
        assert!(matches!(d, Decision::Warn(_)));
        let d = decide(
            env(&[("LC_ALL", "en_US.UTF-8")]),
            || None,
            has(&["en_US.UTF-8"]),
        );
        assert_eq!(d, Decision::Keep);
    }

    #[test]
    fn nothing_available_warns() {
        let d = decide(env(&[]), || None, has(&[]));
        assert!(matches!(d, Decision::Warn(_)));
    }
}
