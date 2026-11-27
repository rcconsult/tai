//! Kitty images in a terminal, for tai and tai-gui alike (ADR 0014, step 2):
//! accepting them, and working out what to draw where. Each front end
//! keeps only its textures and one textured-rectangle draw.
//!
//! What a front end does with this:
//! 1. `install_png_decoder` once, before any terminal exists, and `enable`
//!    on each terminal.
//! 2. Feed the pty's output through `crate::sixel::Translator`, which turns
//!    Sixel into Kitty images (ADR 0006, 0009).
//! 3. While reading a row, keep each cell's `placeholder_cell` on a row that
//!    has Unicode placeholders, and turn them into runs with
//!    `crate::placeholders::row_placements`.
//! 4. Each frame, take `frame_draws` and draw each `Stage` at its place:
//!    `BelowBackground` before the cell backgrounds, `BelowText` before the
//!    glyphs, `AboveText` after them, each image's pixels from `pixels`
//!    through a `TextureCache`.

use libghostty_vt::{
    Terminal,
    alloc::Bytes,
    kitty::graphics::{self, DecodePng, DecodedImage, Graphics, Layer, PlacementIterator},
    render::CellIteration,
    style::StyleColor,
};

use crate::{placeholders, term::PADDING};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The image store's limit. Without one, the terminal rejects every image.
const STORAGE_LIMIT: u64 = 64 * 1024 * 1024;

/// Install the PNG decoder Kitty images need. It's process-global, so it
/// goes before any terminal is created. If it can't be installed, the
/// terminal still works, without PNG images, so both front ends log it and
/// go on.
///
/// # Safety
///
/// No terminal may exist yet, and no other thread may call libghostty-vt
/// (its types are `!Send`, and tai's pty threads never touch them).
#[allow(unsafe_code)]
pub unsafe fn install_png_decoder() {
    // SAFETY: the caller promises no terminal exists and no other thread
    // calls libghostty-vt.
    if let Err(e) = unsafe { graphics::set_png_decoder(Some(Box::new(PngDecoder))) } {
        tracing::warn!("Kitty images: no PNG decoder ({e}); PNG images won't draw");
    }
}

/// PNG to RGBA8 with the `image` crate, the one version both binaries
/// build (the vendored macroquad uses it too; ADR 0014, step 2).
struct PngDecoder;

impl DecodePng for PngDecoder {
    fn decode_png<'alloc>(
        &mut self,
        alloc: &'alloc libghostty_vt::alloc::Allocator<'_>,
        data: &[u8],
    ) -> Option<DecodedImage<'alloc>> {
        let rgba = image::load_from_memory_with_format(data, image::ImageFormat::Png)
            .ok()?
            .to_rgba8();
        let (width, height) = rgba.dimensions();
        let pixels = rgba.into_raw();
        let mut data = Bytes::new_zeroed_with_alloc(alloc, pixels.len()).ok()?;
        data.copy_from_slice(&pixels);
        Some(DecodedImage {
            width,
            height,
            data,
        })
    }
}

/// Accept Kitty images on `terminal`: a storage limit, and images sent as
/// files, temporary files and shared memory besides inline. Temporary files
/// must be in the OS's temp folder, as Ghostty's app allows
/// (src/os/file.zig, tmpDir).
pub fn enable(terminal: &mut Terminal<'_, '_>) -> Result<()> {
    terminal
        .set_kitty_image_storage_limit(STORAGE_LIMIT)?
        .set_kitty_image_from_file_allowed(true)?
        .set_kitty_image_temp_file_dir(Some(&std::env::temp_dir()))?
        .set_kitty_image_from_shared_mem_allowed(true)?;
    Ok(())
}

/// The grid's cell size in logical units, and the scale to physical pixels
/// (the protocol's unit). The grid starts after `PADDING`.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub cell_width: f32,
    pub cell_height: f32,
    pub dpi_scale: f32,
}

/// Where an image draws among the cells, as its Kitty z layer says, in
/// drawing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Before the cell backgrounds.
    BelowBackground,
    /// Before the glyphs; Unicode placeholder images draw here too.
    BelowText,
    /// After the glyphs.
    AboveText,
}

