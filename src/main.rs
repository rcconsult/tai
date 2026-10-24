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
    style::RgbColor,
    terminal::{
        ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode, Point,
        PointCoordinate, PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes,
        SizeReportSize,
    },
};

use crate::fonts::{Coverage, Face, Fonts, is_space};
use crate::pty::{Child, Pty, PtyError};
use crate::shortcuts::Action;

mod clipboard;
mod fonts;
mod locale;
#[cfg(windows)]
#[path = "pty_windows.rs"]
mod pty;
mod shortcuts;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

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
    let mut fonts = Fonts::load()?;
    let coverage = Coverage::new()?;

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

    // Spawn a child shell connected to a pseudo-terminal.
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
        .on_device_attributes(|_term| {
            Some(DeviceAttributes {
                // DA1: VT220-level with a few common features.
                primary: PrimaryDeviceAttributes::new(
                    ConformanceLevel::VT220,
                    &[
                        DeviceAttributeFeature::COLUMNS_132,
                        DeviceAttributeFeature::SELECTIVE_ERASE,
                        DeviceAttributeFeature::ANSI_COLOR,
                    ],
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

    // Each frame: handle resize → read pty → process input → render.
    loop {
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
                                // Reload the fonts so the glyph atlases of the
                                // old size are dropped instead of growing per size.
                                fonts = Fonts::load()?;
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

                match pty.read(&mut terminal) {
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

        // Draw the current terminal screen.
        renderer.render_terminal(&terminal, dims, &fonts, &coverage)?;
        next_frame().await
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
}

impl<'alloc> Renderer<'alloc> {
    fn new() -> Result<Self> {
        Ok(Self {
            render_state: RenderState::new()?,
            row_it: RowIterator::new()?,
            cell_it: CellIterator::new()?,
            placement_it: PlacementIterator::new()?,
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
    ) -> Result<()> {
        // Snapshot the terminal state into our render state. This is the
        // only point where we need access to the terminal; after this the
        // snapshot owns everything we need to draw the frame.
        //
        // When a snapshot is active, the render state cannot be updated —
        // this is upheld in the Rust API by making the snapshot take an
        // exclusive (mutable) reference to the render state.
        let snapshot = self.render_state.update(terminal)?;

        // Get the terminal's background color from the render state snapshot.
        let colors = snapshot.colors()?;
        clear_background(color(colors.background));

        // Obtain the Kitty graphics storage from the terminal. This is a
        // reference valid until the next mutating terminal call.
        let graphics = terminal.kitty_graphics();

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

        // For convenience, `next` gives you the same iteration back only
        // as a shared pointer, so you can simultaneously iterate through
        // all rows while having a handle to query data for each row.
        while let Some(row) = row_it.next() {
            let mut x = PADDING;
            // Whether the last icon is waiting to learn if the next cell is blank.
            let mut icon_open = false;

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

                if graphemes == 0 {
                    if selected {
                        draw_rectangle(
                            x,
                            y,
                            dims.cell_width,
                            dims.cell_height,
                            color(colors.foreground),
                        );
                    } else if let Some(bg) = bg {
                        // The cell has no text, but it might have a background
                        // color (e.g. from an erase with a color set).
                        draw_rectangle(x, y, dims.cell_width, dims.cell_height, color(bg));
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
                        draw_rectangle(x, y, dims.cell_width, dims.cell_height, color(bg));
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
                        let baseline = y + dims.baseline;
                        draw_glyph(
                            &text,
                            x,
                            baseline,
                            &fonts.primary,
                            dims.font_size,
                            color(fg),
                            bold,
                        );
                    }
                }

                x += dims.cell_width;
                continue;
            }

            for icon in icons.drain(..) {
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

            // Clear per-row dirty flag after rendering it.
            row.set_dirty(false)?;
            y += dims.cell_height;
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
            )?
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
        nix::pty::Winsize {
            ws_col: cols,
            ws_row: rows,
            ws_xpixel: (self.window_width * self.dpi_scale) as u16,
            ws_ypixel: (self.window_height * self.dpi_scale) as u16,
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
                    if ch.is_control() {
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

        let before = self.response.len();
        self.encode_key(
            terminal,
            pending.action,
            pending.key,
            pending.ucp,
            mods,
            text,
        )?;
        // A key the encoder has no sequence for still typed its text.
        if self.response.len() == before
            && let Some(text) = text
        {
            self.response.extend_from_slice(text.as_bytes());
        }
        Ok(())
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

        let (x, y) = mouse_position();
        self.mouse_event
            .set_mods(mods)
            .set_position(mouse::Position { x, y });

        self.mouse_encoder
            // Sync encoder tracking mode and format from terminal state so
            // mode changes (e.g. applications enabling SGR mouse reporting)
            // are honoured automatically.
            .set_options_from_terminal(terminal)
            // Provide the encoder with the current terminal geometry so it
            // can convert pixel positions to cell coordinates.
            .set_size(mouse::EncoderSize {
                screen_width: dims.window_width as u32,
                screen_height: dims.window_height as u32,
                cell_width: dims.cell_width as u32,
                cell_height: dims.cell_height as u32,
                padding_top: PADDING as u32,
                padding_bottom: PADDING as u32,
                padding_left: PADDING as u32,
                padding_right: PADDING as u32,
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
    const ALL_KEYS: [(KeyCode, Key, char); 74] = [
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
    let apple_gfx_api = match std::env::var("TAI_APPLE_GFX").as_deref() {
        Ok("metal") => conf::AppleGfxApi::Metal,
        _ => conf::AppleGfxApi::OpenGl,
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
    };

    use libghostty_vt::Terminal;
    use nix::{
        errno::Errno,
        fcntl::{self, OFlag},
        pty::ForkptyResult,
        sys::{signal, wait},
        unistd::{self, Pid},
    };

    use crate::Dimensions;

    /// Handle to a pseudo-terminal (PTY).
    pub struct Pty(OwnedFd);

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
                    _ = Command::new(&shell)
                        .env("TERM", "xterm-256color")
                        .env("COLORTERM", "truecolor")
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

                    Ok((Self(fd), Child::Active(child)))
                }
            }
        }

        /// Wait until the pty master has output to read, or `timeout` passes.
        pub fn wait_readable(&self, timeout: std::time::Duration) {
            let mut fds = [nix::libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: nix::libc::POLLIN,
                revents: 0,
            }];
            let ms = timeout.as_millis().try_into().unwrap_or(i32::MAX);
            // An error or a timeout both just mean "read what there is".
            _ = unsafe { nix::libc::poll(fds.as_mut_ptr(), 1, ms) };
        }

        /// Drain all available output from the pty master and feed it into the
        /// ghostty terminal. The terminal's VT parser will process any escape
        /// sequences and update its internal screen/cursor/style state.
        ///
        /// Because the fd is non-blocking, read() returns an error with
        /// EAGAIN once the kernel buffer is empty, at which point we stop.
        pub fn read(&self, term: &mut Terminal) -> Result<(), PtyError> {
            let mut buf = [0u8; 4096];
            loop {
                match nix::unistd::read(&self.0, &mut buf) {
                    // EOF -- the child closed its side of the pty.
                    Ok(0) => return Err(PtyError::EndOfStream),
                    Ok(len) => term.vt_write(&buf[..len]),

                    // Distinguish "no data right now" from real errors.
                    Err(Errno::EAGAIN) => return Ok(()),
                    Err(Errno::EINTR) => continue, // retry the read
                    // On Linux, the slave closing often produces EIO rather
                    // than a clean EOF (read returning 0). Treat it the same.
                    Err(Errno::EIO) => return Err(PtyError::EndOfStream),
                    Err(err) => return Err(PtyError::OtherError(err)),
                };
            }
        }

        /// Best-effort write to the pty master fd. Because the fd is
        /// non-blocking, write() may return short or fail with EAGAIN.
        /// We retry on EINTR, advance past partial writes, and silently
        /// drop data if the kernel buffer is full -- this matches what most
        /// terminal emulators do under back-pressure.
        pub fn write(&self, data: &[u8]) {
            let mut remaining = data;
            while !remaining.is_empty() {
                match nix::unistd::write(&self.0, remaining) {
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
            _ = unsafe { tiocswinsz(self.0.as_raw_fd(), &dims.to_winsize()) };
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
