//! tai milestone (a): a terminal running `$SHELL` in a PTY on libghostty-vt.
//!
//! Taken from uzaaft/libghostty-rs `example/ghostling_rs/src/main.rs` at tag
//! v0.2.2 (a3a9676, "release: 0.2.2"), licensed MIT OR Apache-2.0. tai's
//! changes: window title, XTVERSION and log name, `TAI_APPLE_GFX` (gl|metal),
//! `COLORTERM`, a login shell on macOS, default signal handling for the
//! shell, HiDPI and baseline fixes, the copy/paste/font-size shortcuts in
//! `shortcuts.rs`, and icon font fallback in `fonts.rs`.
//!
//! Original description follows.
//!
//! A Rust port of Ghostling, using the `ghostty` crate for safe, idiomatic
//! `libghostty-vt` Rust bindings, and macroquad instead of raylib for
//! window management and rendering.
//!
//! This is quite a feature-rich implementation of a terminal emulator in
//! spite of its ~1kLoC size, which goes to show how much heavy-lifting
//! `libghostty-vt` is able to achieve behind a simple interface.

#![deny(unsafe_code)] // Well, almost.
use std::{
    cell::Cell,
    time::{Duration, Instant},
};

use macroquad::{
    input::utils,
    miniquad::{
        CursorIcon, EventHandler, KeyMods, conf,
        window::{order_quit, set_mouse_cursor},
    },
    prelude::*,
};

use libghostty_vt::{
    Terminal, TerminalOptions,
    alloc::Bytes,
    build_info,
    fmt::Format,
    key::{self, Key},
    kitty::graphics::{self, DecodePng, DecodedImage, Graphics, Layer, PlacementIterator},
    mouse,
    render::{CellIterator, Dirty, RenderState, RowIterator},
    selection::{
        FormatOptions,
        gesture::{DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent},
    },
    style::{RgbColor, StyleColor},
    terminal::{
        ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode, Point,
        PointCoordinate, PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes,
        SizeReportSize,
    },
};

use crate::fonts::{Coverage, Face, Fonts, is_space};
use crate::pty::{Child, Pty, PtyError};
use crate::shortcuts::Action;

mod cells;
mod clipboard;
mod fonts;
mod identity;
mod locale;
mod placeholders;
#[cfg(windows)]
#[path = "pty_windows.rs"]
mod pty;
mod shortcuts;
mod sixel;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Whether `TAI_APPLE_GFX=metal` asks for Metal. Other platforms ignore it.
fn use_metal() -> bool {
    cfg!(target_vendor = "apple") && std::env::var("TAI_APPLE_GFX").as_deref() == Ok("metal")
}

/// A material that copies a texture to the screen without blending.
///
/// Drawing a glyph into the frame's render target blends the alpha channel
/// too, which leaves alpha below 1 on antialiased edges. Presenting that
/// texture with macroquad's default alpha blending then mixes those edges
/// with the back buffer and darkens them. The shaders are macroquad's
/// default ones (vendor/macroquad/src/quad_gl.rs, `shader` module).
fn present_material() -> Result<Material> {
    const VERTEX: &str = r#"#version 100
    attribute vec3 position;
    attribute vec2 texcoord;
    attribute vec4 color0;

    varying lowp vec2 uv;
    varying lowp vec4 color;

    uniform mat4 Model;
    uniform mat4 Projection;

    void main() {
        gl_Position = Projection * Model * vec4(position, 1);
        color = color0 / 255.0;
        uv = texcoord;
    }"#;
    const FRAGMENT: &str = r#"#version 100
    varying lowp vec4 color;
    varying lowp vec2 uv;

    uniform sampler2D Texture;

    void main() {
        gl_FragColor = color * texture2D(Texture, uv);
    }"#;
    const METAL: &str = r#"#include <metal_stdlib>
    using namespace metal;

    struct Uniforms
    {
        float4x4 Model;
        float4x4 Projection;
    };

    struct Vertex
    {
        float3 position    [[attribute(0)]];
        float2 texcoord    [[attribute(1)]];
        float4 color0      [[attribute(2)]];
    };

    struct RasterizerData
    {
        float4 position [[position]];
        float4 color [[user(locn0)]];
        float2 uv [[user(locn1)]];
    };

    vertex RasterizerData vertexShader(Vertex v [[stage_in]], constant Uniforms& uniforms [[buffer(0)]])
    {
        RasterizerData out;
        out.position = uniforms.Model * uniforms.Projection * float4(v.position, 1);
        out.color = v.color0 / 255.0;
        out.uv = v.texcoord;
        return out;
    }

    fragment float4 fragmentShader(RasterizerData in [[stage_in]], texture2d<float> tex [[texture(0)]], sampler texSmplr [[sampler(0)]])
    {
        return in.color * tex.sample(texSmplr, in.uv);
    }"#;
    let shader = if use_metal() {
        ShaderSource::Msl { program: METAL }
    } else {
        ShaderSource::Glsl {
            vertex: VERTEX,
            fragment: FRAGMENT,
        }
    };
    // The default pipeline params have blending off.
    Ok(load_material(shader, MaterialParams::default())?)
}

// ---------------------------------------------------------------------------
// Adjustable constants for the renderer. All of these are chosen empirically,
// so feel free to tweak and tune these settings to your liking :)
// ---------------------------------------------------------------------------

/// Small padding from window edges
const PADDING: f32 = 6.0;

/// Horizontal gap between cells. This is usually not necessary for
/// TrueType fonts, but for bitmap fonts like macroquad's builtin font
/// this should be set to a positive value.
const CELL_GAP: f32 = 0.0;

/// Vertical gap between rows.
const ROW_GAP: f32 = 12.0;

/// How long to wait for the shell's echo after sending typed input. Reading
/// right after the write finds nothing yet, which put each echo a frame late.
/// A shell echoes within a millisecond or two; the wait ends as soon as it
/// does, so this only bounds the frame time when nothing comes back.
const ECHO_WAIT: Duration = Duration::from_millis(4);

/// How far into a frame the pty's output may be parsed before the frame is
/// drawn. Parsing stops earlier once the queued output runs out. A child
/// that never pauses can't starve the screen, and parsing still gets most
/// of a 60 Hz frame: a fixed 8 ms per frame halved flood throughput on
/// Linux and cost about 40% on macOS.
const PARSE_DEADLINE: Duration = Duration::from_millis(14);

#[macroquad::main(macroquad_conf)]
async fn main() -> Result<()> {
    // Initialize tracing with default log level set to info.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Make libghostty-vt emit through the same tracing subscriber.
    libghostty_vt::set_logger(Some(Box::new(libghostty_vt::log::TracingLogger)))?;

    // Font sizes are in logical (screen) points; macroquad rasterizes glyphs
    // at font_size * dpi_scale itself, so they stay crisp on HiDPI / Retina
    // displays without scaling here.
    let coverage = Coverage::new()?;
    let mut fonts = Fonts::load(&coverage);

    // Compute the initial grid from the window size
    // and measured cell metrics.
    let mut dims = Dimensions::new(Some(&fonts.primary), shortcuts::DEFAULT_FONT_SIZE);
    let (cols, rows) = dims.grid_size();

    // Track window size so we only recalculate the grid on actual changes.
    //
    // This is kept in a Cell<T> since the terminal effect handler
    // has to keep a reference to it, and we also want to modify it in the
    // main loop below. Don't worry, this is completely safe and is just
    // here to make the Rust compiler happy.
    let grid_size = Cell::new((cols, rows));
    // Same for the cell size in physical pixels, which changes with the
    // DPI scale.
    let cell_pixels = Cell::new(dims.cell_pixels());
    // When a font-size change resizes the grid. Each resize reflows the
    // screen and makes the shell redraw its prompt, so a run of size steps
    // is applied once, after the last one.
    let mut resize_due: Option<Instant> = None;
    // Whether mode 2048 (in-band size reports) was set after the last read.
    let mut in_band_resize = false;

    // Spawn a child shell connected to a pseudo-terminal. On Windows this
    // restricts tai's DLL search to its own folder and System32 (ADR 0007),
    // so keep it after the window and GL context exist, as it is here inside
    // macroquad::main.
    let (pty, mut child) = Pty::new(dims)?;

    // Install the PNG decoder so the terminal can handle PNG images in the
    // Kitty Graphics Protocol. This is a process-global setting and must be
    // done before any terminal is created.
    graphics::set_png_decoder(Some(Box::new(PngDecoder)))?;

    // Create a ghostty virtual terminal with the computed grid and 1000
    // lines of scrollback.  This holds all the parsed screen state (cells,
    // cursor, styles, modes) but knows nothing about the pty or the window.
    let mut terminal = Terminal::new(TerminalOptions {
        cols,
        rows,
        max_scrollback: 1000,
    })?;

    // The terminal options don't include cell pixel dimensions, so
    // issue an initial resize to set them.  Without this, Kitty
    // graphics placement_rect would divide by zero cell sizes.
    {
        let (cell_w, cell_h) = dims.cell_pixels();
        terminal.resize(cols, rows, cell_w, cell_h)?;
    }

    terminal
        // Enable Kitty graphics by setting a storage limit.  Without this the
        // terminal rejects all image data.  64 MiB is a generous default.
        .set_kitty_image_storage_limit(64 * 1024 * 1024)?
        // Allow images to be transmitted via file, temp file, and shared
        // memory mediums in addition to the default inline (direct) medium.
        .set_kitty_image_from_file_allowed(true)?
        .set_kitty_image_from_temp_file_allowed(true)?
        .set_kitty_image_from_shared_mem_allowed(true)?;

    // Whether DA1 lists Sixel (ADR 0009). tai translates Sixel either way.
    let advertise_sixel = std::env::var("TAI_SIXEL").as_deref() == Ok("1");

    // Register effects so the terminal can respond to VT queries (device
    // attributes, mode reports, size queries, etc.) that programs like
    // vim, tmux, and htop send during startup.  Without these, query
    // sequences are silently dropped and those programs may hang or
    // fall back to degraded behavior.
    terminal
        // write_pty effect — the terminal calls this whenever a VT sequence
        // requires a response back to the application (device status reports,
        // mode queries, device attributes, etc.).  Without this, programs like
        // vim and tmux that probe terminal capabilities would hang.
        .on_pty_write(|_t, data| pty.write(data))?
        // size effect — responds to XTWINOPS size queries (CSI 14/16/18 t)
        // so programs can discover the terminal geometry in cells and pixels.
        .on_size({
            let gs = &grid_size;
            let cp = &cell_pixels;
            move |_term| {
                let (columns, rows) = gs.get();
                let (cell_width, cell_height) = cp.get();
                Some(SizeReportSize {
                    rows,
                    columns,
                    cell_width,
                    cell_height,
                })
            }
        })?
        // device_attributes effect — responds to DA1/DA2/DA3 queries so
        // terminal applications can identify the terminal's capabilities.
        // We report VT220-level conformance with a modest feature set.
        .on_device_attributes(move |_term| {
            Some(DeviceAttributes {
                // DA1: VT220-level with a few common features. Sixel only on
                // request (ADR 0009): textual-image picks Sixel over Kitty
                // placeholders whenever it's listed.
                primary: PrimaryDeviceAttributes::new(
                    ConformanceLevel::VT220,
                    if advertise_sixel {
                        &[
                            DeviceAttributeFeature::COLUMNS_132,
                            DeviceAttributeFeature::SIXEL,
                            DeviceAttributeFeature::SELECTIVE_ERASE,
                            DeviceAttributeFeature::ANSI_COLOR,
                        ]
                    } else {
                        &[
                            DeviceAttributeFeature::COLUMNS_132,
                            DeviceAttributeFeature::SELECTIVE_ERASE,
                            DeviceAttributeFeature::ANSI_COLOR,
                        ]
                    },
                ),
                // DA2: VT220-type, version 1, no ROM cartridge.
                secondary: SecondaryDeviceAttributes {
                    device_type: DeviceType::VT220,
                    firmware_version: 1,
                    rom_cartridge: 0,
                },
                // DA3: default unit id (0).
                tertiary: Default::default(),
            })
        })?
        // xtversion effect — responds to CSI > q with our application name.
        .on_xtversion(|_term| Some("tai"))?
        // color_scheme effect — responds to CSI ? 996 n.
        // We don't have any API to query the OS color scheme, so we return
        // false to silently ignore the query rather than guessing.
        .on_color_scheme(|_term| None)?;

    // Create the objects used in rendering the terminal, including the
    // render state and various reusable iterators.
    let mut renderer = Renderer::new()?;

    // Create various objects used for input handling and encoding.
    // These include a key encoder, a reusable key event object, a mouse
    // encoder, a reusable mouse event object, and a byte buffer storing
    // encoded events to be written back into the PTY.
    //
    // The encoders translate input events into the correct VT escape
    // sequences, respecting terminal modes like application cursor keys,
    // the Kitty keyboard protocol, SGR mouse reporting and tracking modes,
    // etc.
    let mut input = Input::new()?;

    // Translates Sixel images in the pty's output into Kitty images.
    let mut sixel = sixel::Translator::new();

    tracing::info!(
        "tai | simd: {}, optimize: {:?}",
        if build_info::supports_simd()? {
            "enabled"
        } else {
            "disabled"
        },
        build_info::optimize_mode()?,
    );
    tracing::info!("Initialized terminal with size {cols}x{rows}");

    let mut stats = FrameStats::new();

    // Each frame: handle resize → read pty → process input → render.
    loop {
        let frame_start = Instant::now();
        // Recalculate grid dimensions when the window is resized.
        // We update both the ghostty terminal (so it reflows text) and the
        // pty's winsize (so the child shell knows about the new size and
        // can send SIGWINCH to its foreground process group).
        if dims.update(Some(&fonts.primary)) {
            apply_dims(dims, &grid_size, &cell_pixels, &mut terminal, &pty)?;
            resize_due = None;
        } else if resize_due.is_some_and(|due| Instant::now() >= due) {
            resize_due = None;
            // Skip it when the steps cancelled out (up three, down three).
            if (dims.grid_size(), dims.cell_pixels()) != (grid_size.get(), cell_pixels.get()) {
                apply_dims(dims, &grid_size, &cell_pixels, &mut terminal, &pty)?;
            }
        }

        // Do different things based on whether the child process
        // (the user shell) is still active or not. We only want to keep
        // processing inputs and communicate with the child when it's
        // alive, and when it's exited we should handle cleanup properly.
        input
            .selection
            .update_mouse_cursor(&terminal, dims, matches!(child, Child::Active(_)))?;

        match child {
            Child::Active(_) => {
                // Forward keyboard/mouse input only while the child is alive.
                input.handle_keyboard_input(&terminal)?;
                input.handle_mouse_input(&mut terminal, dims)?;

                // Run the app shortcuts the keyboard handler picked out.
                for action in std::mem::take(&mut input.actions) {
                    match action {
                        Action::Copy => copy_selection(&terminal)?,
                        Action::Paste => {
                            if let Some(text) = clipboard::get() {
                                let bracketed = terminal.mode(Mode::BRACKETED_PASTE)?;
                                input
                                    .response
                                    .extend(shortcuts::encode_paste(&text, bracketed)?);
                            }
                        }
                        Action::FontBigger | Action::FontSmaller | Action::FontReset => {
                            let size = shortcuts::next_font_size(dims.font_size, action);
                            if size != dims.font_size {
                                // Start empty glyph caches so the old size's
                                // atlases are dropped instead of growing per size.
                                fonts = fonts.fresh_caches();
                                dims = Dimensions::new(Some(&fonts.primary), size);
                                // Draw at the new size right away; resize the
                                // grid once the steps stop.
                                resize_due = Some(Instant::now() + shortcuts::RESIZE_SETTLE);
                            }
                        }
                    }
                }

                // Write the composed response back to the PTY,
                // and clear it in preparation of the next frame.
                let typed = !input.response.is_empty();
                pty.write(&input.response);
                input.response.clear();
                // Give the shell a moment to echo what was just typed, so the
                // echo is drawn this frame instead of the next one.
                if typed {
                    pty.wait_readable(ECHO_WAIT);
                }

                // Sixel images become Kitty images on the way in (ADR 0006).
                let cell_height = cell_pixels.get().1;
                let mut write = |bytes: &[u8]| {
                    sixel.feed(bytes, cell_height, &mut |out| terminal.vt_write(out));
                };
                match pty.read(&mut write, frame_start + PARSE_DEADLINE) {
                    Ok(_) => {}
                    Err(PtyError::EndOfStream) => {
                        // EOF — the child's side of the pty is closed.
                        child.mark_exited();
                    }
                    Err(PtyError::OtherError(e)) => {
                        // Other error — the child's side of the pty is closed.
                        tracing::error!("failed to read from pty: {e}");
                        child.mark_exited();
                    }
                }

                // Send a size report when mode 2048 gets set, as Ghostty's
                // app does (src/termio/stream_handler.zig, at a887df42);
                // libghostty-vt only sends one on resize. Textual waits for
                // it before it reads SGR-Pixels mouse reports as pixels.
                let reports = terminal.mode(Mode::IN_BAND_RESIZE)?;
                if reports && !in_band_resize {
                    pty.write(in_band_size_report(grid_size.get(), cell_pixels.get()).as_bytes());
                }
                in_band_resize = reports;
            }
            // Try to reap the child if it had already exited.
            //
            // EOF can arrive before the child is waitable, so a single
            // WNOHANG attempt right at EOF may miss.  We also check for
            // signal death so the banner can report it properly.
            Child::Exited(_) => {
                child.try_reap();
                // FIXME: For some reason the child isn't being reaped properly.
                // Let's just unconditionally quit for now.
                order_quit();
            }
            Child::Reaped(ref status) => {
                tracing::info!("Child process exited: {status:?}");
                order_quit();
            }
        }

        // Draw the current terminal screen. Mouse buttons and the wheel may
        // change what's shown without changing any cell (a selection being
        // dragged, the viewport scrolled), so they always redraw. Typing
        // doesn't need to: the shell's echo changes cells.
        let mouse_active = mouse_wheel() != (0.0, 0.0)
            || [MouseButton::Left, MouseButton::Right, MouseButton::Middle]
                .into_iter()
                .any(|b| is_mouse_button_down(b) || is_mouse_button_released(b));
        let render_start = Instant::now();
        renderer.render_terminal(&terminal, dims, &fonts, &coverage, mouse_active)?;
        let render_end = Instant::now();
        next_frame().await;
        if let Some(stats) = stats.as_mut() {
            stats.record(frame_start, render_start, render_end, Instant::now());
        }
    }
}