/// Every image the frame draws, in order, each with its stage: the three
/// Kitty layers' placements and the slices that `runs` (Unicode
/// placeholder runs, each with its viewport row) show. Both front ends draw
/// from this one list, so the layers and their order can't differ.
pub fn frame_draws<'t>(
    terminal: &'t Terminal<'_, '_>,
    placement_it: &mut PlacementIterator<'_>,
    graphics: &Graphics<'t>,
    runs: &[(u16, placeholders::Placement)],
    geometry: Geometry,
) -> Result<Vec<(Stage, Draw)>> {
    let mut out = Vec::new();
    for (stage, layer) in [
        (Stage::BelowBackground, Layer::BELOW_BACKGROUND),
        (Stage::BelowText, Layer::BELOW_TEXT),
        (Stage::AboveText, Layer::ABOVE_TEXT),
    ] {
        let draws = placement_draws(terminal, placement_it, graphics, layer, geometry)?;
        out.extend(draws.into_iter().map(|d| (stage, d)));
        if stage == Stage::BelowText {
            let draws = placeholder_draws(placement_it, graphics, runs, geometry)?;
            out.extend(draws.into_iter().map(|d| (stage, d)));
        }
    }
    Ok(out)
}

/// A rectangle in logical units (`dest`) or image pixels (`source`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// One image, or part of one, to draw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Draw {
    pub image_id: u32,
    /// The part of the image, in its pixels.
    pub source: Rect,
    /// Where it goes, in logical units from the pane's top-left corner.
    pub dest: Rect,
}

/// An image's pixels, ready to upload: RGBA8, `width * height * 4` bytes.
pub struct Pixels<'t> {
    pub width: u32,
    pub height: u32,
    /// Changes whenever the image's data does, so a texture made from it
    /// can be kept until then.
    pub generation: u64,
    pub data: &'t [u8],
}

/// The pixels of image `id`, if it's RGBA (the PNG decoder makes every
/// image RGBA) and has all its data.
pub fn pixels<'t>(graphics: &Graphics<'t>, id: u32) -> Result<Option<Pixels<'t>>> {
    let Some(image) = graphics.image(id) else {
        return Ok(None);
    };
    if image.format()? != graphics::ImageFormat::Rgba {
        return Ok(None);
    }
    let (width, height) = (image.width()?, image.height()?);
    let Some(data) = image.data()? else {
        return Ok(None);
    };
    if data.len() < (width as usize) * (height as usize) * 4 {
        return Ok(None);
    }
    Ok(Some(Pixels {
        width,
        height,
        generation: image.generation()?,
        data,
    }))
}

/// Textures made from images, for either front end's texture type: one is
/// made when an image is first drawn, made again when its data changes
/// (its generation), and dropped after a frame that didn't draw it.
pub struct TextureCache<T> {
    /// By image id: the generation it was made from, the texture, and
    /// whether this frame drew it.
    entries: std::collections::HashMap<u32, (u64, T, bool)>,
}

impl<T> Default for TextureCache<T> {
    fn default() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
        }
    }
}

impl<T> TextureCache<T> {
    /// The texture for image `id` at `generation`, made with `make` when
    /// there's none or it's from an older generation. Also says whether it
    /// was just made, for a front end that uploads separately.
    pub fn texture(
        &mut self,
        id: u32,
        generation: u64,
        make: impl FnOnce() -> T,
    ) -> (&mut T, bool) {
        let fresh = !matches!(self.entries.get(&id), Some((g, _, _)) if *g == generation);
        if fresh {
            self.entries.insert(id, (generation, make(), true));
        }
        let entry = self.entries.get_mut(&id).expect("just made or there");
        entry.2 = true;
        (&mut entry.1, fresh)
    }

    // `get`, `has` and `generations` serve a front end that makes textures
    // apart from where it learns the frame's images: tai-gui's GPU side,
    // which can lack an image's pixels for a frame. tai makes and draws in
    // one place, so it doesn't call them.

    /// The texture for image `id`, if there is one.
    #[allow(dead_code)]
    pub fn get(&self, id: u32) -> Option<&T> {
        self.entries.get(&id).map(|(_, texture, _)| texture)
    }

    /// Whether there's a texture for image `id` at `generation`.
    #[allow(dead_code)]
    pub fn has(&self, id: u32, generation: u64) -> bool {
        matches!(self.entries.get(&id), Some((g, _, _)) if *g == generation)
    }

    /// The images there are textures for, with their generations.
    #[allow(dead_code)]
    pub fn generations(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        self.entries
            .iter()
            .map(|(id, (generation, _, _))| (*id, *generation))
    }

    /// After a frame: drop the textures it didn't draw, and return their
    /// image ids.
    pub fn end_frame(&mut self) -> Vec<u32> {
        let gone: Vec<u32> = self
            .entries
            .iter()
            .filter(|(_, (_, _, used))| !used)
            .map(|(id, _)| *id)
            .collect();
        for id in &gone {
            self.entries.remove(id);
        }
        for entry in self.entries.values_mut() {
            entry.2 = false;
        }
        gone
    }
}

