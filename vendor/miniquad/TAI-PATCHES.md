# Vendored miniquad

Source: the crates.io release of miniquad 0.4.11 (not-fl3/miniquad),
MIT OR Apache-2.0, the version vendored macroquad 0.4.16 pins. Only
`src/`, `build.rs`, `Cargo.toml`, the licences and the README are
vendored; the first commit that adds this directory is the release
unchanged, so `git diff` against it shows every patch.

tai's root `Cargo.toml` uses this copy through `[patch.crates-io]`.
ADR 0015 decision 5 is why it is vendored: input methods and Wayland
need changes miniquad doesn't have.

## Patches

1. `Cargo.toml`: the dev-dependencies are gone (the examples aren't
   vendored), and `[lints.rust] warnings = "allow"`, as for macroquad
   (its patch 2).
5. Input methods and dead keys (ADR 0015). Numbered after the Wayland
   patches (2-4, branch `tai-wayland-native`), which this doesn't need.
   - `src/event.rs`: `EventHandler::ime_preedit_event` (the text being
     composed, empty when it ends) and `ime_commit_event` (committed
     text, or text with no key).
   - `src/lib.rs`, `src/native.rs`: `window::set_ime_area` (the cursor's
     cell in client pixels), a `SetImeArea` request. Windows takes its
     top-left as `set_ime_position`; X11, Android and iOS ignore it.
   - `src/native/macos.rs`: the view is an `NSTextInputClient`.
     `keyDown` runs `interpretKeyEvents` first, as Ghostty's does
     (`SurfaceView_AppKit.swift` at 108cacb1): a key that ends a
     composition sends only `ime_commit_event`; keys while composing
     send nothing (as winit does); others send `key_down_event` and then
     the text the system made, or their own characters as before.
     `setMarkedText`/`unmarkText` report the preedit; `insertText`
     outside `keyDown` (emoji picker, dictation) commits, joining a
     surrogate pair split over two calls; `firstRectForCharacterRange`
     answers with the `set_ime_area` cell; `doCommandBySelector`
     swallows commands so AppKit doesn't beep.
