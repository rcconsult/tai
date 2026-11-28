# ADR 0015: IME and dead keys in tai and tai-gui, through one input model

- **Status:** Accepted 2026-11-28 (owner), with the no-replay rule of
  decision 1 confirmed. Wayland for tai (decision 5) is open: both ways
  are tried and the owner picks after a live Linux session. Proposed
  2026-11-28. The owner asked for it ("we go for
  it") after the macOS session found that dead keys don't compose in
  either app. The macOS session reviewed it the same day; its
  corrections are folded in: commits, clearing the preedit, shortcuts
  while composing, and the AppKit details. The Linux session reviewed
  it too and probed X11 live (dead keys, Compose, IBus); its findings are
  folded in, including tai's native Wayland backend.
- **Deciders:** owner. Written by the Claude session on the Windows dev
  host.
- **Supersedes:** decision 7 of [ADR 0012](0012-cjk-font-fallback.md)
  ("IME is out of scope").
- **Related:** [ADR 0014](0014-shared-terminal-core.md) (the shared core
  in `src/term/`, whose step 5 put both apps' keys through
  `term/keys.rs`), [ADR 0011](0011-native-gui-egui.md) (tai-gui, IME
  through winit), [ADR 0003](0003-utf-8-everywhere.md) (rule 5: one
  width table), `docs/lessons/from-libghostty-projects.md` (lesson 2).

## Context

### What happens today

macOS session, 2026-11-28, on main `4f8dbea`. Option+e then e should
give `é`:

- tai sends `e`: the accent is lost.
- tai-gui sends `e e`: the dead key's letter leaks.
- Under the Kitty protocol, both send `\e[101;3u` then `\e[101u`.

TextEdit composes the same simulated keys, so the input is fine. Typing
CJK through an input method fails in both apps on every platform
(ADR 0012 decision 7, README "Known gaps").

### Why, per toolkit (sources read)

**tai on macOS**

- miniquad 0.4.11 `src/native/macos.rs`, `key_down` reads the NSEvent's
  own characters and never calls `interpretKeyEvents`.
- So macOS's text input system never sees the key, and nothing composes.
  A dead key has no characters; the next key has its plain letter.
- `set_ime_enabled` and `set_ime_position` are no-ops there.

**tai on X11** (Linux session, live on Xvfb, 2026-11-28; xterm as the
reference)

- miniquad's `linux_x11.rs` calls `XLookupString` with no input method,
  no input context and no compose status.
- Dead keys (US international): `dead_acute e` gives `e`, and the dead
  key sends nothing. xterm gives `é`.
- Compose: `Multi_key ' e` gives `'e`, and `Multi_key o c` gives `oc`.
- IBus (libpinyin): nothing composes; the keys reach the program as
  typed.

**tai on Wayland**

- tai asks miniquad for `LinuxBackend::WaylandWithX11Fallback`
  (`src/main.rs`), so on a Wayland session it runs on miniquad's native
  Wayland backend, not through XWayland.
- That backend's key text is a per-key `xkb_keysym_to_utf32`
  (`linux_wayland.rs`, around line 305). It has no compose state and no
  `zwp_text_input_v3`, so dead keys, Compose and input methods can't
  work there either. Not checked live: no dev host has a Wayland
  session.

**tai on Windows**

- `TranslateMessage` composes dead keys before `WM_CHAR`, so they work:
  checked live on 2026-11-28, Swiss French chosen with Win+Space, where
  `^ e` gives `ê` and `^ space` gives `^`, in pwsh. Not checked under
  the Kitty protocol.
- miniquad already sends an IME's committed text as characters
  (`WM_IME_COMPOSITION` with `GCS_RESULTSTR`).
- It places the candidate window from `GetCaretPos`, but tai never sets
  a caret.
- The text being composed (the preedit) shows in the IME's own floating
  window, not in the terminal.

**tai-gui (winit 0.30, egui 0.36)**

- winit runs a key through the platform's input method only when IME is
  allowed (macOS: `view.rs` `key_down`, around line 466).
- egui-winit allows it only while egui's output asks for IME
  (`PlatformOutput::ime`), which egui's own text fields do and the
  terminal pane doesn't.
- With IME off on macOS, a dead key arrives as a key with no text:
  egui-winit falls back from the dead logical key to the physical key.
- On Windows, winit composes dead keys into the key event's text either
  way: the same live check as tai's gives `ê` and `^`. Not checked under
  the Kitty protocol, where the dead key may leak as on X11.
- On X11 with IME off, winit skips `XFilterEvent`
  (`x11/event_processor.rs`, around line 152) and composes from
  xkbcommon's compose tables itself (`common/xkb/mod.rs`). Live on
  Xvfb:
  - in legacy mode, dead keys and Compose give `é` and `©`, as in xterm;
  - but under the Kitty protocol the dead key and each Compose key leak
    as keys of their own (`\e[39u` before `\e[101u`), and the composed
    text is lost;
  - and the Escape that cancels a Compose sequence reaches the program
    as `\x1b` (xterm swallows it).
- IBus composes in tai-gui's chat input (preedit shown, `你` committed),
  so winit's XIM link works; the terminal pane just never allows IME.

### How Ghostty does it (pinned `108cacb1`)

**macOS** (`macos/Sources/Ghostty/Surface View/SurfaceView_AppKit.swift`)

- `keyDown` calls `interpretKeyEvents`. The view is an
  `NSTextInputClient`, so `setMarkedText` holds the preedit (a dead key's
  `´` too) and `insertText` hands over committed text.
- A key that arrives while there is marked text, or that just ended it,
  is sent with `composing` set.
- Committed text is sent as the key's text.
- Control characters an input method accumulates while composing are
  dropped (`shouldSuppressComposingControlInput`).

**Linux**

- The GTK app uses `GtkIMContext`, which covers dead keys, Compose, IBus
  and fcitx.

**Preedit**

- `Surface.preeditCallback` stores the preedit, and the renderer draws it
  at the cursor (`src/renderer/generic.zig`).
- The input method's candidate window is placed at the cursor cell
  (`firstRect`).

**Encoder**

- The `composing` flag (`src/input/key_encode.zig`) makes the legacy
  encoding send nothing.
- The Kitty encoding sends only plain modifier keys.

### libghostty-vt

- The key event has `composing`: `ghostty_key_event_set_composing` in
  `include/ghostty/vt/key/event.h`, and `Event::set_composing` in the
  crate.
- `term/keys.rs` doesn't set it yet.

### Other projects (lesson 2)

- Raw keys drive only the encoder; committed text goes as text.
- Don't filter ASCII out of commits.
- Keys go to the IME while a composition is open.
- Repaint on every preedit and commit.
- Move the IME caret only once the cursor has stayed on one cell for two
  frames.

## Decision

1. **One input model in the core.** Each front end reports three
   things to `src/term/`:
   - **key events**, as now (`KeyIn`), plus a `composing` flag;
   - **committed text**, from the input method or a dead key;
   - **the preedit**: its text and the cursor within it, or none.

   The core applies Ghostty's rules for all three
   (`SurfaceView_AppKit.swift` `keyDown`, around lines 1219–1290):
   - A key that arrives while composing goes to the encoder with
     `set_composing(true)`. The legacy encoding sends nothing for it; the
     Kitty encoding sends only plain modifiers.
   - **A commit that ends a composition is text only.** It goes through
     the encoder as a key event with no key, no modifiers, composing off
     and the committed text (Ghostty's `committedTextAction`). It is
     never the pending key's text, and never raw bytes, because it must
     count as typed input, not as a paste.
   - **A commit with no composition before it is the key's text**, as
     today: plain typing, and Option+a giving å.
   - Text with no key at all (the emoji picker, dictation) is a
     text-only commit too.
   - ASCII isn't filtered out of commits.
   - Control characters that arrive while composing are dropped.
   - The committing key isn't replayed. Ghostty replays Up, Down and
     Right after a commit (and Left with a modifier), for Korean input
     methods. winit swallows that key, so tai-gui can't replay it, and
     tai does the same.
   - The committing key's release has no press that reached the
     program, so the press rule of ADR 0014 step 5 drops it. The core
     gets a test for that.
   - The preedit is state and is never written to the pty. It is
     cleared on commit, on focus loss, when the input method is
     disabled (where the front end learns of it), and when an ordinary
     key arrives while the core still holds a preedit (both toolkits
     send keys again only once composing has ended).
2. **The preedit is drawn by the core's row reader.**
   - `term/rows.rs` overlays it at the cursor, as cells measured with
     libghostty-vt's width table (ADR 0003 rule 5).
   - It is underlined and covers the cells under it.
   - Both renderers draw it from the same row output, as they draw
     everything else since ADR 0014 step 4.
   - A preedit or commit change repaints.
3. **The candidate window opens at the cursor cell.** The core gives
   each front end the cursor cell's rectangle. That rectangle moves only
   after the cursor has stayed on one cell for two frames (wispterm's
   rule), so a TUI redrawing its status line doesn't drag the window
   around.
4. **tai-gui** allows IME while a terminal pane has the focus.
   - The pane sets egui's `PlatformOutput::ime` to the cursor cell's
     rectangle.
   - It feeds `Event::Ime` preedit and commit to the core.
   - It has almost no keys to mark as composing. With IME allowed, winit
     holds back the keys an input method uses on macOS and Linux, and
     sends none while there is marked text unless `doCommandBySelector`
     fired (winit 0.30.13 macOS `view.rs`). On Windows, egui-winit
     filters `VK_PROCESSKEY` presses and their releases (egui-winit
     0.36.2 `lib.rs`, around lines 577–588).
   - egui-winit 0.36.2 ignores `Ime::Enabled` and `Ime::Disabled`
     (`lib.rs`, around line 721), so tai-gui doesn't learn of an input
     source switch. The core's other clearing rules (decision 1) cover
     it.
   - Shortcuts (Cmd+C, Cmd+V, Cmd+=) don't fire while composing: winit
     hands those keys to the input method. tai does the same (decision
     5), so the two apps agree.
   - That covers dead keys and input methods on macOS, X11 (XIM) and
     Wayland (text-input-v3), through winit.
5. **tai** gets the same three reports through a vendored miniquad.
   - **Where:** `vendor/miniquad`, from crates.io 0.4.11, used through
     `[patch.crates-io]`. Its patches are listed in
     `vendor/miniquad/TAI-PATCHES.md`, as macroquad's are.
   - **macOS:** the view becomes an `NSTextInputClient`. `keyDown` calls
     `interpretKeyEvents`, and the client implements:
     - `setMarkedText`, `unmarkText` and `insertText`;
     - `hasMarkedText` and `markedRange`;
     - `firstRectForCharacterRange`, from `set_ime_position`.

     From Ghostty's view as well:
     - **Input source switch:** a key during which the keyboard layout
       changed, with no marked text before it, is dropped; it was the
       switch shortcut. `setMarkedText` outside `keyDown` (a layout
       switched while a dead key is pending) updates the preedit at
       once.
     - **Key equivalents:** `performKeyEquivalent` lets Ctrl+Return
       through as it is and turns Ctrl+/ into Ctrl+_, so AppKit doesn't
       beep. `doCommandBySelector` swallows unhandled commands, such as
       Cmd+. (`cancel:`), without a beep.
     - **Surrogate pairs:** a high and a low UTF-16 surrogate that
       `insertText` delivers in two calls (as the emoji picker can) are
       joined.
     - **Shortcuts:** tai checks them only after the text input system
       has had the key, so they don't fire while composing, as in
       tai-gui (decision 4).
   - **X11:** `XOpenIM`, `XCreateIC`, `XFilterEvent` and
     `Xutf8LookupString`. That covers dead keys, Compose and XIM input
     methods. With no input method server running, Xlib's built-in
     local input method still composes dead keys and Compose (xterm did
     on Xvfb with no XIM server), so this covers dead keys without any
     IME. XIM's preedit callbacks give the preedit where the input method
     supports on-the-spot style (`XIMPreeditCallbacks`); otherwise the
     input method keeps its own window.
   - **Wayland: open, two ways tried.** tai doesn't start on miniquad
     0.4.11's native Wayland backend today (Linux session, nested mutter,
     2026-11-28):
     - with no server-side decorations and no libdecor plugin, libdecor
       leaves no `xdg_toplevel` and miniquad asserts
       (`linux_wayland.rs`, around line 1181);
     - with `ServerWithMiniquadFallback` it gets past that, but its EGL
       context is OpenGL ES 2: `egl.rs` never calls `eglBindAPI` and
       asks for `EGL_CONTEXT_CLIENT_VERSION 2`, and tai needs desktop
       OpenGL 3.2 (ADR 0012).

     The two ways, each on its own branch for the owner's live test:
     - **X11 only** (`LinuxBackend::X11Only`): a Wayland session reaches
       tai through XWayland, and the X11 patch above covers it.
     - **Native Wayland:** the vendored miniquad gets the decoration
       fallback, a desktop OpenGL 3.2 core context
       (`eglBindAPI(EGL_OPENGL_API)`), and then xkbcommon's compose
       tables and `zwp_text_input_v3`, as winit has for tai-gui.
   - **Windows:** the preedit from `GCS_COMPSTR`, and the candidate
     window from `set_ime_position` instead of the caret.
   - **How the reports reach tai:** through the input subscriber stream
     tai already reads (`utils::repeat_all_miniquad_input`), with new
     preedit and commit events in the vendored macroquad's forwarding.
   - **Upstream:** the patches are offered back to miniquad once they
     have held up.
6. **Order of work.** Each part is on its own branch, checked live on
   all three hosts. The parts merge only when both apps behave the same
   on every platform, so main never has one app ahead of the other.
   1. The core model and its tests: composing suppression, commit,
      preedit state and the preedit overlay.
   2. tai-gui on all three platforms.
   3. tai on macOS (vendored miniquad, `NSTextInputClient`).
   4. tai on X11 (XIM).
   5. tai on Wayland, by the way the owner picks.
   6. tai on Windows (inline preedit and candidate position).
7. **What the checks cover:**
   - **Dead keys:**
     - macOS: Option+e e and Option+u o on U.S.; the háček and čárka on
       Czech and Slovak QWERTY; ^ and ¨ on French and Swiss French;
     - Windows: US-International ' e;
     - X11: `dead_acute` and Compose.
   - **An input method per platform:** macOS Japanese (Kotoeri) and
     Pinyin, once the owner allows enabling them; Windows Microsoft IME
     (Japanese); Linux IBus or fcitx.
   - **Bytes:** a probe in legacy mode and with Kitty flags 11, 8 and
     16. A composition sends nothing until it commits; then both apps
     send the same bytes, with no orphan release.
   - **macOS only:**
     - the emoji picker (Ctrl+Cmd+Space), dictation and press-and-hold
       (the accent menu, whose choice may replace a letter already sent);
     - a layout switch while a dead key is pending;
     - Ctrl+/, Ctrl+Return and Cmd+.;
     - Cmd shortcuts pressed during a composition (they don't fire),
       and Cmd+C, Cmd+V and Cmd+= with no composition (they still fire,
       including when `interpretKeyEvents` ends in
       `doCommandBySelector`).
   - **Edge cases:** Backspace and Escape inside a composition cancel it
     without reaching the program, including the Escape that cancels a
     Compose sequence (tai-gui sends it today), and the candidate window
     sits at the cursor.
   - **Wayland:** dead keys, Compose and an input method in both apps,
     once a host with a Wayland session can run them.


## Implementation notes (2026-11-28)

Facts found while building parts 1-4, recorded without changing the
decisions above.

- **Checked live so far.**
  - Part 1 (core) and part 2 (tai-gui): macOS (US, Czech, Slovak,
    French and Swiss French dead keys; Kotoeri; Pinyin), Linux X11
    (dead keys and Compose through Xlib's own input method; IBus
    libpinyin) and Windows (Swiss French dead keys). The bytes match
    xterm on Linux, legacy and Kitty flags 11.
  - Part 4 (tai on X11): the same Linux checks, byte-identical to
    tai-gui and xterm.
- **tai-gui lost fast-typed keys through IBus on X11** (15 ms per key),
  fixed in `b580b7c`. egui reports Space and Enter on a focused clickable
  widget as a click (`FAKE_PRIMARY_CLICKED`); the pane took that click
  as a reason to request focus; a focus request interrupts the
  composition, which egui-winit does with `set_ime_allowed(false)` then
  `(true)`, and winit 0.30.13 then recreates the X input context,
  losing the key ibus-x11 still held. The pane now asks for focus only
  when it doesn't have it. Any other focus request while typing would do
  the same, so the rule is: nothing requests focus for a pane that has
  it. (Not winit #4777: re-sending the cursor area was ruled out.)
- **The candidate window on X11** opens at the bottom-left of the
  window in both apps with IBus: with on-the-spot preedit callbacks,
  ibus-x11 seems to ignore the spot location; xterm, over-the-spot, gets
  it at the cursor. Open.
- **tai-gui on macOS: the emoji picker and dictation** don't reach the
  pane: winit 0.30.13 `insertText` commits only while there is marked
  text, and tai-gui has no Edit menu for the picker's shortcut. tai
  (part 3) handles both.
- **A dead key cancelled by Backspace, Escape or an arrow on macOS**
  commits the accent (Option+u then Backspace sends ¨), as Ghostty's
  `committedTextAction` does; not compared with a running Ghostty.
- **Wayland without an input method** (a compositor with no
  text-input-v3 method): winit composes from xkbcommon itself and egui
  reports a dead key as a plain key, so under the Kitty protocol it
  still leaks in tai-gui, as on main.
- **tai has no focus-lost signal** from miniquad on every platform, so
  a composition left open when tai loses the focus ends at the next key.
- **tai sets `LC_CTYPE` on X11** (part 4), as winit does for tai-gui;
  XIM can't commit without it (`src/locale.rs`).

## Why this and not the alternatives

- **tai-gui only.** It's the smaller half, but it leaves the two apps
  different, which ADR 0014 exists to stop.
- **Composing dead keys in tai itself, alone**, from keyboard layout
  tables (UCKeyTranslate on macOS, xkbcommon's compose tables on X11).
  It would fix dead keys without the input-method plumbing, but not CJK
  input. On X11 it is what winit does today with IME off, and it is the
  path that leaks dead keys under the Kitty protocol. Where there is no
  OS input path for it (Wayland), compose tables are part of the plan,
  next to text-input-v3, not instead of it.
- **Running tai on X11 only** (`LinuxBackend::X11Only`), so a Wayland
  session reaches it through XWayland's XIM. One patch fewer, but it
  changes how tai runs on Wayland (scaling, the window), for a gap
  tai-gui fills natively. Once tai turned out not to start on the
  native backend at all, this became one of the two ways tried
  (decision 5).
- **Waiting for upstream miniquad.** It has IME on Windows only, and
  nothing on macOS or X11 is planned that we know of.
- **Moving tai onto winit (one renderer).** That would remove the
  miniquad work, but it is the larger change ADR 0014 decision 4 left to
  its own record.

## Accepted risks

- A vendored miniquad is a second vendored toolkit crate to carry
  through upgrades.
- The macOS patch is Objective-C runtime code (`msg_send!`, a class with
  added protocol methods), in the same style as miniquad's own.
- XIM is old, and which preedit styles IBus and fcitx offer through it
  is unchecked.
- No dev host has a Wayland session, so the Wayland half is untested
  until one does.

## Triggers to revisit

- Upstream miniquad gains IME on macOS and X11. Then drop the vendored
  copy's patches that it covers.
- tai moves to winit, or to tai-gui's renderer. Then tai takes tai-gui's
  path and the vendored miniquad goes.
- A Korean input method user loses the arrow key that ended a syllable
  (decision 1, no replay). Then look for a way to get that key from
  winit, and replay it in both apps as Ghostty does.
- An input method that the core's rules get wrong (a Korean IME
  committing on every syllable, or a voice-input commit). Then compare
  with Ghostty's handling of the same input method before changing the
  rules.