/// Per-second frame timings, logged when `TAI_FRAME_STATS=1`, for comparing
/// renderers (ADR 0008).
struct FrameStats {
    since: Instant,
    frames: u32,
    /// Sums and maxima, in seconds: the whole frame, the renderer's CPU
    /// side, and `next_frame` (macroquad's own draws and the buffer swap).
    total: (f64, f64),
    render: (f64, f64),
    present: (f64, f64),
}

impl FrameStats {
    fn new() -> Option<Self> {
        (std::env::var("TAI_FRAME_STATS").as_deref() == Ok("1")).then(|| Self {
            since: Instant::now(),
            frames: 0,
            total: (0.0, 0.0),
            render: (0.0, 0.0),
            present: (0.0, 0.0),
        })
    }

    fn record(&mut self, start: Instant, render: Instant, rendered: Instant, end: Instant) {
        let add = |acc: &mut (f64, f64), d: Duration| {
            acc.0 += d.as_secs_f64();
            acc.1 = acc.1.max(d.as_secs_f64());
        };
        add(&mut self.total, end - start);
        add(&mut self.render, rendered - render);
        add(&mut self.present, end - rendered);
        self.frames += 1;
        if end - self.since >= Duration::from_secs(1) {
            let n = f64::from(self.frames);
            let ms = |acc: (f64, f64)| (acc.0 / n * 1000.0, acc.1 * 1000.0);
            let (t, r, p) = (ms(self.total), ms(self.render), ms(self.present));
            tracing::info!(
                "frames {} | frame avg {:.2} max {:.2} ms | render avg {:.2} max {:.2} ms | next_frame avg {:.2} max {:.2} ms",
                self.frames,
                t.0,
                t.1,
                r.0,
                r.1,
                p.0,
                p.1
            );
            *self = Self::new().expect("stats are on");
        }
    }
}

/// Push new grid dimensions to the terminal (so it reflows text) and to the
/// pty's winsize (so the child shell gets SIGWINCH).
fn apply_dims(
    dims: Dimensions,
    grid_size: &Cell<(u16, u16)>,
    cell_pixels: &Cell<(u32, u32)>,
    terminal: &mut Terminal<'_, '_>,
    pty: &Pty,
) -> Result<()> {
    let (cols, rows) = dims.grid_size();
    grid_size.set((cols, rows));
    cell_pixels.set(dims.cell_pixels());
    let (cell_w, cell_h) = dims.cell_pixels();
    terminal.resize(cols, rows, cell_w, cell_h)?;
    pty.resize(dims);
    Ok(())
}

