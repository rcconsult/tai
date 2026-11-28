# ADR 0015: IME and dead keys in tai and tai-gui, through one input model

- **Status:** Proposed 2026-11-28. The owner asked for it ("we go for
  it") after the macOS session found that dead keys don't compose in
  either app. The macOS session reviewed it the same day; its
  corrections are folded in: commits, clearing the preedit, shortcuts
  while composing, and the AppKit details. It waits for the Linux
  session's review and the owner's acceptance.
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

**tai on X11**

- miniquad's `linux_x11.rs` calls `XLookupString` with no input method,
  no input context and no compose status.
- So dead keys, the Compose key and XIM input methods (IBus, fcitx)
  shouldn't compose either. Not checked live yet.

**tai on Windows**

- `TranslateMessage` composes dead keys before `WM_CHAR`, so they should
  already work. Not checked live yet.
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
  way. Not checked live yet.

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
     methods. XIM's preedit callbacks give the preedit where the input
     method supports on-the-spot style; otherwise the input method keeps
     its own window.
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
   5. tai on Windows (inline preedit and candidate position).
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
     - Cmd shortcuts pressed during a composition.
   - **Edge cases:** Backspace and Escape inside a composition cancel it
     without reaching the program, and the candidate window sits at the
     cursor.

## Why this and not the alternatives

- **tai-gui only.** It's the smaller half, but it leaves the two apps
  different, which ADR 0014 exists to stop.
- **Composing dead keys in tai itself**, from keyboard layout tables
  (UCKeyTranslate on macOS, xkbcommon's compose tables on X11). It would
  fix dead keys without the input-method plumbing, but not CJK input. It
  would also copy what the OS already does, and still differ from
  tai-gui, which uses the OS path.
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
- XIM is old. Wayland sessions reach tai through XWayland, whose XIM
  bridge depends on the input method.

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
