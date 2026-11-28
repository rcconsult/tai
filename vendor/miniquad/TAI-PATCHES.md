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
2. `src/native/egl.rs`, `create_egl_context` takes an `EglApi`. The
   Wayland backend asks for desktop OpenGL (`eglBindAPI(EGL_OPENGL_API)`,
   `EGL_OPENGL_BIT`, version 2.1 in the compatibility profile, so the
   driver gives its highest version), as GLX does on X11 (`glx.rs`).
   Upstream never binds an API, so EGL's default, OpenGL ES 2, is what
   tai got on Wayland, and tai needs desktop OpenGL 3.2 (ADR 0012). The
   X11 EGL fallback and Android keep OpenGL ES 2.
3. `src/native/linux_wayland/decorations.rs`, `try_libdecor`: when
   libdecor makes no xdg surface (no plugin installed), fall back to
   miniquad's own decorations, after releasing libdecor's frame and
   context (left attached, they crashed the next dispatch), instead of
   asserting on the missing
   toplevel (`linux_wayland.rs`, `assert!(!display.xdg_toplevel
   .is_null())`). Seen on nested mutter with libdecor 0.1.0 and no
   plugin.