/// One layer's visible placements, in drawing order. libghostty-vt's
/// iterator walks a hash map, so Ghostty's renderer sorts placements by z
/// and then image id, which puts the newer of two overlapping images on
/// top (`src/renderer/image.zig`, `sortUnstable` and `Id.zLessThan`,
/// a887df42). Virtual placements (Unicode placeholders) aren't here; they
/// come from `placeholder_draws`.
fn placement_draws<'t>(
    terminal: &'t Terminal<'_, '_>,
    placement_it: &mut PlacementIterator<'_>,
    graphics: &Graphics<'t>,
    layer: Layer,
    geometry: Geometry,
) -> Result<Vec<Draw>> {
    let mut draws = Vec::new();
    let mut placements = placement_it.update(graphics)?;
    placements.set_layer(layer)?;
    while let Some(placement) = placements.next() {
        let image_id = placement.image_id()?;
        let Some(image) = graphics.image(image_id) else {
            continue;
        };
        let info = placement.placement_render_info(&image, terminal)?;
        // Not in the viewport, or nothing to draw.
        if !info.viewport_visible
            || info.pixel_width == 0
            || info.pixel_height == 0
            || info.grid_cols == 0
            || info.grid_rows == 0
        {
            continue;
        }
        if pixels(graphics, image_id)?.is_none() {
            continue;
        }
        // The sub-cell offsets are in physical pixels; draws are logical.
        let x_offset = placement.x_offset()? as f32 / geometry.dpi_scale;
        let y_offset = placement.y_offset()? as f32 / geometry.dpi_scale;
        draws.push((
            placement.z()?,
            Draw {
                image_id,
                source: Rect {
                    x: info.source_x as f32,
                    y: info.source_y as f32,
                    w: info.source_width as f32,
                    h: info.source_height as f32,
                },
                dest: Rect {
                    x: PADDING + info.viewport_col as f32 * geometry.cell_width + x_offset,
                    y: PADDING + info.viewport_row as f32 * geometry.cell_height + y_offset,
                    w: info.grid_cols as f32 * geometry.cell_width,
                    h: info.grid_rows as f32 * geometry.cell_height,
                },
            },
        ));
    }
    draws.sort_by_key(|(z, d)| (*z, d.image_id));
    Ok(draws.into_iter().map(|(_, d)| d).collect())
}

/// The placeholder in a cell on a row that has Unicode placeholders, or
/// `None` when the cell is an ordinary one. A placeholder cell names its
/// image by its foreground colour and its placement by its underline
/// colour; its own character has no glyph.
pub fn placeholder_cell(
    cell: &CellIteration<'_, '_>,
) -> Result<Option<placeholders::PlaceholderCell>> {
    if cell.graphemes_len()? == 0 {
        return Ok(None);
    }
    let cps = cell.graphemes()?;
    if cps.first() != Some(&placeholders::PLACEHOLDER) {
        return Ok(None);
    }
    let style = cell.style()?;
    Ok(Some(placeholders::PlaceholderCell {
        diacritics: cps[1..].to_vec(),
        fg: id_color(style.fg_color),
        underline: id_color(style.underline_color),
    }))
}

fn id_color(c: StyleColor) -> placeholders::IdColor {
    match c {
        StyleColor::None => placeholders::IdColor::None,
        StyleColor::Palette(index) => placeholders::IdColor::Palette(index.0),
        StyleColor::Rgb(libghostty_vt::color::Rgb { r, g, b }) => {
            placeholders::IdColor::Rgb(r, g, b)
        }
    }
}

