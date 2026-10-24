# ADR 0003: UTF-8 everywhere, with the locale sensed per OS

- **Status:** Proposed 2026-10-24. Not accepted, not implemented.
- **Deciders:** owner. Written by the Claude session on the Windows dev host.
- **Related:** [ADR 0002](0002-windows-11-support.md) (Windows), popos-tai's
  `fonts-fallback` branch (a fallback font for missing glyphs).

## Context

tai has to show text correctly whatever the OS and whatever locale the user
has picked. The first Windows run showed a missing glyph, not an encoding
error, and that is the shape of the problem.

**What was observed on Windows 11 (2026-10-24):**

- The user's culture is fr-CH, whose number group separator is U+202F
  (narrow no-break space). In tai, pwsh's `ps` printed `6<box>597,39`.
- The console code pages were the legacy ones (OEM 437, ANSI 1252), not
  UTF-8. The character still arrived as one codepoint, not as mojibake such
  as `â€¯`, because ConPTY converts console output to UTF-8 for the
  terminal regardless of the code page.
- The embedded font, `fonts/JetBrainsMono-Medium.ttf`, maps 1364
  codepoints. A read of its `cmap` table found U+00A0, U+2019, U+20AC,
  U+2026, U+2500 and U+FFFD, but **not** U+202F, U+2009 (thin space) or
  U+2007 (figure space). All three are spaces (Unicode category Zs) that
  locales use in formatted numbers.

**What libghostty-vt provides** (libghostty-vt 0.2.2, Ghostty `a887df42`,
"terminal: bound page map probe lengths without tombstones (#13294)"):

- It parses the PTY byte stream as UTF-8. There is no other input encoding.
- `include/ghostty/vt/unicode.h`, exposed in Rust as
  `libghostty_vt::unicode::codepoint_width` and `grapheme_width`, gives
  "the exact same width table the terminal itself uses", including the
  grapheme-cluster rules of mode 2027 (`GHOSTTY_MODE_GRAPHEME_CLUSTER`).
- It does **not** detect or set a locale. Ghostty does that in its app
  layer: `src/os/locale.zig`, `ensureLocale`. On macOS, if `LANG` is unset
  (as when the `.app` is launched from Finder), it builds
  `<lang>_<country>.UTF-8` from the Cocoa preferred locale and sets `LANG`.
  On every Unix it then calls `setlocale(LC_ALL, "")`. If that fails it
  drops `LANG` and retries, and as a last resort it sets `LANG=en_US.UTF-8`.

**What tai does today:** the Unix child gets `TERM` and `COLORTERM` and
inherits everything else, including whatever locale tai was started with.
The Windows child gets `COLORTERM`. tai sets no locale variables.

**What the macOS session observed (macOS 12.7.6, 2026-10-24):**

- GUI launches carry no locale. `launchctl getenv` for `LANG`, `LC_ALL`
  and `LC_CTYPE` is empty, and neither Finder nor a Dock-launched iTerm2 has
  any of them in its environment. A tai started that way (simulated with
  `env -i`) gave its shell no `LANG` or `LC_*`. An interactive zsh fixes
  that from the user's `.zshrc`, but anything tai spawns directly, such as
  `ppxai-server`, would run in the C locale.
- The system-derived value can be invalid. With `AppleLocale=en_CH`,
  Ghostty's `setLangFromCocoa` builds `en_CH.UTF-8`, which macOS 12 doesn't
  have (no `/usr/share/locale/en_CH*`). `locale` then reports
  `LC_CTYPE="C"`. Ghostty recovers only through the later steps of
  `ensureLocale`: `setlocale` fails, so it unsets `LANG`, retries, and
  falls back to `en_US.UTF-8`. Mismatches between region and language like
  `en_CH` are common.
- A tai started from another terminal passes that terminal's identity on
  (`TERM_PROGRAM=iTerm.app`, `TERM_PROGRAM_VERSION`, `LC_TERMINAL`,
  `TERM_SESSION_ID`, `TERMINFO_DIRS`). Programs in tai may then use
  iTerm2-only sequences such as OSC 1337. Ghostty overwrites
  `TERM_PROGRAM` with its own value.

## Decision (proposed)

1. **UTF-8 is tai's only encoding,** on the PTY, the clipboard (on Windows the
   round trip through miniquad was tested with non-ASCII text) and the
   engine link. tai never transcodes.