/// Copy the selection to the clipboard as plain text, the way Ghostty does:
/// soft-wrapped lines joined and trailing blanks trimmed. Does nothing
/// without a selection.
fn copy_selection(terminal: &Terminal<'_, '_>) -> Result<()> {
    let options = FormatOptions::new()
        .with_emit_format(Format::Plain)
        .with_unwrap(true)
        .with_trim(true);
    if let Some(text) = terminal.format_selection_alloc(None, options)?
        && !text.is_empty()
    {
        clipboard::set(&String::from_utf8_lossy(&text));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

struct Renderer<'alloc> {
    render_state: RenderState<'alloc>,
    row_it: RowIterator<'alloc>,
    cell_it: CellIterator<'alloc>,
    placement_it: PlacementIterator<'alloc>,
    /// The last drawn frame, kept in a texture so a frame where nothing
    /// changed only copies it to the screen instead of drawing every cell.
    frame: Option<Frame>,
    /// Copies the frame to the screen with blending off. See
    /// [`present_material`].
    present_material: Material,
    /// The cell-buffer renderer of ADR 0008, or `None` for the
    /// immediate-mode one: under Metal, without instancing, or with
    /// `TAI_RENDERER=immediate`.
    cells: Option<CellGrid>,
}

/// The state of the cell-buffer renderer (ADR 0008). Each viewport row's
/// backgrounds and glyph instances are rebuilt only when the row is dirty,
/// as Ghostty's `rebuildRow` does (src/renderer/generic.zig at a887df42),
/// and every changed frame draws all rows from them: the backgrounds as
/// one texture with a texel per cell, the text as one instanced draw.
struct CellGrid {
    text: cells::TextPass,
    atlas: cells::GlyphAtlas,
    rows: Vec<RowCells>,
    /// What the rows and the atlas were built for.
    dims: Option<Dimensions>,
    bg_texture: Option<Texture2D>,
    bg_pixels: Vec<u8>,
    instances: Vec<cells::Instance>,
}

/// One viewport row, as last built.
#[derive(Default)]
struct RowCells {
    /// A colour per cell; transparent where the terminal's background shows.
    bg: Vec<[u8; 4]>,
    text: Vec<cells::Instance>,
    virtual_runs: Vec<placeholders::Placement>,
}

impl CellGrid {
    /// `None` for the immediate renderer. Logs which renderer draws and,
    /// when it's the immediate one, why.
    fn new() -> Option<Self> {
        if use_metal() {
            tracing::info!("renderer: immediate (Metal; the cell renderer needs OpenGL)");
            return None;
        }
        if std::env::var("TAI_RENDERER").as_deref() == Ok("immediate") {
            tracing::info!("renderer: immediate (TAI_RENDERER=immediate)");
            return None;
        }
        let text = cells::TextPass::new()
            .map_err(|why| tracing::warn!("renderer: immediate ({why})"))
            .ok()?;
        let atlas_height = cells::ATLAS_MAX_HEIGHT.min(text.max_texture_size);
        tracing::info!(
            "renderer: cells (GLSL {}, glyph atlas up to {atlas_height} px)",
            text.glsl
        );
        Some(Self {
            atlas: cells::GlyphAtlas::new(atlas_height),
            text,
            rows: Vec::new(),
            dims: None,
            bg_texture: None,
            bg_pixels: Vec::new(),
            instances: Vec::new(),
        })
    }
}

/// A drawn terminal frame and what it was drawn from.
struct Frame {
    target: RenderTarget,
    dims: Dimensions,
    /// Whether the cursor was drawn, and where. The render state's dirty
    /// flags track cell contents, so the cursor is compared separately.
    cursor: Option<(u16, u16)>,
}

impl<'alloc> Renderer<'alloc> {
    fn new() -> Result<Self> {
        Ok(Self {
            render_state: RenderState::new()?,
            row_it: RowIterator::new()?,
            cell_it: CellIterator::new()?,
            placement_it: PlacementIterator::new()?,
            frame: None,
            present_material: present_material()?,
            cells: CellGrid::new(),
        })
    }

    /// Render the current terminal screen using the RenderState API.
    ///
    /// For each row/cell we read the grapheme codepoints and the cell's style,
    /// resolve foreground/background colors via the palette, and draw each
    /// character individually with `draw_text_ex`.  This supports per-cell colors
    /// from SGR sequences (bold, 256-color, 24-bit RGB, etc.).
    ///
    /// cell_width and cell_height are the measured dimensions of a single
    /// monospace glyph at the current font size, in screen (logical) pixels.
    /// font_size is the logical font size (before DPI scaling).
    fn render_terminal(
        &mut self,
        terminal: &Terminal<'alloc, '_>,
        dims: Dimensions,
        fonts: &Fonts,
        coverage: &Coverage,
        force: bool,
    ) -> Result<()> {
        if let Some(mut grid) = self.cells.take() {
            let result = self.render_cells(&mut grid, terminal, dims, coverage, force);
            self.cells = Some(grid);
            return result;
        }
        // Snapshot the terminal state into our render state. This is the
        // only point where we need access to the terminal; after this the
        // snapshot owns everything we need to draw the frame.
        //
        // When a snapshot is active, the render state cannot be updated —
        // this is upheld in the Rust API by making the snapshot take an
        // exclusive (mutable) reference to the render state.
        let snapshot = self.render_state.update(terminal)?;

        // Redraw only when something changed, as Ghostty does: its renderer
        // draws on a wakeup from the IO thread and skips clean frames
        // (src/renderer/Thread.zig at a887df42). The render state reports
        // changed cells; the cursor, the window and font size, and input
        // (a selection being dragged) are checked here too. Otherwise the
        // last frame is copied to the screen.
        let cursor = if snapshot.cursor_visible()? {
            snapshot.cursor_viewport()?.map(|vp| (vp.x, vp.y))
        } else {
            None
        };
        let unchanged = !force
            && snapshot.dirty()? == Dirty::Clean
            && self
                .frame
                .as_ref()
                .is_some_and(|f| f.dims == dims && f.cursor == cursor);
        if unchanged {
            if let Some(frame) = &self.frame {
                Self::present(frame, &self.present_material);
            }
            return Ok(());
        }
        // Obtain the Kitty graphics storage from the terminal. This is a
        // reference valid until the next mutating terminal call.
        let graphics = terminal.kitty_graphics();

        // Redraw only the rows that changed when that's safe, as Ghostty's
        // renderer rebuilds only dirty rows (src/renderer/generic.zig,
        // rebuildCells, at a887df42). Other rows keep what the frame texture
        // already holds. A full-screen TUI that updates one line, such as a
        // spinner, then costs one row instead of the whole screen. Images
        // can span rows, and mouse input can change the selection or the
        // viewport without marking rows, so both redraw everything.
        let mut full = force
            || snapshot.dirty()? == Dirty::Full
            || self.frame.as_ref().is_none_or(|f| f.dims != dims);
        if !full && let Ok(graphics) = graphics.as_ref() {
            let mut placements = self.placement_it.update(graphics)?;
            placements.set_layer(Layer::All)?;
            full = placements.next().is_some();
        }

        if self.frame.as_ref().is_none_or(|f| f.dims != dims) {
            // The texture has the window's size in physical pixels, so text
            // stays as sharp as when it's drawn to the screen directly.
            let target = render_target(
                (dims.window_width * dims.dpi_scale).round() as u32,
                (dims.window_height * dims.dpi_scale).round() as u32,
            );
            target.texture.set_filter(FilterMode::Nearest);
            self.frame = Some(Frame {
                target,
                dims,
                cursor,
            });
        }
        let frame = self.frame.as_mut().expect("frame was just created");
        // The cursor is drawn over its row, so the rows it leaves and enters
        // are redrawn too.
        let cursor_rows = [frame.cursor, cursor].map(|c| c.map(|(_, y)| y));
        frame.cursor = cursor;
        let mut camera =
            Camera2D::from_display_rect(Rect::new(0.0, 0.0, dims.window_width, dims.window_height));
        // A render target is drawn upside down compared with the screen.
        camera.zoom.y = -camera.zoom.y;
        camera.render_target = Some(frame.target.clone());
        set_camera(&camera);

        // Get the terminal's background color from the render state snapshot.
        let colors = snapshot.colors()?;
        if full {
            clear_background(color(colors.background));
        }

        // Populate the row iterator from the current render state snapshot,
        // resulting in a row *iteration* object, which can be thought of as
        // a cursor into a row of the snapshot. Attributes of the current row
        // can be read from it via a shared reference, or it can be moved
        // to point at the next row via a mutable reference.
        let mut row_it = self.row_it.update(&snapshot)?;

        // --- Layer 1: images below cell backgrounds (z < INT32_MIN/2) ---
        if let Ok(graphics) = graphics.as_ref() {
            Self::render_kitty_images(
                terminal,
                &mut self.placement_it,
                graphics,
                Layer::BelowBg,
                dims,
            )?
        }

        // Small padding from the window edges.
        let mut y = PADDING;

        // Reusable text buffer for reading cell graphemes as UTF-8 text.
        let mut text = String::with_capacity(16);

        // Fallback-font icons of the current row. They're drawn after the
        // row's cells: an icon may spread into the next cell when that one is
        // blank, which is only known once the next cell is read, and the next
        // cell's background must not paint over it.
        let mut icons: Vec<Icon> = Vec::new();

        // Backgrounds and text are queued and drawn in two passes once all
        // rows are read, as Ghostty's renderer keeps separate background
        // and text cell buffers (src/renderer/generic.zig at a887df42).
        // Drawing each cell's background and then its glyph switched
        // textures on every cell, and every switch is a separate draw call:
        // a TUI that colours every cell's background took about a second
        // per frame on the macOS dev host's Intel GPU.
        let mut backgrounds = Backgrounds::default();
        let mut glyphs = Glyphs::default();
        let mut row_icons: Vec<Icon> = Vec::new();

        // Kitty Unicode placeholder runs, by viewport row, drawn once all
        // text is down (ADR 0005), and the current row's placeholder cells.
        let mut virtual_runs: Vec<(u16, placeholders::Placement)> = Vec::new();
        let mut placeholder_cells: Vec<Option<placeholders::PlaceholderCell>> = Vec::new();
        let mut row_index: u16 = 0;

        // For convenience, `next` gives you the same iteration back only
        // as a shared pointer, so you can simultaneously iterate through
        // all rows while having a handle to query data for each row.
        while let Some(row) = row_it.next() {
            if !full {
                if !row.dirty()? && !cursor_rows.contains(&Some(row_index)) {
                    y += dims.cell_height;
                    row_index += 1;
                    continue;
                }
                // Clear the row, padding included, before drawing it again.
                backgrounds.push(
                    0.0,
                    y,
                    dims.window_width,
                    dims.cell_height,
                    color(colors.background),
                );
            }
            let mut x = PADDING;
            // Whether the last icon is waiting to learn if the next cell is blank.
            let mut icon_open = false;
            // libghostty-vt flags rows holding a U+10EEEE placeholder.
            let placeholder_row = row.raw_row()?.has_kitty_virtual_placeholder()?;
            placeholder_cells.clear();

            // Cell iterators work similarly as they create a cell *iteration*
            // from a row iteration, which can then be used in a similar pattern.
            let mut cell_it = self.cell_it.update(row)?;

            while let Some(cell) = cell_it.next() {
                // Selection membership is computed by libghostty from the
                // terminal-owned selection.  Ghostling only chooses the visual
                // policy: selected cells use reverse-video colors, matching the
                // common terminal behavior and avoiding a separate theme knob.
                let selected = cell.is_selected()?;
                let graphemes = cell.graphemes_len()?;
                let bg = cell.bg_color()?;
                if graphemes > 0 {
                    // Convert read grapheme codepoints into UTF-8 text.
                    cell.graphemes_utf8(&mut text)?;
                }
                if std::mem::take(&mut icon_open)
                    && (graphemes == 0 || text.chars().next().is_some_and(is_space))
                    && let Some(icon) = icons.last_mut()
                {
                    icon.cells = 2;
                }

                // A placeholder cell shows part of an image, drawn later; its
                // own character has no glyph.
                if placeholder_row {
                    let cp = (graphemes > 0).then(|| cell.graphemes()).transpose()?;
                    if let Some(cps) = cp.filter(|c| c.first() == Some(&placeholders::PLACEHOLDER))
                    {
                        let style = cell.style()?;
                        placeholder_cells.push(Some(placeholders::PlaceholderCell {
                            diacritics: cps[1..].to_vec(),
                            fg: id_color(style.fg_color),
                            underline: id_color(style.underline_color),
                        }));
                        if let Some(bg) = bg {
                            backgrounds.push(x, y, dims.cell_width, dims.cell_height, color(bg));
                        }
                        x += dims.cell_width;
                        continue;
                    }
                    placeholder_cells.push(None);
                }

                if graphemes == 0 {
                    if selected {
                        backgrounds.push(
                            x,
                            y,
                            dims.cell_width,
                            dims.cell_height,
                            color(colors.foreground),
                        );
                    } else if let Some(bg) = bg {
                        // The cell has no text, but it might have a background
                        // color (e.g. from an erase with a color set).
                        backgrounds.push(x, y, dims.cell_width, dims.cell_height, color(bg));
                    }
                } else {
                    // Resolve foreground and background colors using the new
                    // per-cell color queries. These flatten style colors,
                    // content-tag colors, and palette lookups into a single RGB
                    // value, returning `None` when the cell has no explicit color
                    // (in which case we use the terminal default).
                    let mut fg = cell.fg_color()?.unwrap_or(colors.foreground);
                    let mut has_bg = bg.is_some();
                    let mut bg = bg.unwrap_or(colors.background);
                    let mut bold = false;

                    if cell.has_styling()? {
                        // Read the style for flags (inverse, bold, italic) — color
                        // resolution is handled above via the new API.
                        let style = cell.style()?;

                        // Inverse (reverse video): swap foreground and background colors.
                        if style.inverse {
                            std::mem::swap(&mut fg, &mut bg);
                            has_bg = true;
                        }
                        bold = style.bold;
                    }

                    if selected {
                        std::mem::swap(&mut fg, &mut bg);
                        has_bg = true;
                    }

                    // Draw a background rectangle if the cell has a non-default bg
                    // or if inverse mode forced a swap.
                    if has_bg {
                        backgrounds.push(x, y, dims.cell_width, dims.cell_height, color(bg));
                    }

                    // Draw the text for the cell, in a fallback font when the
                    // primary lacks the glyph (e.g. Nerd Font icons). The first
                    // codepoint decides for the whole grapheme.
                    let face = text
                        .chars()
                        .next()
                        .map_or(Face::Primary, |ch| coverage.face_for(ch));
                    if face == Face::Symbols || face == Face::Iosevka {
                        icons.push(Icon {
                            text: text.clone(),
                            face,
                            x,
                            y,
                            fg: color(fg),
                            bold,
                            cells: 1,
                        });
                        icon_open = true;
                    } else if face == Face::Primary {
                        glyphs.push(&text, x, y + dims.baseline, color(fg), bold);
                    }
                }

                x += dims.cell_width;
                continue;
            }

            row_icons.append(&mut icons);

            if placeholder_row {
                let runs = placeholders::row_placements(placeholder_cells.drain(..));
                virtual_runs.extend(runs.into_iter().map(|run| (row_index, run)));
            }

            // Clear per-row dirty flag after rendering it.
            row.set_dirty(false)?;
            y += dims.cell_height;
            row_index += 1;
        }

        // The two passes, then the icons, which overhang their cell.
        backgrounds.draw();
        glyphs.draw(&fonts.primary, dims.font_size);
        // Cache every icon glyph before drawing any: see cache_glyphs.
        for icon in &row_icons {
            let place = coverage.place(icon.face, dims.font_size, icon.cells);
            cache_glyphs(&icon.text, fonts.get(icon.face), place.font_size);
        }
        for icon in row_icons {
            let place = coverage.place(icon.face, dims.font_size, icon.cells);
            let baseline = icon.y + dims.baseline - place.rise;
            draw_glyph(
                &icon.text,
                icon.x,
                baseline,
                fonts.get(icon.face),
                place.font_size,
                icon.fg,
                icon.bold,
            );
        }

        // --- Layer 2: images below text (i32::MIN / 2 <= z < 0) ---
        // Drawn after cell backgrounds but before the cursor and any
        // above-text images.  In our single-pass renderer the cell text
        // has already been drawn, but this still achieves the correct
        // visual for the common case where images sit behind text.
        if let Ok(graphics) = graphics.as_ref() {
            Self::render_kitty_images(
                terminal,
                &mut self.placement_it,
                graphics,
                Layer::BelowText,
                dims,
            )?;
            // Images shown through Unicode placeholders (ADR 0005).
            if !virtual_runs.is_empty() {
                Self::render_virtual_placements(
                    &mut self.placement_it,
                    graphics,
                    &virtual_runs,
                    dims,
                )?;
            }
        }

        // Draw the cursor if visible.
        if snapshot.cursor_visible()?
            && let Some(vp) = snapshot.cursor_viewport()?
        {
            // Draw the cursor using the foreground color (or explicit cursor
            // color if the terminal set one).
            let cursor_color = colors.cursor.unwrap_or(colors.foreground);

            draw_rectangle(
                PADDING + vp.x as f32 * dims.cell_width,
                PADDING + vp.y as f32 * dims.cell_height,
                dims.cell_width,
                dims.cell_height,
                color(cursor_color),
            );
        }

        // --- Layer 3: images above text (z >= 0) ---
        if let Ok(graphics) = graphics.as_ref() {
            Self::render_kitty_images(
                terminal,
                &mut self.placement_it,
                graphics,
                Layer::AboveText,
                dims,
            )?
        }

        // Reset global dirty state so the next update reports changes accurately.
        snapshot.set_dirty(Dirty::Clean)?;

        set_default_camera();
        if let Some(frame) = &self.frame {
            Self::present(frame, &self.present_material);
        }
        Ok(())
    }

    /// Render the terminal with the cell-buffer renderer (ADR 0008). Cells
    /// are read as in the immediate-mode path above, but into per-row
    /// buffers, and only for dirty rows.
    fn render_cells(
        &mut self,
        grid: &mut CellGrid,
        terminal: &Terminal<'alloc, '_>,
        dims: Dimensions,
        coverage: &Coverage,
        force: bool,
    ) -> Result<()> {
        let snapshot = self.render_state.update(terminal)?;
        let cursor = if snapshot.cursor_visible()? {
            snapshot.cursor_viewport()?.map(|vp| (vp.x, vp.y))
        } else {
            None
        };
        let unchanged = !force
            && snapshot.dirty()? == Dirty::Clean
            && self
                .frame
                .as_ref()
                .is_some_and(|f| f.dims == dims && f.cursor == cursor);
        if unchanged {
            if let Some(frame) = &self.frame {
                Self::present(frame, &self.present_material);
            }
            return Ok(());
        }
        let graphics = terminal.kitty_graphics();

        // Mouse input can change the selection or the viewport without
        // marking rows, so it rebuilds every row, as do new dimensions.
        let rebuild = force || snapshot.dirty()? == Dirty::Full || grid.dims != Some(dims);
        if grid
            .dims
            .is_none_or(|d| d.font_size != dims.font_size || d.dpi_scale != dims.dpi_scale)
        {
            // A new glyph size: start an empty atlas, as Ghostty builds a
            // new one per font size (see `Fonts::fresh_caches`).
            grid.atlas.clear();
        }
        grid.dims = Some(dims);

        if self.frame.as_ref().is_none_or(|f| f.dims != dims) {
            let target = render_target(
                (dims.window_width * dims.dpi_scale).round() as u32,
                (dims.window_height * dims.dpi_scale).round() as u32,
            );
            target.texture.set_filter(FilterMode::Nearest);
            self.frame = Some(Frame {
                target,
                dims,
                cursor,
            });
        }
        let frame = self.frame.as_mut().expect("frame was just created");
        frame.cursor = cursor;
        let mut camera =
            Camera2D::from_display_rect(Rect::new(0.0, 0.0, dims.window_width, dims.window_height));
        // A render target is drawn upside down compared with the screen.
        camera.zoom.y = -camera.zoom.y;
        camera.render_target = Some(frame.target.clone());
        set_camera(&camera);

        let colors = snapshot.colors()?;
        let CellGrid {
            text: text_pass,
            atlas,
            rows,
            bg_texture,
            bg_pixels,
            instances,
            ..
        } = grid;

        let mut row_it = self.row_it.update(&snapshot)?;
        let mut text = String::with_capacity(16);
        let mut icons: Vec<Icon> = Vec::new();
        let mut placeholder_cells: Vec<Option<placeholders::PlaceholderCell>> = Vec::new();
        let mut row_index = 0;
        let mut y = PADDING;
        let transparent = [0; 4];
        let rgba = |c: RgbColor| [c.r, c.g, c.b, 255];

        while let Some(row) = row_it.next() {
            if rows.len() <= row_index {
                rows.push(RowCells::default());
            }
            if !rebuild && !row.dirty()? {
                y += dims.cell_height;
                row_index += 1;
                continue;
            }
            let built = &mut rows[row_index];
            built.bg.clear();
            built.text.clear();
            built.virtual_runs.clear();
            let mut x = PADDING;
            let mut icon_open = false;
            let placeholder_row = row.raw_row()?.has_kitty_virtual_placeholder()?;
            placeholder_cells.clear();

            let mut cell_it = self.cell_it.update(row)?;
            while let Some(cell) = cell_it.next() {
                let selected = cell.is_selected()?;
                let graphemes = cell.graphemes_len()?;
                let bg = cell.bg_color()?;
                if graphemes > 0 {
                    cell.graphemes_utf8(&mut text)?;
                }
                if std::mem::take(&mut icon_open)
                    && (graphemes == 0 || text.chars().next().is_some_and(is_space))
                    && let Some(icon) = icons.last_mut()
                {
                    icon.cells = 2;
                }

                if placeholder_row {
                    let cp = (graphemes > 0).then(|| cell.graphemes()).transpose()?;
                    if let Some(cps) = cp.filter(|c| c.first() == Some(&placeholders::PLACEHOLDER))
                    {
                        let style = cell.style()?;
                        placeholder_cells.push(Some(placeholders::PlaceholderCell {
                            diacritics: cps[1..].to_vec(),
                            fg: id_color(style.fg_color),
                            underline: id_color(style.underline_color),
                        }));
                        built.bg.push(bg.map_or(transparent, rgba));
                        x += dims.cell_width;
                        continue;
                    }
                    placeholder_cells.push(None);
                }

                if graphemes == 0 {
                    built.bg.push(if selected {
                        rgba(colors.foreground)
                    } else {
                        bg.map_or(transparent, rgba)
                    });
                } else {
                    let mut fg = cell.fg_color()?.unwrap_or(colors.foreground);
                    let mut has_bg = bg.is_some();
                    let mut bg = bg.unwrap_or(colors.background);
                    let mut bold = false;
                    if cell.has_styling()? {
                        let style = cell.style()?;
                        if style.inverse {
                            std::mem::swap(&mut fg, &mut bg);
                            has_bg = true;
                        }
                        bold = style.bold;
                    }
                    if selected {
                        std::mem::swap(&mut fg, &mut bg);
                        has_bg = true;
                    }
                    built.bg.push(if has_bg { rgba(bg) } else { transparent });

                    let face = text
                        .chars()
                        .next()
                        .map_or(Face::Primary, |ch| coverage.face_for(ch));
                    if face == Face::Symbols || face == Face::Iosevka {
                        icons.push(Icon {
                            text: text.clone(),
                            face,
                            x,
                            y,
                            fg: color(fg),
                            bold,
                            cells: 1,
                        });
                        icon_open = true;
                    } else if face == Face::Primary {
                        atlas.push_text(
                            &mut built.text,
                            coverage.font(face),
                            face,
                            &text,
                            x,
                            y + dims.baseline,
                            dims.font_size,
                            dims.dpi_scale,
                            color(fg),
                            bold,
                        );
                    }
                }
                x += dims.cell_width;
            }

            // Icons go after the row's text, so one that overhangs into a
            // blank next cell isn't covered by it.
            for icon in icons.drain(..) {
                let place = coverage.place(icon.face, dims.font_size, icon.cells);
                atlas.push_text(
                    &mut built.text,
                    coverage.font(icon.face),
                    icon.face,
                    &icon.text,
                    icon.x,
                    icon.y + dims.baseline - place.rise,
                    place.font_size,
                    dims.dpi_scale,
                    icon.fg,
                    icon.bold,
                );
            }
            if placeholder_row {
                built.virtual_runs = placeholders::row_placements(placeholder_cells.drain(..));
            }
            row.set_dirty(false)?;
            y += dims.cell_height;
            row_index += 1;
        }
        rows.truncate(row_index);

        clear_background(color(colors.background));
        if let Ok(graphics) = graphics.as_ref() {
            Self::render_kitty_images(
                terminal,
                &mut self.placement_it,
                graphics,
                Layer::BelowBg,
                dims,
            )?
        }

        // Backgrounds: one texel per cell, stretched over the grid with
        // nearest filtering, as Ghostty's cell_bg pass looks up each pixel's
        // cell (src/renderer/shaders/glsl/cell_bg.f.glsl at a887df42).
        let cols = rows.iter().map(|r| r.bg.len()).max().unwrap_or(0);
        if cols > 0 {
            bg_pixels.clear();
            bg_pixels.resize(cols * rows.len() * 4, 0);
            for (r, row) in rows.iter().enumerate() {
                let start = r * cols * 4;
                bg_pixels[start..start + row.bg.len() * 4].copy_from_slice(row.bg.as_flattened());
            }
            let (w, h) = (cols as u16, rows.len() as u16);
            let texture = match bg_texture {
                Some(t) if t.width() as u16 == w && t.height() as u16 == h => {
                    t.update_from_bytes(w.into(), h.into(), bg_pixels);
                    t
                }
                _ => {
                    let t = Texture2D::from_rgba8(w, h, bg_pixels);
                    t.set_filter(FilterMode::Nearest);
                    bg_texture.insert(t)
                }
            };
            draw_texture_ex(
                texture,
                PADDING,
                PADDING,
                WHITE,
                DrawTextureParams {
                    dest_size: Some(vec2(
                        cols as f32 * dims.cell_width,
                        rows.len() as f32 * dims.cell_height,
                    )),
                    ..Default::default()
                },
            );
        }

        instances.clear();
        for row in rows.iter() {
            instances.extend_from_slice(&row.text);
        }
        text_pass.draw(
            instances,
            atlas,
            camera.matrix(),
            frame.target.render_pass.raw_miniquad_id(),
        );

        if let Ok(graphics) = graphics.as_ref() {
            Self::render_kitty_images(
                terminal,
                &mut self.placement_it,
                graphics,
                Layer::BelowText,
                dims,
            )?;
            let virtual_runs: Vec<(u16, placeholders::Placement)> = rows
                .iter()
                .enumerate()
                .flat_map(|(r, row)| row.virtual_runs.iter().map(move |run| (r as u16, *run)))
                .collect();
            if !virtual_runs.is_empty() {
                Self::render_virtual_placements(
                    &mut self.placement_it,
                    graphics,
                    &virtual_runs,
                    dims,
                )?;
            }
        }

        if let Some((x, y)) = cursor {
            let cursor_color = colors.cursor.unwrap_or(colors.foreground);
            draw_rectangle(
                PADDING + x as f32 * dims.cell_width,
                PADDING + y as f32 * dims.cell_height,
                dims.cell_width,
                dims.cell_height,
                color(cursor_color),
            );
        }

        if let Ok(graphics) = graphics.as_ref() {
            Self::render_kitty_images(
                terminal,
                &mut self.placement_it,
                graphics,
                Layer::AboveText,
                dims,
            )?
        }

        snapshot.set_dirty(Dirty::Clean)?;
        set_default_camera();
        if let Some(frame) = &self.frame {
            Self::present(frame, &self.present_material);
        }
        Ok(())
    }

    /// Copy a drawn frame to the screen.
    fn present(frame: &Frame, material: &Material) {
        gl_use_material(material);
        draw_texture_ex(
            &frame.target.texture,
            0.0,
            0.0,
            WHITE,
            DrawTextureParams {
                dest_size: Some(vec2(frame.dims.window_width, frame.dims.window_height)),
                ..Default::default()
            },
        );
        gl_use_default_material();
    }

    /// Draw the image slices that Unicode placeholder runs show. Each run's
    /// image and grid come from its virtual placement; the slice and its
    /// position are worked out as in Ghostty (`src/placeholders.rs`).
    fn render_virtual_placements(
        placement_it: &mut PlacementIterator<'_>,
        graphics: &Graphics<'_>,
        runs: &[(u16, placeholders::Placement)],
        dims: Dimensions,
    ) -> Result<()> {
        // Virtual placements' grids (`c`, `r`): by image and placement id,
        // and the first one of each image for runs that name no placement.
        let mut by_id = std::collections::HashMap::new();
        let mut first = std::collections::HashMap::new();
        let mut placements = placement_it.update(graphics)?;
        placements.set_layer(Layer::All)?;
        while let Some(placement) = placements.next() {
            if !placement.is_virtual()? {
                continue;
            }
            let image_id = placement.image_id()?;
            let grid = (placement.columns()?, placement.rows()?);
            by_id.insert((image_id, placement.placement_id()?), grid);
            first.entry(image_id).or_insert(grid);
        }

        // The protocol's sizes are in physical pixels; tai draws in logical
        // units.
        let scale = dims.dpi_scale;
        let cell_w = (dims.cell_width * scale).round().max(1.0) as u32;
        let cell_h = (dims.cell_height * scale).round().max(1.0) as u32;
        let mut textures = std::collections::HashMap::new();

        for (row, run) in runs {
            let grid = if run.placement_id > 0 {
                by_id.get(&(run.image_id, run.placement_id))
            } else {
                first.get(&run.image_id)
            };
            let (Some(&(columns, rows)), Some(image)) = (grid, graphics.image(run.image_id)) else {
                continue;
            };
            if image.format()? != graphics::ImageFormat::Rgba {
                continue;
            }
            let (img_w, img_h) = (image.width()?, image.height()?);
            let data = image.data()?;
            if data.len() < (img_w * img_h * 4) as usize {
                continue;
            }
            let grid = placeholders::grid(columns, rows, img_w, img_h, cell_w, cell_h);
            let Some(slice) = placeholders::render(run, grid, img_w, img_h, cell_w, cell_h) else {
                continue;
            };
            let tex = textures
                .entry(run.image_id)
                .or_insert_with(|| Texture2D::from_rgba8(img_w as u16, img_h as u16, data));
            draw_texture_ex(
                tex,
                PADDING + f32::from(run.x) * dims.cell_width + slice.offset_x as f32 / scale,
                PADDING + f32::from(*row) * dims.cell_height + slice.offset_y as f32 / scale,
                WHITE,
                DrawTextureParams {
                    dest_size: Some(vec2(
                        slice.dest_width as f32 / scale,
                        slice.dest_height as f32 / scale,
                    )),
                    source: Some(Rect {
                        x: slice.source_x as f32,
                        y: slice.source_y as f32,
                        w: slice.source_width as f32,
                        h: slice.source_height as f32,
                    }),
                    ..Default::default()
                },
            );
        }
        Ok(())
    }

    fn render_kitty_images<'t>(
        terminal: &'t Terminal<'_, '_>,
        placement_it: &mut PlacementIterator<'_>,
        graphics: &Graphics<'t>,
        layer: Layer,
        dims: Dimensions,
    ) -> Result<()> {
        let mut placements = placement_it.update(graphics)?;
        placements.set_layer(layer)?;

        while let Some(placement) = placements.next() {
            let image_id = placement.image_id()?;
            let Some(image) = graphics.image(image_id) else {
                continue;
            };

            let info = placement.placement_render_info(&image, terminal)?;

            // Skip images that aren't visible in our viewport,
            // or have zero width/height in either pixel or grid coordinates
            if !info.viewport_visible
                || info.pixel_width == 0
                || info.pixel_height == 0
                || info.grid_cols == 0
                || info.grid_rows == 0
            {
                continue;
            }

            // We only handle RGBA (the PNG decoder we registered converts
            // everything to RGBA)
            if image.format()? != graphics::ImageFormat::Rgba {
                continue;
            }

            let image_width = image.width()?;
            let image_height = image.height()?;
            let data = image.data()?;
            if data.len() < (image_width * image_height * 4) as usize {
                continue;
            }

            // Compute grid cell count for rendered size.
            let dest_size = vec2(
                info.grid_cols as f32 * dims.cell_width,
                info.grid_rows as f32 * dims.cell_height,
            );

            // Read the sub-cell pixel offsets. They are in the terminal's
            // physical pixels; we draw in logical units.
            let x_offset = placement.x_offset()? as f32 / dims.dpi_scale;
            let y_offset = placement.y_offset()? as f32 / dims.dpi_scale;

            let tex = Texture2D::from_rgba8(image_width as u16, image_height as u16, data);

            draw_texture_ex(
                &tex,
                PADDING + info.viewport_col as f32 * dims.cell_width + x_offset,
                PADDING + info.viewport_row as f32 * dims.cell_height + y_offset,
                WHITE,
                DrawTextureParams {
                    dest_size: Some(dest_size),
                    source: Some(Rect {
                        x: info.source_x as f32,
                        y: info.source_y as f32,
                        w: info.source_width as f32,
                        h: info.source_height as f32,
                    }),
                    ..Default::default()
                },
            );
        }

        Ok(())
    }
}