/// The image slices that Unicode placeholder runs show, each run given with
/// its viewport row. Each run's image and grid come from its virtual
/// placement; the slice and its position are worked out as in Ghostty
/// (`src/placeholders.rs`).
fn placeholder_draws(
    placement_it: &mut PlacementIterator<'_>,
    graphics: &Graphics<'_>,
    runs: &[(u16, placeholders::Placement)],
    geometry: Geometry,
) -> Result<Vec<Draw>> {
    if runs.is_empty() {
        return Ok(Vec::new());
    }
    // Virtual placements' grids (`c`, `r`): by image and placement id, and
    // the first one of each image for runs that name no placement.
    let mut by_id = std::collections::HashMap::new();
    let mut first = std::collections::HashMap::new();
    let mut placements = placement_it.update(graphics)?;
    placements.set_layer(Layer::all())?;
    while let Some(placement) = placements.next() {
        if !placement.is_virtual()? {
            continue;
        }
        let image_id = placement.image_id()?;
        let grid = (placement.columns()?, placement.rows()?);
        by_id.insert((image_id, placement.placement_id()?), grid);
        first.entry(image_id).or_insert(grid);
    }

    // The protocol's sizes are in physical pixels; draws are logical.
    let scale = geometry.dpi_scale;
    let cell_w = (geometry.cell_width * scale).round().max(1.0) as u32;
    let cell_h = (geometry.cell_height * scale).round().max(1.0) as u32;
    let mut draws = Vec::new();
    for (row, run) in runs {
        let grid = if run.placement_id > 0 {
            by_id.get(&(run.image_id, run.placement_id))
        } else {
            first.get(&run.image_id)
        };
        let (Some(&(columns, rows)), Some(image)) = (grid, pixels(graphics, run.image_id)?) else {
            continue;
        };
        let (img_w, img_h) = (image.width, image.height);
        let grid = placeholders::grid(columns, rows, img_w, img_h, cell_w, cell_h);
        let Some(slice) = placeholders::render(run, grid, img_w, img_h, cell_w, cell_h) else {
            continue;
        };
        draws.push(Draw {
            image_id: run.image_id,
            source: Rect {
                x: slice.source_x as f32,
                y: slice.source_y as f32,
                w: slice.source_width as f32,
                h: slice.source_height as f32,
            },
            dest: Rect {
                x: PADDING + f32::from(run.x) * geometry.cell_width + slice.offset_x as f32 / scale,
                y: PADDING + f32::from(*row) * geometry.cell_height + slice.offset_y as f32 / scale,
                w: slice.dest_width as f32 / scale,
                h: slice.dest_height as f32 / scale,
            },
        });
    }
    Ok(draws)
}

#[cfg(test)]
mod tests {
    use libghostty_vt::kitty::graphics::{Layer, PlacementIterator};

    use super::{Geometry, TextureCache, enable, placement_draws};

    #[test]
    fn a_texture_is_made_again_only_for_new_data_and_dropped_when_unused() {
        let mut cache = TextureCache::default();
        let mut made = 0;
        let make = |n: &mut i32| {
            *n += 1;
            *n
        };
        // First draw: made. Same generation: kept.
        assert_eq!(cache.texture(7, 1, || make(&mut made)), (&mut 1, true));
        assert_eq!(cache.texture(7, 1, || make(&mut made)), (&mut 1, false));
        assert!(cache.end_frame().is_empty());
        // New data: made again.
        assert_eq!(cache.texture(7, 2, || make(&mut made)), (&mut 2, true));
        assert!(cache.end_frame().is_empty());
        // What a front end that uploads apart asks.
        assert!(cache.has(7, 2) && !cache.has(7, 1) && !cache.has(8, 2));
        assert_eq!(cache.get(7), Some(&2));
        assert_eq!(cache.generations().collect::<Vec<_>>(), vec![(7, 2)]);
        // A frame that doesn't draw it drops it.
        assert_eq!(cache.end_frame(), vec![7]);
        assert_eq!(cache.get(7), None);
        assert_eq!(cache.texture(7, 2, || make(&mut made)), (&mut 3, true));
    }

    #[test]
    fn overlapping_images_draw_oldest_first() {
        let mut term = crate::term::setup::new_terminal(80, 24, (10, 20)).unwrap();
        enable(&mut term).unwrap();
        // Eight 1x1 RGBA images at the same cell, as a carousel would
        // draw them. With no i=, libghostty-vt numbers them in order.
        for _ in 0..8 {
            term.vt_write(b"\x1b_Ga=T,f=32,q=2,C=1,s=1,v=1;/wAA/w==\x1b\\");
        }
        let graphics = term.kitty_graphics().unwrap().unwrap();
        let mut it = PlacementIterator::new().unwrap();
        let geometry = Geometry {
            cell_width: 10.0,
            cell_height: 20.0,
            dpi_scale: 1.0,
        };
        let draws = placement_draws(&term, &mut it, &graphics, Layer::all(), geometry).unwrap();
        let ids: Vec<u32> = draws.iter().map(|d| d.image_id).collect();
        assert_eq!(ids.len(), 8);
        // The newest image draws last, so it ends up on top.
        assert!(ids.is_sorted(), "draw order {ids:?}");
        // At the cursor's cell, the first, one cell big, after the padding.
        assert_eq!((draws[0].dest.x, draws[0].dest.y), (6.0, 6.0));
        assert_eq!((draws[0].dest.w, draws[0].dest.h), (10.0, 20.0));
    }
}