2. **Sense the locale on Unix, and fix only the encoding.** At startup tai
   works out the child's effective character type, which is the first set
   of `LC_ALL`, `LC_CTYPE` and `LANG`:
   - If it names UTF-8 (`UTF-8` or `utf8`, any case), leave everything
     alone.
   - If none is set, then on macOS build `LANG` from the system's preferred
     locale as Ghostty does, but **validate it** and keep Ghostty's whole
     fallback chain (drop the value, retry, then `en_US.UTF-8`). On Linux
     set `LC_CTYPE=C.UTF-8`.
   - If `LANG` or `LC_CTYPE` names a non-UTF-8 locale, set `LC_CTYPE` to
     the UTF-8 variant of the same locale if the system has it, otherwise
     to `C.UTF-8` (Linux) or `UTF-8` (macOS). `LANG` and the other
     categories stay as they are, so the user's number, date and
     collation formats don't change.
   - If `LC_ALL` names a non-UTF-8 locale, don't override it: the user
     chose it explicitly, and overriding `LC_ALL` would change every
     category. Log a warning.

   This changes only the child's environment, never tai's own.
3. **On Windows, don't change code pages.** ConPTY already delivers UTF-8.
   Forcing code page 65001 breaks some legacy console programs. pwsh
   decoding a native program's UTF-8 output with the OEM code page is a
   known Windows gap that Windows Terminal shares. It can become an opt-in
   setting later.
4. **Never draw a box for a space.** When the font has no glyph for a
   codepoint of category Zs (U+00A0, U+1680, U+2000–U+200A, U+202F,
   U+205F, U+3000), draw an empty cell. This covers every locale's
   separators without depending on font coverage. Other missing glyphs go
   to the fallback font chain (`fonts-fallback`), and only then to U+FFFD.
5. **One width table.** Any layout tai does itself (selection, IME
   preedit, the milestone (b) chat UI) measures text with
   `libghostty_vt::unicode`, never a separate crate such as
   `unicode-width`, so tai and the terminal grid always agree.

## Why this and not the alternatives

- **Port Ghostty's `ensureLocale` as is:** it calls `setlocale(LC_ALL)`
  in tai's own process, which tai doesn't need (tai doesn't format text by
  locale), and it can replace `LANG` wholesale. Decision 2 keeps its
  macOS step and its fallback, but touches only `LC_CTYPE` where it can.
- **Always force `LANG=en_US.UTF-8`:** simple, but it throws away the
  user's formats, the very thing that produced U+202F here.
- **Ship a font that covers everything:** large, and the user's own
  locale data keeps growing. Decision 4 handles the common case (spaces)
  without any font, and leaves the rest to `fonts-fallback`.
- **Transcode legacy encodings in tai:** libghostty-vt is UTF-8 only, and
  every OS tai targets can run its children in UTF-8.

## Accepted risks

- **Width disagreements:** a program that measures with its own libc
  `wcwidth` can count a character as 1 cell while libghostty-vt counts 2,
  or the other way round, and the libc tables differ by OS and version.
  Columns then misalign. Mode 2027 helps only programs that enable it.
- **`C.UTF-8` may be missing** on old Linux systems. Upstream glibc added
  it in 2.35, though some distributions shipped it earlier. tai then falls back to `en_US.UTF-8` if available
  and otherwise logs a warning.
- **macOS locale sensing needs the system's preferred locale.** Ghostty
  reads it through Cocoa. tai has no Objective-C binding yet, so this may
  need `objc2` or a `defaults read -g AppleLocale` call.

## Open questions

- Should tai set `TERM_PROGRAM=tai` (and `TERM_PROGRAM_VERSION`) and drop
  an inherited `LC_TERMINAL*` and `TERM_SESSION_ID`, as Ghostty does for
  `TERM_PROGRAM`? It isn't an encoding question, but it's the same
  "what environment does the child get" decision.
- The sensed locale must also reach children tai spawns without a shell,
  such as `ppxai-server` (ADR 0001).

- Should tai's user configuration (when it has one) be able to override
  the sensed locale, as Ghostty's `language` option does on GTK?
- Is the Windows code page opt-in worth having before milestone (b)?

## Triggers to revisit

- libghostty-vt gains locale handling of its own.
- A user reports misaligned columns from width disagreements. Then look at
  enabling mode 2027 by default.