/// A fallback-font glyph waiting to be drawn at the end of its row.
struct Icon {
    text: String,
    face: Face,
    x: f32,
    y: f32,
    fg: Color,
    bold: bool,
    /// How many cells it spans: 2 when the next cell is blank.
    cells: u8,
}

/// Cell backgrounds queued for one frame. Neighbouring cells of the same
/// colour on a row merge into one rectangle.
#[derive(Default)]
struct Backgrounds(Vec<(Rect, Color)>);

impl Backgrounds {
    fn push(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        if let Some((last, c)) = self.0.last_mut()
            && *c == color
            && last.y == y
            && last.h == h
            && (last.x + last.w - x).abs() < 0.01
        {
            last.w += w;
            return;
        }
        self.0.push((Rect::new(x, y, w, h), color));
    }

    fn draw(&self) {
        for (r, c) in &self.0 {
            draw_rectangle(r.x, r.y, r.w, r.h, *c);
        }
    }
}

/// Primary-font text queued for one frame: each cell's text is a range of
/// one buffer, so queuing doesn't allocate per cell.
#[derive(Default)]
struct Glyphs {
    text: String,
    cells: Vec<(std::ops::Range<usize>, f32, f32, Color, bool)>,
}

impl Glyphs {
    fn push(&mut self, text: &str, x: f32, baseline: f32, fg: Color, bold: bool) {
        let start = self.text.len();
        self.text.push_str(text);
        self.cells
            .push((start..self.text.len(), x, baseline, fg, bold));
    }

    fn draw(&self, font: &Font, font_size: u16) {
        // Cache every glyph first: see cache_glyphs.
        cache_glyphs(&self.text, font, font_size);
        for (range, x, baseline, fg, bold) in &self.cells {
            draw_glyph(
                &self.text[range.clone()],
                *x,
                *baseline,
                font,
                font_size,
                *fg,
                *bold,
            );
        }
    }
}

/// Put the glyphs of `text` into the font's atlas without drawing them.
///
/// macroquad caches a glyph the first time it's drawn. When a new glyph
/// doesn't fit, the atlas doubles, every glyph moves, and the atlas texture
/// is replaced. Glyphs of that font already queued in the frame still point
/// at the old texture and positions, so they draw as nothing or garbage,
/// and tai's kept frame keeps them that way. Measuring the text caches the
/// glyphs at the same pixel size `draw_text_ex` uses, so any growth happens
/// before the first glyph of the frame is queued.
fn cache_glyphs(text: &str, font: &Font, font_size: u16) {
    measure_text(text, Some(font), font_size, 1.0);
}

/// Draw one cell's text with its baseline at `baseline`.
fn draw_glyph(
    text: &str,
    x: f32,
    baseline: f32,
    font: &Font,
    font_size: u16,
    fg: Color,
    bold: bool,
) {
    let params = TextParams {
        font: Some(font),
        font_size,
        color: fg,
        ..Default::default()
    };
    draw_text_ex(text, x, baseline, params.clone());

    // Bold: draw the text a second time shifted 1 pixel to the right to
    // thicken the strokes ("fake bold").
    //
    // In a more sophisticated terminal one would obviously use the correct
    // bold version of the font using font discovery, but let's do it the
    // hackier way here.
    if bold {
        draw_text_ex(text, x + 1.0, baseline, params);
    }
}

/// Convert Ghostty colors to macroquad colors
fn color(rgb: RgbColor) -> Color {
    let RgbColor { r, g, b } = rgb;
    color_u8!(r, g, b, 255)
}

/// A style colour as the Kitty placeholder protocol reads it: the raw value,
/// not the colour it resolves to.
fn id_color(c: StyleColor) -> placeholders::IdColor {
    match c {
        StyleColor::None => placeholders::IdColor::None,
        StyleColor::Palette(index) => placeholders::IdColor::Palette(index.0),
        StyleColor::Rgb(RgbColor { r, g, b }) => placeholders::IdColor::Rgb(r, g, b),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Dimensions {
    window_width: f32,
    window_height: f32,
    cell_width: f32,
    cell_height: f32,
    /// Font size in logical points.
    font_size: u16,
    /// Offset from a cell's top edge to the text baseline.
    baseline: f32,
    /// DPI scale the cell metrics were measured at.
    dpi_scale: f32,
}

impl Dimensions {
    fn new(font: Option<&Font>, font_size: u16) -> Self {
        // Measure a representative glyph to derive the monospace cell size.
        // measure_text applies the DPI scale itself and returns logical
        // units, the same units we draw in.
        let glyph_size = measure_text("M", font, font_size, 1.0);

        // Guard against zero cell dimensions — these would cause division
        // by zero when computing the terminal grid.
        // Add in the cell and row gaps here too to make calculations easier.
        Self {
            cell_width: glyph_size.width.max(1.0) + CELL_GAP,
            cell_height: glyph_size.height.max(1.0) + ROW_GAP,
            // Center the glyph vertically: half the row gap above it, then
            // its ascent (offset_y) down to the baseline.
            font_size,
            baseline: ROW_GAP / 2.0 + glyph_size.offset_y,
            dpi_scale: screen_dpi_scale(),
            window_width: screen_width(),
            window_height: screen_height(),
        }
    }

    /// Cell size in physical pixels, as the terminal and the pty report it.
    fn cell_pixels(self) -> (u32, u32) {
        (
            (self.cell_width * self.dpi_scale) as u32,
            (self.cell_height * self.dpi_scale) as u32,
        )
    }

    fn grid_size(self) -> (u16, u16) {
        let cols = ((self.window_width - 2.0 * PADDING) / self.cell_width).max(1.0) as u16;
        let rows = ((self.window_height - 2.0 * PADDING) / self.cell_height).max(1.0) as u16;
        (cols, rows)
    }

    /// The grid's size in physical pixels, as the pty reports it: whole
    /// cells only, without the padding or the leftover part of a cell.
    /// Programs divide this by the grid size to get the cell size (as
    /// textual-image does to size images), so it must match `cell_pixels`.
    /// This is what Ghostty reports with balanced padding
    /// (`src/renderer/size.zig`, `Size.terminal`, at a887df42).
    fn grid_pixels(self) -> (u16, u16) {
        let (cols, rows) = self.grid_size();
        let (cell_w, cell_h) = self.cell_pixels();
        let px = |cells: u16, cell: u32| (u32::from(cells) * cell).min(u32::from(u16::MAX)) as u16;
        (px(cols, cell_w), px(rows, cell_h))
    }

    fn update(&mut self, font: Option<&Font>) -> bool {
        // The DPI scale may not be final on the first frame, or may change
        // when the window moves between displays; re-measure the cells then.
        if screen_dpi_scale() != self.dpi_scale {
            *self = Self::new(font, self.font_size);
            return true;
        }
        if screen_width() == self.window_width && screen_height() == self.window_height {
            return false;
        }
        self.window_width = screen_width();
        self.window_height = screen_height();
        true
    }
    #[cfg(unix)]
    fn to_winsize(self) -> nix::pty::Winsize {
        let (cols, rows) = self.grid_size();
        let (xpixel, ypixel) = self.grid_pixels();
        nix::pty::Winsize {
            ws_col: cols,
            ws_row: rows,
            ws_xpixel: xpixel,
            ws_ypixel: ypixel,
        }
    }
}

// ---------------------------------------------------------------------------
// Input handling
// ---------------------------------------------------------------------------

/// One keyboard event from macroquad's ordered input stream, with the
/// modifiers held when it happened.
enum KeyInput {
    Down {
        keycode: KeyCode,
        mods: key::Mods,
        repeat: bool,
    },
    Up {
        keycode: KeyCode,
        mods: key::Mods,
    },
    Char(char),
}

/// Collects the keyboard events macroquad replays to an input subscriber
/// (`utils::repeat_all_miniquad_input`), in the order they happened.
#[derive(Default)]
struct KeyEvents(Vec<KeyInput>);

impl EventHandler for KeyEvents {
    fn update(&mut self) {}
    fn draw(&mut self) {}
    fn key_down_event(&mut self, keycode: KeyCode, keymods: KeyMods, repeat: bool) {
        self.0.push(KeyInput::Down {
            keycode,
            mods: mods_from(keymods),
            repeat,
        });
    }
    fn key_up_event(&mut self, keycode: KeyCode, keymods: KeyMods) {
        self.0.push(KeyInput::Up {
            keycode,
            mods: mods_from(keymods),
        });
    }
    fn char_event(&mut self, character: char, _keymods: KeyMods, _repeat: bool) {
        self.0.push(KeyInput::Char(character));
    }
}

/// Whether `ch` is the character macOS reports for a function key: U+F700
/// to U+F8FF for the arrows, Home, F1 and so on (NSUpArrowFunctionKey...).
/// miniquad passes them on as typed characters. Given as the key's text,
/// the encoder sends that character instead of the key's sequence once a
/// program enables the Kitty keyboard protocol (ESC[>1u), as ppxaide
/// does: U+F701 for the down arrow, not ESC[B (see the test). Ghostty's
/// macOS app drops them
/// the same way (macos/Sources/Ghostty/NSEvent+Extension.swift,
/// `ghosttyCharacters`, at a887df42).
fn is_function_key_char(ch: char) -> bool {
    ('\u{f700}'..='\u{f8ff}').contains(&ch)
}

/// The mode 2048 size report, ESC [ 48 ; rows ; cols ; height ; width t,
/// encoded like Ghostty's src/terminal/size_report.zig (at a887df42).
fn in_band_size_report((cols, rows): (u16, u16), (cell_w, cell_h): (u32, u32)) -> String {
    let (height, width) = (
        u64::from(rows) * u64::from(cell_h),
        u64::from(cols) * u64::from(cell_w),
    );
    format!("\x1b[48;{rows};{cols};{height};{width}t")
}

fn mods_from(keymods: KeyMods) -> key::Mods {
    let mut mods = key::Mods::empty();
    if keymods.shift {
        mods |= key::Mods::SHIFT;
    }
    if keymods.alt {
        mods |= key::Mods::ALT;
    }
    if keymods.ctrl {
        mods |= key::Mods::CTRL;
    }
    if keymods.logo {
        mods |= key::Mods::SUPER;
    }
    mods
}

/// A key press (or repeat) waiting for the characters the platform sends
/// after it.
struct PendingKey {
    action: key::Action,
    key: Key,
    ucp: char,
    mods: key::Mods,
    text: String,
}

struct Input<'alloc> {
    key_encoder: key::Encoder<'alloc>,
    key_event: key::Event<'alloc>,
    mouse_encoder: mouse::Encoder<'alloc>,
    mouse_event: mouse::Event<'alloc>,
    response: Vec<u8>,
    selection: SelectionState<'alloc>,
    /// App shortcuts pressed this frame, run by the main loop.
    actions: Vec<Action>,
    /// Keys whose press was a shortcut, so their release is swallowed too.
    swallowed: Vec<KeyCode>,
    /// A font-size shortcut still held, which keeps stepping.
    held: Option<shortcuts::Held>,
    /// Our subscription to macroquad's ordered input event stream.
    input_subscriber: usize,
    /// This frame's keyboard events, reused between frames.
    key_events: KeyEvents,
    /// The key pressed last, waiting for the characters it produces.
    pending_key: Option<PendingKey>,
    /// Whether characters are dropped until the next key press, because
    /// they belong to a shortcut or a Cmd chord.
    drop_chars: bool,
    /// Whether the platform reports a key's character just before its key
    /// press (X11) rather than just after it (Windows, macOS, Wayland).
    /// `None` until a key with a character shows which.
    chars_before_keys: Option<bool>,
    /// On X11, characters waiting for the key press they belong to.
    lead_text: String,
}
impl<'alloc> Input<'alloc> {
    fn new() -> Result<Self> {
        Ok(Self {
            key_encoder: key::Encoder::new()?,
            key_event: key::Event::new()?,
            mouse_encoder: mouse::Encoder::new()?,
            mouse_event: mouse::Event::new()?,
            response: Vec::with_capacity(64),
            selection: SelectionState::new()?,
            actions: Vec::new(),
            swallowed: Vec::new(),
            held: None,
            input_subscriber: utils::register_input_subscriber(),
            key_events: KeyEvents::default(),
            pending_key: None,
            drop_chars: false,
            // miniquad doesn't say whether it ended up on Wayland or X11.
            chars_before_keys: if cfg!(target_os = "linux") {
                None
            } else {
                Some(false)
            },
            lead_text: String::new(),
        })
    }

    /// Read this frame's keyboard events from macroquad's ordered event
    /// stream and use the libghostty key encoder to produce the correct VT
    /// escape sequences, which are then written to the pty. The encoder
    /// respects terminal modes (cursor key application mode, Kitty keyboard
    /// protocol, etc.) so we don't need to maintain our own escape-sequence
    /// tables.
    ///
    /// The stream keeps the order in which keys and characters happened, and
    /// marks auto-repeat. Polling key state instead (is_key_pressed) loses
    /// both: macroquad never reports a repeat as a press, so held non-text
    /// keys didn't repeat, and a frame's characters couldn't be matched to
    /// the keys that produced them, so they could be dropped or reordered.
    fn handle_keyboard_input(&mut self, terminal: &Terminal<'alloc, '_>) -> Result<()> {
        // macroquad also queues characters for get_char_pressed and never
        // clears that queue itself. tai reads the stream instead, so empty it.
        while get_char_pressed().is_some() {}

        let mut events = std::mem::take(&mut self.key_events);
        utils::repeat_all_miniquad_input(&mut events, self.input_subscriber);
        if self.chars_before_keys.is_none() {
            self.chars_before_keys = Self::char_order(&events.0);
        }
        let chars_before_keys = self.chars_before_keys == Some(true);
        let order_unknown = self.chars_before_keys.is_none();
        for event in events.0.drain(..) {
            match event {
                KeyInput::Down {
                    keycode,
                    mods,
                    repeat,
                } => {
                    self.flush_pending_key(terminal)?;
                    // On X11 the key's character came just before it.
                    let lead = std::mem::take(&mut self.lead_text);

                    // A shortcut's key held down: its repeats, and the text
                    // the platform produces for them ("=" for Ctrl+=), never
                    // reach the pty. The held step timer below handles them.
                    if self.swallowed.contains(&keycode) {
                        self.drop_chars = true;
                        continue;
                    }
                    if !repeat
                        && let Some(shortcut) =
                            shortcuts::action_for(keycode, mods, cfg!(target_os = "macos"))
                    {
                        self.actions.push(shortcut);
                        self.swallowed.push(keycode);
                        if shortcuts::repeats(shortcut) {
                            self.held =
                                Some(shortcuts::Held::new(keycode, shortcut, Instant::now()));
                        }
                        self.drop_chars = true;
                        continue;
                    }

                    // On macOS a Cmd chord is never terminal input, but the
                    // platform still reports its character ("=" for Cmd+=).
                    self.drop_chars = cfg!(target_os = "macos") && mods.contains(key::Mods::SUPER);

                    let mut lead = if self.drop_chars { String::new() } else { lead };

                    // With the order still unknown, held characters belong to
                    // this key only if it's a Ctrl chord (X11 reports 'c' for
                    // Ctrl+C just before it). Not with Alt as well, which is
                    // AltGr on Windows. Otherwise they were typed on their own.
                    if order_unknown
                        && !(mods.contains(key::Mods::CTRL) && !mods.contains(key::Mods::ALT))
                    {
                        self.response.extend_from_slice(lead.as_bytes());
                        lead.clear();
                    }

                    // A key tai has no mapping for: its character, if any,
                    // goes to the pty as plain text.
                    let Some((key, ucp)) = Self::key_for(keycode) else {
                        self.response.extend_from_slice(lead.as_bytes());
                        continue;
                    };
                    self.pending_key = Some(PendingKey {
                        action: if repeat {
                            key::Action::Repeat
                        } else {
                            key::Action::Press
                        },
                        key,
                        ucp,
                        mods,
                        text: lead,
                    });
                }
                KeyInput::Up { keycode, mods } => {
                    self.flush_pending_key(terminal)?;
                    self.flush_lead_text();
                    // The release of a shortcut's key is swallowed like its press.
                    if let Some(i) = self.swallowed.iter().position(|&k| k == keycode) {
                        self.swallowed.swap_remove(i);
                        continue;
                    }
                    if let Some((key, ucp)) = Self::key_for(keycode) {
                        self.encode_key(terminal, key::Action::Release, key, ucp, mods, None)?;
                    }
                }
                KeyInput::Char(ch) => {
                    // Windows also reports Ctrl+letter, Enter, Tab and
                    // Backspace as control characters (Ctrl+C as U+0003).
                    // Given those as text, the encoder emits CSI u (Ctrl+C
                    // becomes ESC[3;5u) instead of the control byte, and
                    // ConPTY doesn't treat that as Ctrl+C. The encoder
                    // derives control bytes from the key and mods anyway.
                    if ch.is_control() || is_function_key_char(ch) {
                        continue;
                    }
                    if chars_before_keys {
                        self.lead_text.push(ch);
                        continue;
                    }
                    if self.drop_chars {
                        continue;
                    }
                    match &mut self.pending_key {
                        // The character belongs to the key just pressed.
                        Some(pending) => pending.text.push(ch),
                        // Maybe X11 reporting it before its key: hold it
                        // until the next key event decides.
                        None if order_unknown => self.lead_text.push(ch),
                        // No key to attach it to: an unmapped key, or a
                        // character that arrived a frame after its key (seen
                        // in VMs). Write it as it is.
                        None => {
                            let mut buf = [0u8; 4];
                            self.response
                                .extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                    }
                }
            }
        }
        self.flush_pending_key(terminal)?;
        self.flush_lead_text();
        self.key_events = events;

        // A held font-size shortcut keeps stepping, as in Ghostty, until its
        // key is released or the modifiers no longer make the same chord.
        if let Some(held) = &mut self.held {
            let same_chord = is_key_down(held.key)
                && shortcuts::action_for(
                    held.key,
                    Self::keyboard_mods(),
                    cfg!(target_os = "macos"),
                ) == Some(held.action);
            if !same_chord {
                self.held = None;
            } else if held.due(Instant::now()) {
                self.actions.push(held.action);
            }
        }
        Ok(())
    }

    /// Characters with no key press after them (a key reported only as a
    /// character): write them as they are.
    fn flush_lead_text(&mut self) {
        self.response.extend_from_slice(self.lead_text.as_bytes());
        self.lead_text.clear();
    }

    /// Learn from the first key with a character whether characters come
    /// just before their key press (Some(true)) or just after it
    /// (Some(false)). Any modifiers count: on X11 Ctrl+C and Ctrl+= still
    /// report 'c' and '=' just before the press, and those are often the
    /// first keys typed. Where Ctrl+C reports U+0003 instead (Windows,
    /// Wayland) it doesn't match, and the order stays unknown.
    fn char_order(events: &[KeyInput]) -> Option<bool> {
        let is_char_of = |event: Option<&KeyInput>, ucp: char| matches!(event, Some(KeyInput::Char(ch)) if ch.to_ascii_lowercase() == ucp);
        events.iter().enumerate().find_map(|(i, event)| {
            let KeyInput::Down {
                keycode,
                repeat: false,
                ..
            } = event
            else {
                return None;
            };
            let (_, ucp) = Self::key_for(*keycode)?;
            if ucp == '\0' {
                return None;
            }
            if i > 0 && is_char_of(events.get(i - 1), ucp) {
                Some(true)
            } else if is_char_of(events.get(i + 1), ucp) {
                Some(false)
            } else {
                None
            }
        })
    }

    /// Encode the key pressed last, with the characters that followed it.
    fn flush_pending_key(&mut self, terminal: &Terminal<'alloc, '_>) -> Result<()> {
        let Some(pending) = self.pending_key.take() else {
            return Ok(());
        };
        let text = (!pending.text.is_empty()).then_some(pending.text.as_str());

        // Windows reports AltGr as Ctrl+Alt. When it produced a character
        // (AltGr+2 gives '@' on Swiss and German layouts), that character is
        // the input: with Ctrl+Alt left in, the encoder sends ESC NUL or a
        // CSI u sequence instead. Marking them consumed isn't enough (it
        // sends NUL), so drop them.
        #[cfg(windows)]
        let mods = if text.is_some() && pending.mods.contains(key::Mods::CTRL | key::Mods::ALT) {
            pending.mods - (key::Mods::CTRL | key::Mods::ALT)
        } else {
            pending.mods
        };
        #[cfg(not(windows))]
        let mods = pending.mods;

        // A Ctrl chord's own character as text ('[' for Ctrl+[ on X11) makes
        // the encoder send CSI u. Not with Alt as well, which is AltGr on
        // Windows and has real text.
        let ctrl_only =
            mods.contains(key::Mods::CTRL) && !mods.intersects(key::Mods::ALT | key::Mods::SUPER);
        let own_char = |t: &str| {
            let mut chars = t.chars();
            chars
                .next()
                .is_some_and(|c| c.eq_ignore_ascii_case(&pending.ucp))
                && chars.next().is_none()
        };
        // Only for keys with a C0 form: Ctrl+; and the like keep their text,
        // or they would send nothing.
        let has_c0 = Self::c0_control(pending.ucp).is_some();
        let text = if ctrl_only && has_c0 && text.is_some_and(own_char) {
            None
        } else {
            text
        };

        let before = self.response.len();
        self.encode_key(
            terminal,
            pending.action,
            pending.key,
            pending.ucp,
            mods,
            text,
        )?;
        if self.response.len() == before {
            if let Some(text) = text {
                // A key the encoder has no sequence for still typed its text.
                self.response.extend_from_slice(text.as_bytes());
            } else if ctrl_only && let Some(byte) = Self::c0_control(pending.ucp) {
                // The encoder sends nothing for Ctrl+[ (it does for Ctrl+]
                // and Ctrl+\). Send the classic control byte: ESC for Ctrl+[.
                self.response.push(byte);
            }
        }
        Ok(())
    }

    /// The C0 control byte a Ctrl chord has always sent in terminals:
    /// Ctrl+@ through Ctrl+_ (letters in either case) map to 0x00-0x1F.
    fn c0_control(ucp: char) -> Option<u8> {
        let upper = ucp.to_ascii_uppercase();
        ('@'..='_').contains(&upper).then_some(upper as u8 & 0x1F)
    }

    fn encode_key(
        &mut self,
        terminal: &Terminal<'alloc, '_>,
        action: key::Action,
        key: Key,
        ucp: char,
        mods: key::Mods,
        text: Option<&str>,
    ) -> Result<()> {
        // Consumed mods are modifiers the platform's text input
        // already accounted for when producing the UTF-8 text.
        // For printable keys, shift is consumed (it turns 'a' → 'A').
        // For non-printable keys nothing is consumed.
        let mut consumed = key::Mods::empty();
        if ucp != '\0' && mods.contains(key::Mods::SHIFT) {
            consumed |= key::Mods::SHIFT;
        }

        self.key_event
            .set_action(action)
            .set_key(key)
            .set_mods(mods)
            .set_consumed_mods(consumed)
            // The unshifted codepoint is the character the key produces
            // with no modifiers.  The Kitty protocol needs it to identify
            // keys independent of the current shift state.
            .set_unshifted_codepoint(ucp)
            .set_utf8(text);

        self.key_encoder
            // Sync encoder options from the terminal so mode changes (e.g.
            // application cursor keys, Kitty keyboard protocol) are honoured.
            .set_options_from_terminal(terminal)
            .encode_to_vec(&self.key_event, &mut self.response)?;
        Ok(())
    }

    /// The libghostty key and unshifted codepoint for a macroquad key code.
    fn key_for(keycode: KeyCode) -> Option<(Key, char)> {
        Self::ALL_KEYS
            .iter()
            .find(|(kc, _, _)| *kc == keycode)
            .map(|&(_, key, ucp)| (key, ucp))
    }

    /// Poll macroquad for mouse events and use the libghostty mouse encoder
    /// to produce the correct VT escape sequences, which are then written
    /// to the pty.  The encoder handles tracking mode (X10, normal, button,
    /// any-event) and output format (X10, UTF8, SGR, URxvt, SGR-Pixels)
    /// based on what the terminal application has requested.
    fn handle_mouse_input(
        &mut self,
        terminal: &mut Terminal<'alloc, '_>,
        dims: Dimensions,
    ) -> Result<()> {
        // Track whether any button is currently held — the encoder uses
        // this to distinguish drags from plain motion.
        let any_pressed = Self::ALL_MOUSE_BUTTONS
            .into_iter()
            .any(|(m, _)| is_mouse_button_down(m));

        let mods = Self::keyboard_mods();

        // The encoder works in surface pixels, which are physical pixels
        // (include/ghostty/vt/mouse/encoder.h). macroquad reports logical
        // points, so scale them. SGR-Pixels reports (mode 1016) then share
        // units with the window size tai reports (ws_xpixel, CSI 14 t), and
        // programs that divide one by the other land on the right cell on
        // Retina displays too. Ghostty's app does the same: its
        // mouse_encode.zig posToPixels (at a887df42) reports surface pixels.
        let scale = dims.dpi_scale;
        let (x, y) = mouse_position();
        self.mouse_event
            .set_mods(mods)
            .set_position(mouse::Position {
                x: x * scale,
                y: y * scale,
            });

        let (cell_width, cell_height) = dims.cell_pixels();
        let padding = (PADDING * scale) as u32;
        self.mouse_encoder
            // Sync encoder tracking mode and format from terminal state so
            // mode changes (e.g. applications enabling SGR mouse reporting)
            // are honoured automatically.
            .set_options_from_terminal(terminal)
            // Provide the encoder with the current terminal geometry so it
            // can convert pixel positions to cell coordinates.
            .set_size(mouse::EncoderSize {
                screen_width: (dims.window_width * scale) as u32,
                screen_height: (dims.window_height * scale) as u32,
                cell_width,
                cell_height,
                padding_top: padding,
                padding_bottom: padding,
                padding_left: padding,
                padding_right: padding,
            })
            .set_any_button_pressed(any_pressed)
            // Enable motion deduplication so the encoder suppresses redundant
            // motion events within the same cell.
            .set_track_last_cell(true);

        // Check each mouse button for press/release events.
        for (mb, btn) in Self::ALL_MOUSE_BUTTONS {
            let action = if is_mouse_button_released(mb) {
                if self.selection.is_selecting(terminal)? {
                    self.selection.on_release(terminal, dims)?;
                    continue;
                }
                mouse::Action::Release
            } else if is_mouse_button_pressed(mb) {
                if mb == MouseButton::Left && Self::selection_allowed_with_mods(terminal, mods)? {
                    self.selection.on_click(terminal, x, y, dims)?;
                    continue;
                }
                mouse::Action::Press
            } else {
                continue;
            };

            self.mouse_event.set_action(action).set_button(Some(btn));
            self.mouse_encoder
                .encode_to_vec(&self.mouse_event, &mut self.response)?;
        }

        // Mouse motion — send a motion event with whatever button is held
        // (or no button for pure motion in any-event tracking mode).
        let delta = mouse_delta_position();
        if delta.x.abs() > 1e-6 || delta.y.abs() > 1e-6 {
            if self.selection.is_selecting(terminal)? {
                self.selection.on_drag(terminal, x, y, dims, mods)?;
                return Ok(());
            }

            self.mouse_event
                .set_action(mouse::Action::Motion)
                .set_button(
                    Self::ALL_MOUSE_BUTTONS
                        .into_iter()
                        .find(|(mb, _)| is_mouse_button_down(*mb))
                        .map(|(_, btn)| btn),
                );
            self.mouse_encoder
                .encode_to_vec(&self.mouse_event, &mut self.response)?;
        }

        // Scroll wheel handling.  When a mouse tracking mode is active the
        // wheel events are forwarded to the application as button 4/5
        // press+release pairs.  Otherwise we scroll the viewport through
        // the scrollback buffer so the user can review history.
        let (wheel_x, wheel_y) = mouse_wheel();
        if wheel_x.abs() > 1e-6 || wheel_y.abs() > 1e-6 {
            if terminal.is_mouse_tracking()? {
                let scroll_btn = if wheel_y > 0.0 {
                    mouse::Button::Four
                } else {
                    mouse::Button::Five
                };

                self.mouse_event
                    .set_button(Some(scroll_btn))
                    .set_action(mouse::Action::Press);
                self.mouse_encoder
                    .encode_to_vec(&self.mouse_event, &mut self.response)?;

                self.mouse_event.set_action(mouse::Action::Release);
                self.mouse_encoder
                    .encode_to_vec(&self.mouse_event, &mut self.response)?;
            } else {
                // Scroll the viewport through scrollback. Adapt
                // the scroll delta to the wheel/touchpad velocity
                // for a comfortable pace.  Delta is negative to scroll
                // up (into history), positive to scroll down.
                let scroll_delta: isize = (wheel_y * -2.5) as isize;
                terminal.scroll_viewport(ScrollViewport::Delta(scroll_delta));
            }
        }
        Ok(())
    }

    /// Build a Mods bitmask from the current macroquad modifier key state.
    fn keyboard_mods() -> key::Mods {
        let mut mods = key::Mods::empty();
        if is_key_down(KeyCode::LeftShift) || is_key_down(KeyCode::RightShift) {
            mods |= key::Mods::SHIFT;
        }
        if is_key_down(KeyCode::LeftAlt) || is_key_down(KeyCode::RightAlt) {
            mods |= key::Mods::ALT;
        }
        if is_key_down(KeyCode::LeftControl) || is_key_down(KeyCode::RightControl) {
            mods |= key::Mods::CTRL;
        }
        if is_key_down(KeyCode::LeftSuper) || is_key_down(KeyCode::RightSuper) {
            mods |= key::Mods::SUPER;
        }
        mods
    }

    // Match Ghostty's rectangular-selection modifier convention so Ghostling feels
    // the same across platforms: Option alone on macOS, Ctrl/Super+Alt elsewhere.
    fn selection_rectangle_mods(mods: key::Mods) -> bool {
        let is_alt = mods.contains(key::Mods::ALT);
        if cfg!(target_os = "macos") {
            is_alt
        } else {
            is_alt && (mods.contains(key::Mods::CTRL) || mods.contains(key::Mods::SUPER))
        }
    }

    /// Decide whether a mouse gesture should manipulate terminal selection or be
    /// forwarded to the running application.  Applications that request mouse
    /// tracking get normal events, but Shift provides the common terminal-emulator
    /// escape hatch for selecting text anyway.
    fn selection_allowed_with_mods(
        terminal: &Terminal<'alloc, '_>,
        mods: key::Mods,
    ) -> Result<bool> {
        let mouse_tracking = terminal.is_mouse_tracking()?;

        // When an application has enabled mouse tracking, normal mouse events
        // belong to the application.  Holding Shift overrides that so users can
        // still select text in full-screen apps such as vim and tmux.
        Ok(!mouse_tracking || mods.contains(key::Mods::SHIFT))
    }

    /// Convert the current mouse position to a clamped viewport grid point.
    /// Clamping lets users start or continue a drag slightly outside the padded
    /// terminal area while still extending to the nearest visible cell.
    fn mouse_viewport_point(terminal: &Terminal<'alloc, '_>, dims: Dimensions) -> Result<Point> {
        let cols = terminal.cols()?;
        let rows = terminal.rows()?;
        let (xpos, ypos) = mouse_position();

        // Clamp to the last cell, not one past it: grid_ref rejects x == cols
        // or y == rows with InvalidValue, and the right and bottom padding
        // (where a window-corner drag starts) map there.
        let x = if xpos > PADDING {
            ((xpos - PADDING) / dims.cell_width).clamp(0.0, cols.saturating_sub(1) as f32) as u16
        } else {
            0
        };
        let y = if ypos > PADDING {
            ((ypos - PADDING) / dims.cell_height).clamp(0.0, rows.saturating_sub(1) as f32) as u32
        } else {
            0
        };

        Ok(Point::Viewport(PointCoordinate { x, y }))
    }

    // Decide whether the mouse is over an area where a selection drag can begin.
    // Blank cells are included because terminal selections can start in empty grid
    // space; the check is about selectable geometry, not text contents.
    fn mouse_over_selectable_text(
        terminal: &Terminal<'alloc, '_>,
        dims: Dimensions,
    ) -> Result<bool> {
        // Test the real terminal grid bounds rather than using the window dimensions:
        // padding and any partially visible trailing pixels are not selectable cells,
        // and the I-beam cursor should not appear there.
        let cols = terminal.cols()?;
        let rows = terminal.rows()?;

        let (xpos, ypos) = mouse_position();
        let left = PADDING;
        let top = PADDING;
        let right = left + cols as f32 * dims.cell_width;
        let bottom = top + rows as f32 * dims.cell_height;

        if xpos < left || xpos >= right || ypos < top || ypos >= bottom {
            return Ok(false);
        }

        if !Self::selection_allowed_with_mods(terminal, Self::keyboard_mods())? {
            return Ok(false);
        }
        Ok(true)
    }

    /// All macroquad mouse buttons we want to check with their libghostty equivalent.
    const ALL_MOUSE_BUTTONS: [(MouseButton, mouse::Button); 3] = [
        (MouseButton::Left, mouse::Button::Left),
        (MouseButton::Right, mouse::Button::Right),
        (MouseButton::Middle, mouse::Button::Middle),
    ];

    /// All macroquad keys we want to check for press/repeat/release events,
    /// with their libghostty equivalent and their unshifted Unicode codepoint,
    /// i.e. character the key produces with no modifiers on a US layout. The
    /// Kitty keyboard protocol requires this to identify keys. Returns NUL
    /// for keys that don't have a natural codepoint (arrows, F-keys, etc.).
    const ALL_KEYS: [(KeyCode, Key, char); 75] = [
        (KeyCode::A, Key::A, 'a'),
        (KeyCode::B, Key::B, 'b'),
        (KeyCode::C, Key::C, 'c'),
        (KeyCode::D, Key::D, 'd'),
        (KeyCode::E, Key::E, 'e'),
        (KeyCode::F, Key::F, 'f'),
        (KeyCode::G, Key::G, 'g'),
        (KeyCode::H, Key::H, 'h'),
        (KeyCode::I, Key::I, 'i'),
        (KeyCode::J, Key::J, 'j'),
        (KeyCode::K, Key::K, 'k'),
        (KeyCode::L, Key::L, 'l'),
        (KeyCode::M, Key::M, 'm'),
        (KeyCode::N, Key::N, 'n'),
        (KeyCode::O, Key::O, 'o'),
        (KeyCode::P, Key::P, 'p'),
        (KeyCode::Q, Key::Q, 'q'),
        (KeyCode::R, Key::R, 'r'),
        (KeyCode::S, Key::S, 's'),
        (KeyCode::T, Key::T, 't'),
        (KeyCode::U, Key::U, 'u'),
        (KeyCode::V, Key::V, 'v'),
        (KeyCode::W, Key::W, 'w'),
        (KeyCode::X, Key::X, 'x'),
        (KeyCode::Y, Key::Y, 'y'),
        (KeyCode::Z, Key::Z, 'z'),
        (KeyCode::Key0, Key::Digit0, '0'),
        (KeyCode::Key1, Key::Digit1, '1'),
        (KeyCode::Key2, Key::Digit2, '2'),
        (KeyCode::Key3, Key::Digit3, '3'),
        (KeyCode::Key4, Key::Digit4, '4'),
        (KeyCode::Key5, Key::Digit5, '5'),
        (KeyCode::Key6, Key::Digit6, '6'),
        (KeyCode::Key7, Key::Digit7, '7'),
        (KeyCode::Key8, Key::Digit8, '8'),
        (KeyCode::Key9, Key::Digit9, '9'),
        (KeyCode::Space, Key::Space, ' '),
        (KeyCode::Enter, Key::Enter, '\0'),
        (KeyCode::Tab, Key::Tab, '\0'),
        (KeyCode::Backspace, Key::Backspace, '\0'),
        (KeyCode::Delete, Key::Delete, '\0'),
        (KeyCode::Escape, Key::Escape, '\0'),
        (KeyCode::Up, Key::ArrowUp, '\0'),
        (KeyCode::Down, Key::ArrowDown, '\0'),
        (KeyCode::Left, Key::ArrowLeft, '\0'),
        (KeyCode::Right, Key::ArrowRight, '\0'),
        (KeyCode::Home, Key::Home, '\0'),
        (KeyCode::End, Key::End, '\0'),
        (KeyCode::PageUp, Key::PageUp, '\0'),
        (KeyCode::PageDown, Key::PageDown, '\0'),
        (KeyCode::Insert, Key::Insert, '\0'),
        (KeyCode::Minus, Key::Minus, '-'),
        (KeyCode::Equal, Key::Equal, '='),
        (KeyCode::LeftBracket, Key::BracketLeft, '['),
        (KeyCode::RightBracket, Key::BracketRight, ']'),
        (KeyCode::Backslash, Key::Backslash, '\\'),
        (KeyCode::Semicolon, Key::Semicolon, ';'),
        (KeyCode::Apostrophe, Key::Quote, '\''),
        (KeyCode::Comma, Key::Comma, ','),
        (KeyCode::Period, Key::Period, '.'),
        (KeyCode::Slash, Key::Slash, '/'),
        (KeyCode::GraveAccent, Key::Backquote, '`'),
        (KeyCode::F1, Key::F1, '\0'),
        (KeyCode::F2, Key::F2, '\0'),
        (KeyCode::F3, Key::F3, '\0'),
        (KeyCode::F4, Key::F4, '\0'),
        (KeyCode::F5, Key::F5, '\0'),
        (KeyCode::F6, Key::F6, '\0'),
        (KeyCode::F7, Key::F7, '\0'),
        (KeyCode::F8, Key::F8, '\0'),
        (KeyCode::F9, Key::F9, '\0'),
        (KeyCode::F10, Key::F10, '\0'),
        (KeyCode::F11, Key::F11, '\0'),
        (KeyCode::F12, Key::F12, '\0'),
        (KeyCode::KpEnter, Key::NumpadEnter, '\0'),
    ];
}

#[derive(Debug)]
struct SelectionState<'alloc> {
    gesture: Gesture<'alloc>,
    press_event: PressEvent<'alloc>,
    release_event: ReleaseEvent<'alloc>,
    drag_event: DragEvent<'alloc>,
    start_time: Instant,
    /// The cursor last set, so it is only set again when it changes.
    cursor: Cell<Option<CursorIcon>>,
}

impl SelectionState<'_> {
    fn new() -> Result<Self> {
        let mut press_event = PressEvent::new()?;
        press_event.set_repeat_interval(Duration::from_millis(500))?;

        Ok(Self {
            gesture: Gesture::new()?,
            press_event,
            release_event: ReleaseEvent::new()?,
            drag_event: DragEvent::new()?,
            start_time: Instant::now(),
            cursor: Cell::new(None),
        })
    }

    /// Whether we currently have a selection and the mouse is kept down.
    fn is_selecting(&self, terminal: &Terminal<'_, '_>) -> Result<bool> {
        Ok(self.gesture.click_count(terminal)? > 0 && is_mouse_button_down(MouseButton::Left))
    }

    /// Update the selection state upon a left click.
    fn on_click(
        &mut self,
        terminal: &Terminal<'_, '_>,
        x: f32,
        y: f32,
        dims: Dimensions,
    ) -> Result<()> {
        let point = Input::mouse_viewport_point(terminal, dims)?;
        let grid_ref = terminal.grid_ref(point)?;

        let selection = self
            .press_event
            .set_repeat_distance(dims.cell_width.into())?
            .set_time(self.start_time.elapsed())?
            .set_position(x.into(), y.into())?
            .apply(&mut self.gesture, terminal, grid_ref)?;

        terminal.set_selection(selection.as_ref())?;
        Ok(())
    }

    /// Potentially reset the selection state when the mouse is released.
    fn on_release(&mut self, terminal: &Terminal<'_, '_>, dims: Dimensions) -> Result<()> {
        let point = Input::mouse_viewport_point(terminal, dims)?;
        // The point might fall outside the grid, which is completely okay in this case.
        let grid_ref = terminal.grid_ref(point).ok();

        self.release_event
            .apply(&mut self.gesture, terminal, grid_ref)?;
        Ok(())
    }

    /// Update the selection state during and after a mouse drag.
    fn on_drag(
        &mut self,
        terminal: &Terminal<'_, '_>,
        x: f32,
        y: f32,
        dims: Dimensions,
        mods: key::Mods,
    ) -> Result<()> {
        let point = Input::mouse_viewport_point(terminal, dims)?;
        let grid_ref = terminal.grid_ref(point)?;
        let geometry = Geometry {
            columns: terminal.cols()?.into(),
            cell_width: dims.cell_width as u32,
            padding_left: PADDING as u32,
            screen_height: dims.window_height as u32,
        };

        let selection = self
            .drag_event
            .set_rectangle(Input::selection_rectangle_mods(mods))?
            .set_position(x.into(), y.into())?
            .apply(&mut self.gesture, terminal, grid_ref, geometry)?;

        terminal.set_selection(selection.as_ref())?;
        Ok(())
    }

    /// Update the platform cursor to advertise selection affordance.
    ///
    /// This is kept separate from [`Input::handle_mouse`] so the cursor
    /// changes on hover even when no mouse button event is being processed.
    fn update_mouse_cursor(
        &self,
        terminal: &Terminal<'_, '_>,
        dims: Dimensions,
        input_enabled: bool,
    ) -> Result<()> {
        let selectable = input_enabled
            && (self.is_selecting(terminal)? || Input::mouse_over_selectable_text(terminal, dims)?);
        let rectangle = Input::selection_rectangle_mods(Input::keyboard_mods());

        let cursor = if selectable {
            if rectangle {
                CursorIcon::Crosshair
            } else {
                CursorIcon::Text
            }
        } else {
            CursorIcon::Default
        };
        // On Windows set_mouse_cursor calls SetCursor right away. Doing that
        // every frame overwrote the resize arrows Windows shows on the
        // window border, so the cursor flickered and the border was hard
        // to grab.
        if self.cursor.get() != Some(cursor) {
            set_mouse_cursor(cursor);
            self.cursor.set(Some(cursor));
        }
        Ok(())
    }
}

/// Configuration for macroquad.
///
/// By default the window is resizable and DPI-aware,
/// while prioritizing Wayland over X11 on Linux.
/// On Apple platforms, `TAI_APPLE_GFX=metal` selects Metal instead of OpenGL.
/// Metal stays opt-in: on the macOS dev host it leaked GPU memory steadily
/// (about 1.2 MB/s while htop redrew) where OpenGL stayed flat.
fn macroquad_conf() -> Conf {
    let apple_gfx_api = if use_metal() {
        conf::AppleGfxApi::Metal
    } else {
        conf::AppleGfxApi::OpenGl
    };
    Conf {
        window_title: "tai".to_owned(),
        window_resizable: true,
        high_dpi: true,
        platform: conf::Platform {
            // Default to Wayland and fallback to X11 on Linux.
            linux_backend: conf::LinuxBackend::WaylandWithX11Fallback,
            apple_gfx_api,
            ..Default::default()
        },
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// System callbacks (process-global, set once at startup)
// ---------------------------------------------------------------------------

/// A decoder that decodes raw PNG data into 8-bit RGBA pixels using
/// `macroquad`'s PNG decoder. The output buffer is allocated through the
/// provided `Allocator` so the library can free it later.
struct PngDecoder;

impl DecodePng for PngDecoder {
    fn decode_png<'alloc>(
        &mut self,
        alloc: &'alloc libghostty_vt::alloc::Allocator<'_>,
        data: &[u8],
    ) -> Option<DecodedImage<'alloc>> {
        // `macroquad` decodes the PNG image for us and converts it
        // to the correct RGBA8 pixel format automatically.
        let img = Image::from_file_with_format(data, Some(ImageFormat::Png)).ok()?;
        let mut data = Bytes::new_with_alloc(alloc, img.bytes.len()).ok()?;
        data.copy_from_slice(&img.bytes);

        Some(DecodedImage {
            width: img.width as u32,
            height: img.height as u32,
            data,
        })
    }
}

/// This is a full implementation of a pseudo-terminal (PTY) interface.
///
/// Normally we would try to avoid using unsafe code in examples,
/// but pseudo-terminals are very hard to use safely in the general case
/// and as such the standard library does not handle them at all.
///
/// Of course, you could just use any off-the-shelf PTY crate on crates.io,
/// but we'd like to keep this demo self-contained using minimal dependencies,
/// so here you go.
///
/// Windows has no forkpty; `pty_windows.rs` implements the same interface
/// on ConPTY (ADR 0002).
#[cfg(unix)]
mod pty {
    // Unfortunately, there will be some unsafe shenanigans here.
    #![allow(unsafe_code)]
    use std::{
        os::{
            fd::{AsRawFd, OwnedFd},
            unix::process::CommandExt,
        },
        path::PathBuf,
        process::Command,
        sync::{
            Mutex,
            mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
        },
        time::{Duration, Instant},
    };

    use nix::{
        errno::Errno,
        fcntl::{self, OFlag},
        pty::ForkptyResult,
        sys::{signal, wait},
        unistd::{self, Pid},
    };

    use crate::Dimensions;

    type Chunk = Result<Vec<u8>, PtyError>;

    /// Chunks the reader thread may queue before it stops reading, which
    /// makes a flooding child wait instead of tai's memory growing.
    const QUEUED_CHUNKS: usize = 64;

    /// Handle to a pseudo-terminal (PTY).
    ///
    /// A reader thread drains the master into a channel as soon as output
    /// arrives, as Ghostty's read thread does (`src/termio/Exec.zig`,
    /// `ReadThread`, at a887df42). Reading only once per frame left the
    /// child blocked on a full pty buffer while tai drew, which capped a
    /// full-screen TUI at tai's frame rate. The terminal stays on the main
    /// thread: libghostty-vt's types are `!Send`.
    pub struct Pty {
        fd: OwnedFd,
        output: Receiver<Chunk>,
        /// A chunk wait_readable took from `output`, for the next read.
        waited: Mutex<Option<Chunk>>,
    }

    impl Pty {
        /// Spawn the user's default shell in a new pseudo-terminal.
        ///
        /// Creates a pty pair via forkpty(), sets the initial window size, execs the
        /// shell in the child, and puts the master fd into non-blocking mode so we
        /// can poll it each frame without stalling the render loop.
        ///
        /// The shell is chosen by checking, in order:
        ///   1. $SHELL environment variable
        ///   2. The pw_shell field from the passwd database
        ///   3. /bin/sh as a last resort
        pub fn new(dims: Dimensions) -> std::io::Result<(Self, Child)> {
            // Work out the shell's locale before forking: it may run
            // `defaults` on macOS, and it logs (ADR 0003).
            let locale = crate::locale::child_env();
            // Other terminals' identity variables to drop (ADR 0005).
            let foreign = crate::identity::foreign_vars(std::env::vars_os());

            // forkpty() combines openpty + fork + login_tty into one call.
            // In the child it sets up the slave side as stdin/stdout/stderr.
            match unsafe { nix::pty::forkpty(&dims.to_winsize(), None)? } {
                // Child process -- replace ourselves with the shell.
                // TERM tells programs what escape sequences we understand.
                ForkptyResult::Child => {
                    // Give the shell default signal handling. Ignored signals
                    // survive exec, so if whatever launched tai ignored SIGINT
                    // (a background job does), Ctrl+C would never interrupt
                    // anything in the shell.
                    for sig in [
                        signal::Signal::SIGINT,
                        signal::Signal::SIGQUIT,
                        signal::Signal::SIGTSTP,
                        signal::Signal::SIGTTIN,
                        signal::Signal::SIGTTOU,
                        signal::Signal::SIGPIPE,
                        signal::Signal::SIGCHLD,
                    ] {
                        _ = unsafe { signal::signal(sig, signal::SigHandler::SigDfl) };
                    }
                    _ = signal::sigprocmask(
                        signal::SigmaskHow::SIG_SETMASK,
                        Some(&signal::SigSet::empty()),
                        None,
                    );

                    // Determine the user's preferred shell. We try $SHELL first (the
                    // standard convention), then fall back to the passwd entry, and
                    // finally to /bin/sh if nothing else is available.
                    let shell = match std::env::var_os("SHELL") {
                        Some(shell) if !shell.is_empty() => PathBuf::from(shell),
                        _ => match unistd::User::from_uid(unistd::getuid()) {
                            Ok(Some(user)) => user.shell,
                            _ => PathBuf::from("/bin/sh"),
                        },
                    };

                    // Extract just the program name for argv[0] (e.g. "/bin/zsh" -> "zsh").
                    let name = shell.file_name().unwrap_or(shell.as_os_str());

                    // On macOS, start a login shell ("-zsh") like Terminal.app does,
                    // so ~/.zprofile and path_helper set up PATH.
                    let mut arg0 = std::ffi::OsString::new();
                    if cfg!(target_os = "macos") {
                        arg0.push("-");
                    }
                    arg0.push(name);

                    // Replace the child process with the user's shell via `exec`.
                    let mut command = Command::new(&shell);
                    for name in &foreign {
                        command.env_remove(name);
                    }
                    _ = command
                        .env("TERM", "xterm-256color")
                        .env("COLORTERM", "truecolor")
                        // Identify tai, not the terminal that started it.
                        .envs(crate::identity::vars())
                        // A UTF-8 character type; see src/locale.rs.
                        .envs(locale.iter().map(|(k, v)| (k, v)))
                        .arg0(arg0)
                        .exec();

                    // `exec` only returns on error.
                    std::process::exit(127);
                }

                // Parent -- make the master fd non-blocking so read() returns EAGAIN
                // instead of blocking when there's no data, letting us poll each frame.
                ForkptyResult::Parent { child, master: fd } => {
                    let raw_flags = fcntl::fcntl(&fd, fcntl::F_GETFL)?;
                    let flags = OFlag::from_bits_retain(raw_flags) | OFlag::O_NONBLOCK;
                    _ = fcntl::fcntl(&fd, fcntl::F_SETFL(flags))?;

                    let reader = fd.try_clone()?;
                    let (tx, output) = mpsc::sync_channel::<Chunk>(QUEUED_CHUNKS);
                    std::thread::Builder::new()
                        .name("pty-reader".into())
                        .spawn(move || read_loop(&reader, &tx))?;

                    let pty = Self {
                        fd,
                        output,
                        waited: Mutex::new(None),
                    };
                    Ok((pty, Child::Active(child)))
                }
            }
        }

        /// Wait until output is available, or `timeout` passes. The reader
        /// thread's channel has no peek, so a chunk that arrives is kept for
        /// the next read.
        pub fn wait_readable(&self, timeout: Duration) {
            let Ok(mut waited) = self.waited.lock() else {
                return;
            };
            if waited.is_some() {
                return;
            }
            match self.output.recv_timeout(timeout) {
                Ok(chunk) => *waited = Some(chunk),
                // read() reports a closed channel.
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
            }
        }

        /// Pass the output received so far to `sink`, until `deadline`; the
        /// rest waits for the next frame.
        pub fn read(&self, sink: &mut dyn FnMut(&[u8]), deadline: Instant) -> Result<(), PtyError> {
            if let Some(chunk) = self.waited.lock().ok().and_then(|mut w| w.take()) {
                sink(&chunk?);
            }
            while Instant::now() < deadline {
                match self.output.try_recv() {
                    Ok(chunk) => sink(&chunk?),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Err(PtyError::EndOfStream),
                }
            }
            Ok(())
        }

        /// Best-effort write to the pty master fd. Because the fd is
        /// non-blocking, write() may return short or fail with EAGAIN.
        /// We retry on EINTR, advance past partial writes, and silently
        /// drop data if the kernel buffer is full -- this matches what most
        /// terminal emulators do under back-pressure.
        pub fn write(&self, data: &[u8]) {
            let mut remaining = data;
            while !remaining.is_empty() {
                match nix::unistd::write(&self.fd, remaining) {
                    Ok(len) => remaining = &remaining[len..],
                    Err(Errno::EINTR) => continue,
                    // EAGAIN or real error -- drop the remainder.
                    Err(_) => break,
                }
            }
        }

        /// Send a resize event to the pty via the TIOCSWINSZ ioctl.
        ///
        /// ioctl is a way to control a resource without inventing a dedicated
        /// system call for it, and in this case we have to generate bindings
        /// for this ioctl and feed the new window size to it.
        pub fn resize(&self, dims: Dimensions) {
            nix::ioctl_write_ptr_bad!(tiocswinsz, nix::libc::TIOCSWINSZ, nix::pty::Winsize);
            _ = unsafe { tiocswinsz(self.fd.as_raw_fd(), &dims.to_winsize()) };
        }
    }

    /// The reader thread: wait for output on the (non-blocking) master, read
    /// all of it, and queue it for the frame loop. It ends at end of stream,
    /// on an error, or when the Pty is dropped.
    fn read_loop(fd: &OwnedFd, tx: &mpsc::SyncSender<Chunk>) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut fds = [nix::libc::pollfd {
                fd: fd.as_raw_fd(),
                events: nix::libc::POLLIN,
                revents: 0,
            }];
            // Errors other than EINTR show up on the read below.
            _ = unsafe { nix::libc::poll(fds.as_mut_ptr(), 1, -1) };
            loop {
                let chunk = match unistd::read(fd, &mut buf) {
                    Ok(len) if len > 0 => Ok(buf[..len].to_vec()),
                    Err(Errno::EAGAIN) => break,
                    Err(Errno::EINTR) => continue,
                    // EOF -- the child closed its side of the pty. On Linux,
                    // the slave closing often produces EIO rather than a
                    // clean EOF (read returning 0). Treat it the same.
                    Ok(_) | Err(Errno::EIO) => Err(PtyError::EndOfStream),
                    Err(err) => Err(PtyError::OtherError(err)),
                };
                let last = chunk.is_err();
                if tx.send(chunk).is_err() || last {
                    return;
                }
            }
        }
    }

    /// The child process within a pseudo-terminal.
    ///
    /// The child can be in one of three states: active (default), exited
    /// and reaped. The parent process has to wait for the child to fully
    /// exit, which is why it tries to "reap" the child process by waiting
    /// for it to exit via `waitpid`. Once it has exited fully, it will be
    /// considered "reaped".
    pub enum Child {
        Active(Pid),
        Exited(Pid),
        Reaped(wait::WaitStatus),
    }

    impl Child {
        /// Record that the child's side of the pty has closed.
        pub fn mark_exited(&mut self) {
            if let Child::Active(pid) = *self {
                *self = Child::Exited(pid);
            }
        }

        /// Reap an exited child without blocking.
        pub fn try_reap(&mut self) {
            if let Child::Exited(pid) = *self
                && let Ok(wp) = wait::waitpid(pid, Some(wait::WaitPidFlag::WNOHANG))
            {
                *self = Child::Reaped(wp);
            }
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            // If the child is still running, ask it to exit. Then do a
            // blocking waitpid to reap it and avoid leaving a zombie.
            match *self {
                Child::Active(pid) => {
                    let _ = signal::kill(pid, signal::SIGHUP);
                    let _ = wait::waitpid(pid, None);
                }
                Child::Exited(pid) => {
                    let _ = wait::waitpid(pid, None);
                }
                Child::Reaped(_) => {}
            }
        }
    }

    #[derive(Clone, Debug)]
    pub enum PtyError {
        EndOfStream,
        OtherError(nix::errno::Errno),
    }
}

#[cfg(test)]
mod tests {
    use super::{Dimensions, Input, PADDING};

    fn dims(window: (f32, f32), cell: (f32, f32), dpi_scale: f32) -> Dimensions {
        Dimensions {
            window_width: window.0,
            window_height: window.1,
            cell_width: cell.0,
            cell_height: cell.1,
            font_size: 16,
            baseline: 0.0,
            dpi_scale,
        }
    }

    #[test]
    fn pty_pixels_cover_whole_cells_only() {
        // An 800x600 window with 6x20 cells and 6 px of padding: 131 cols
        // (786 px) and 29 rows (580 px), not the window's 800x600.
        let d = dims((800.0, 600.0), (6.0, 20.0), 1.0);
        assert_eq!(d.grid_size(), (131, 29));
        assert_eq!(d.grid_pixels(), (786, 580));
        // At 2x the pixels double with the cells, and pixels / cells is
        // exactly the cell size the terminal reports (CSI 16 t).
        let d = dims((800.0, 600.0), (6.5, 19.5), 2.0);
        let ((cols, rows), (w, h)) = (d.grid_size(), d.grid_pixels());
        assert_eq!((u32::from(w / cols), u32::from(h / rows)), d.cell_pixels());
        assert_eq!((w % cols, h % rows), (0, 0));
        assert!(f32::from(w) <= (800.0 - 2.0 * PADDING) * 2.0);
    }

    #[test]
    fn in_band_size_report_matches_resize() {
        use libghostty_vt::{Terminal, TerminalOptions};
        use std::{cell::RefCell, rc::Rc};
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 0,
        })
        .unwrap();
        let sent = Rc::new(RefCell::new(Vec::new()));
        let s = sent.clone();
        terminal
            .on_pty_write(move |_t, data| s.borrow_mut().extend_from_slice(data))
            .unwrap();
        terminal.vt_write(b"\x1b[?2048h");
        // Setting the mode sends nothing; tai has to.
        assert!(sent.borrow().is_empty());
        // libghostty-vt's own report on resize is what tai's must match.
        terminal.resize(100, 30, 9, 18).unwrap();
        let expected = super::in_band_size_report((100, 30), (9, 18));
        assert_eq!(expected, "\x1b[48;30;100;540;900t");
        assert_eq!(*sent.borrow(), expected.as_bytes());
    }

    #[test]
    fn arrow_with_macos_function_key_text() {
        use libghostty_vt::{
            Terminal, TerminalOptions,
            key::{self, Key},
        };
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 0,
        })
        .unwrap();
        // Kitty keyboard protocol, disambiguate (what ppxaide pushes).
        terminal.vt_write(b"\x1b[>1u");
        let mut encoder = key::Encoder::new().unwrap();
        let mut event = key::Event::new().unwrap();
        let mut encode = |text: Option<&str>| {
            event
                .set_action(key::Action::Press)
                .set_key(Key::ArrowDown)
                .set_mods(key::Mods::empty())
                .set_consumed_mods(key::Mods::empty())
                .set_unshifted_codepoint('\0')
                .set_utf8(text);
            let mut out = Vec::new();
            encoder
                .set_options_from_terminal(&terminal)
                .encode_to_vec(&event, &mut out)
                .unwrap();
            out
        };
        // Without text the arrow is its CSI sequence; with macOS's function
        // key character as text it would be that character, which is why
        // is_function_key_char drops it.
        assert_eq!(encode(None), b"\x1b[B");
        assert_eq!(encode(Some("\u{f701}")), "\u{f701}".as_bytes());
        assert!(super::is_function_key_char('\u{f701}'));
        assert!(!super::is_function_key_char('a'));
    }

    #[test]
    fn ctrl_chords_map_to_c0_controls() {
        assert_eq!(Input::c0_control('['), Some(0x1B));
        assert_eq!(Input::c0_control('c'), Some(0x03));
        assert_eq!(Input::c0_control('@'), Some(0x00));
        assert_eq!(Input::c0_control('_'), Some(0x1F));
        assert_eq!(Input::c0_control('1'), None);
        assert_eq!(Input::c0_control('='), None);
    }
}
