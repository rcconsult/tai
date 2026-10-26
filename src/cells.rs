//! Drawing the grid's text from cell buffers in one instanced draw, as
//! Ghostty's renderer does (ADR 0008).
//!
//! Ported in spirit from Ghostty at a887df42 ("terminal: bound page map
//! probe lengths without tombstones (#13294)"): the glyph atlas from
//! `src/font/Atlas.zig` and the text pass from
//! `src/renderer/shaders/glsl/cell_text.v.glsl` / `cell_text.f.glsl`.
//! Ghostty's GLSL needs OpenGL 4.3 (SSBOs); these shaders use per-instance
//! vertex attributes instead, so they run on macOS's OpenGL 4.1 and on a
//! 3.2 core context.

use std::collections::HashMap;

use macroquad::{
    color::Color,
    math::Mat4,
    miniquad::{
        BlendFactor, BlendState, BlendValue, BufferId, BufferLayout, BufferSource, BufferType,
        BufferUsage, Equation, PassAction, Pipeline, PipelineParams, RenderPass, ShaderMeta,
        ShaderSource, UniformBlockLayout, UniformDesc, UniformType, UniformsSource,
        VertexAttribute, VertexFormat, VertexStep, gl,
    },
    texture::{FilterMode, Texture2D},
    window::get_internal_gl,
};

use crate::fonts::Face;

/// One glyph quad: where it's drawn, in logical pixels (x, y, w, h), where
/// it is in the atlas, in atlas pixels, and its colour.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Instance {
    dest: [f32; 4],
    src: [f32; 4],
    color: [u8; 4],
}

// The text pass's vertex layout (dest, src: Float4; color0: Byte4) reads
// instances at this stride.
const _: () = assert!(std::mem::size_of::<Instance>() == 36);

/// A rasterized glyph: its place in the atlas and fontdue's metrics, in
/// physical pixels.
#[derive(Clone, Copy, Debug)]
struct Glyph {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    xmin: f32,
    ymin: f32,
    advance: f32,
}

/// Transparent pixels kept around every glyph, so linear filtering at a
/// glyph's edge reads transparency, not a neighbour or the texture's
/// clamped edge. macroquad's atlas keeps the same gap but starts its first
/// row at the top edge.
const GAP: u32 = 2;
const ATLAS_WIDTH: u32 = 1024;
/// The tallest the atlas grows, unless the driver's texture limit is lower
/// (`TextPass::max_texture_size`).
pub const ATLAS_MAX_HEIGHT: u32 = 8192;

/// All faces' glyphs in one RGBA texture, packed in shelves.
///
/// Unlike macroquad's atlas, which repacks every glyph when it grows, this
/// one only adds rows at the bottom, as Ghostty's `Atlas.grow` keeps the old
/// contents where they were. A glyph's position never changes once it's
/// placed, so cell buffers built in earlier frames stay valid.
pub struct GlyphAtlas {
    height: u32,
    /// The tallest it may grow: `ATLAS_MAX_HEIGHT` or the driver's limit.
    max_height: u32,
    pixels: Vec<u8>,
    glyphs: HashMap<(Face, char, u16), Option<Glyph>>,
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    /// The rows (start, end) holding glyphs added since the last upload.
    dirty_rows: Option<(u32, u32)>,
    texture: Option<Texture2D>,
}

impl GlyphAtlas {
    pub fn new(max_height: u32) -> Self {
        let height = 256;
        Self {
            height,
            max_height,
            pixels: vec![0; (ATLAS_WIDTH * height * 4) as usize],
            glyphs: HashMap::new(),
            shelf_x: 0,
            shelf_y: GAP,
            shelf_h: 0,
            dirty_rows: Some((0, height)),
            texture: None,
        }
    }

    /// Drop every glyph, for a new font size or DPI scale.
    pub fn clear(&mut self) {
        *self = Self {
            texture: self.texture.take(),
            ..Self::new(self.max_height)
        };
    }

    /// The glyph for `ch` at `px` physical pixels, rasterized and placed on
    /// first use. `None` for a glyph with no pixels (a space) or when the
    /// atlas is full.
    fn glyph(&mut self, face: Face, ch: char, px: u16, font: &fontdue::Font) -> Option<Glyph> {
        if let Some(glyph) = self.glyphs.get(&(face, ch, px)) {
            return *glyph;
        }
        let glyph = self.place(ch, px, font);
        self.glyphs.insert((face, ch, px), glyph);
        glyph
    }

    fn place(&mut self, ch: char, px: u16, font: &fontdue::Font) -> Option<Glyph> {
        let (metrics, bitmap) = font.rasterize(ch, f32::from(px));
        let (w, h) = (metrics.width as u32, metrics.height as u32);
        if w == 0 || h == 0 || w + 2 * GAP > ATLAS_WIDTH {
            return None;
        }
        if self.shelf_x + w + 2 * GAP > ATLAS_WIDTH {
            self.shelf_y += self.shelf_h + 2 * GAP;
            self.shelf_x = 0;
            self.shelf_h = 0;
        }
        while self.shelf_y + h + GAP > self.height {
            if self.height * 2 > self.max_height {
                tracing::warn!("glyph atlas is full; {ch:?} at {px}px isn't drawn");
                return None;
            }
            self.height *= 2;
            self.pixels
                .resize((ATLAS_WIDTH * self.height * 4) as usize, 0);
        }
        let (x, y) = (self.shelf_x + GAP, self.shelf_y);
        for row in 0..h {
            let start = (((y + row) * ATLAS_WIDTH + x) * 4) as usize;
            let line = &bitmap[(row * w) as usize..((row + 1) * w) as usize];
            let (pixels, _) = self.pixels[start..start + (w * 4) as usize].as_chunks_mut::<4>();
            for (pixel, coverage) in pixels.iter_mut().zip(line) {
                *pixel = [255, 255, 255, *coverage];
            }
        }
        self.shelf_x += w + 2 * GAP;
        self.shelf_h = self.shelf_h.max(h);
        self.dirty_rows = Some(match self.dirty_rows {
            Some((start, end)) => (start.min(y), end.max(y + h)),
            None => (y, y + h),
        });
        Some(Glyph {
            x,
            y,
            w,
            h,
            xmin: metrics.xmin as f32,
            ymin: metrics.ymin as f32,
            advance: metrics.advance_width,
        })
    }

    /// Queue `text` in `face` with its baseline at (`x`, `baseline`), laid
    /// out as macroquad's `draw_text_ex` lays it out (no rotation, scale 1),
    /// so both renderers put glyphs on the same pixels. `bold` draws it a
    /// second time 1 logical pixel to the right, as tai's fake bold does.
    #[allow(clippy::too_many_arguments)]
    pub fn push_text(
        &mut self,
        out: &mut Vec<Instance>,
        font: &fontdue::Font,
        face: Face,
        text: &str,
        x: f32,
        baseline: f32,
        font_size: u16,
        dpi_scale: f32,
        fg: Color,
        bold: bool,
    ) {
        let px = (f32::from(font_size) * dpi_scale).ceil() as u16;
        let color = [fg.r, fg.g, fg.b, fg.a].map(|c| (c * 255.0).round() as u8);
        let mut advance = 0.0;
        for ch in text.chars() {
            let Some(glyph) = self.glyph(face, ch, px, font) else {
                advance += font.metrics(ch, f32::from(px)).advance_width;
                continue;
            };
            let (w, h) = (glyph.w as f32, glyph.h as f32);
            let dest = [
                x + (glyph.xmin + advance) / dpi_scale,
                baseline + (-h - glyph.ymin) / dpi_scale,
                w / dpi_scale,
                h / dpi_scale,
            ];
            let src = [glyph.x as f32, glyph.y as f32, w, h];
            out.push(Instance { dest, src, color });
            if bold {
                let mut dest = dest;
                dest[0] += 1.0;
                out.push(Instance { dest, src, color });
            }
            advance += glyph.advance;
        }
    }

    /// The atlas texture with every glyph placed so far. Uploads only the
    /// rows that changed since the last call, unless the atlas grew: a
    /// first screen of CJK or a font-size step adds glyphs for several
    /// frames, and the whole atlas can be 32 MB.
    fn texture(&mut self) -> &Texture2D {
        let size_changed = self
            .texture
            .as_ref()
            .is_none_or(|t| t.height() as u32 != self.height);
        if size_changed {
            let texture =
                Texture2D::from_rgba8(ATLAS_WIDTH as u16, self.height as u16, &self.pixels);
            texture.set_filter(FilterMode::Linear);
            self.texture = Some(texture);
        } else if let Some((start, end)) = self.dirty_rows
            && let Some(texture) = &self.texture
        {
            let row_bytes = (ATLAS_WIDTH * 4) as usize;
            // SAFETY: as in `TextPass::new`.
            #[allow(unsafe_code)]
            let gl = unsafe { get_internal_gl() };
            gl.quad_context.texture_update_part(
                texture.raw_miniquad_id(),
                0,
                start as i32,
                ATLAS_WIDTH as i32,
                (end - start) as i32,
                &self.pixels[start as usize * row_bytes..end as usize * row_bytes],
            );
        }
        self.dirty_rows = None;
        self.texture.as_ref().expect("texture was just created")
    }
}

const VERTEX: &str = r#"
in vec2 corner;
in vec4 dest;
in vec4 src;
in vec4 color0;

uniform mat4 Projection;
uniform vec2 AtlasSize;

out vec2 uv;
out vec4 color;

void main() {
    gl_Position = Projection * vec4(dest.xy + corner * dest.zw, 0.0, 1.0);
    uv = (src.xy + corner * src.zw) / AtlasSize;
    color = color0 / 255.0;
}
"#;

const FRAGMENT: &str = r#"
in vec2 uv;
in vec4 color;

uniform sampler2D Atlas;

out vec4 frag;

void main() {
    frag = color * texture(Atlas, uv);
}
"#;

#[repr(C)]
struct Uniforms {
    projection: [f32; 16],
    atlas_size: [f32; 2],
}

/// The GPU side of the text pass: one quad, drawn once per glyph instance.
pub struct TextPass {
    pipeline: Pipeline,
    corners: BufferId,
    indices: BufferId,
    instances: BufferId,
    /// How many instances `instances` holds.
    capacity: usize,
    /// The driver's GL_VERSION string.
    pub gl_version: String,
    /// The GLSL version the shaders were built for, as "3.30" or "1.50".
    pub glsl: &'static str,
    /// The driver's GL_MAX_TEXTURE_SIZE.
    pub max_texture_size: u32,
}

impl TextPass {
    /// Why not, when the context can't run it: an OpenGL without
    /// instancing or GLSL 1.50, or a shader that didn't compile. (Metal
    /// never gets here; there's no MSL port yet.)
    pub fn new() -> Result<Self, String> {
        // SAFETY: macroquad's context exists for the program's lifetime once
        // macroquad::main runs, and tai uses it from the main thread only.
        #[allow(unsafe_code)]
        let gl = unsafe { get_internal_gl() };
        let ctx = gl.quad_context;
        let info = ctx.info();
        let (version, glsl) = if info.glsl_support.v330 {
            ("#version 330\n", "3.30")
        } else if info.glsl_support.v150 {
            ("#version 150\n", "1.50")
        } else {
            return Err(format!(
                "no GLSL 3.30 or 1.50 in OpenGL {}",
                info.gl_version_string
            ));
        };
        if !info.features.instancing {
            return Err("no instancing".into());
        }
        let mut max_texture_size = 0;
        // SAFETY: the context is current on this thread (see above), and
        // GL_MAX_TEXTURE_SIZE writes one integer.
        #[allow(unsafe_code)]
        unsafe {
            gl::glGetIntegerv(gl::GL_MAX_TEXTURE_SIZE, &mut max_texture_size)
        };
        let max_texture_size = u32::try_from(max_texture_size).unwrap_or(0);
        if max_texture_size < ATLAS_WIDTH {
            return Err(format!("GL_MAX_TEXTURE_SIZE is {max_texture_size}"));
        }
        let vertex = format!("{version}{VERTEX}");
        let fragment = format!("{version}{FRAGMENT}");
        let shader = ctx
            .new_shader(
                ShaderSource::Glsl {
                    vertex: &vertex,
                    fragment: &fragment,
                },
                ShaderMeta {
                    uniforms: UniformBlockLayout {
                        uniforms: vec![
                            UniformDesc::new("Projection", UniformType::Mat4),
                            UniformDesc::new("AtlasSize", UniformType::Float2),
                        ],
                    },
                    images: vec!["Atlas".to_string()],
                },
            )
            .map_err(|e| format!("the text shader didn't compile: {e:?}"))?;
        let pipeline = ctx.new_pipeline(
            &[
                BufferLayout::default(),
                BufferLayout {
                    step_func: VertexStep::PerInstance,
                    ..Default::default()
                },
            ],
            &[
                VertexAttribute::with_buffer("corner", VertexFormat::Float2, 0),
                VertexAttribute::with_buffer("dest", VertexFormat::Float4, 1),
                VertexAttribute::with_buffer("src", VertexFormat::Float4, 1),
                VertexAttribute::with_buffer("color0", VertexFormat::Byte4, 1),
            ],
            shader,
            // macroquad's default blending (vendor/macroquad/src/quad_gl.rs).
            PipelineParams {
                color_blend: Some(BlendState::new(
                    Equation::Add,
                    BlendFactor::Value(BlendValue::SourceAlpha),
                    BlendFactor::OneMinusValue(BlendValue::SourceAlpha),
                )),
                ..Default::default()
            },
        );
        let corners: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let corners = ctx.new_buffer(
            BufferType::VertexBuffer,
            BufferUsage::Immutable,
            BufferSource::slice(&corners),
        );
        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let indices = ctx.new_buffer(
            BufferType::IndexBuffer,
            BufferUsage::Immutable,
            BufferSource::slice(&indices),
        );
        let capacity = 4096;
        let instances = ctx.new_buffer(
            BufferType::VertexBuffer,
            BufferUsage::Stream,
            BufferSource::empty::<Instance>(capacity),
        );
        Ok(Self {
            pipeline,
            corners,
            indices,
            instances,
            capacity,
            gl_version: info.gl_version_string.clone(),
            glsl,
            max_texture_size,
        })
    }

    /// Draw `instances` into `pass` with `projection`, after everything
    /// macroquad has queued so far.
    pub fn draw(
        &mut self,
        instances: &[Instance],
        atlas: &mut GlyphAtlas,
        projection: Mat4,
        pass: RenderPass,
    ) {
        if instances.is_empty() {
            return;
        }
        let atlas_size = [ATLAS_WIDTH as f32, atlas.height as f32];
        let atlas_texture = atlas.texture().raw_miniquad_id();
        // SAFETY: as in `new`.
        #[allow(unsafe_code)]
        let mut gl = unsafe { get_internal_gl() };
        // macroquad batches its draws; send them first so the text lands on
        // top of the backgrounds queued before it.
        gl.flush();
        let ctx = gl.quad_context;
        if instances.len() > self.capacity {
            ctx.delete_buffer(self.instances);
            self.capacity = instances.len().next_power_of_two();
            self.instances = ctx.new_buffer(
                BufferType::VertexBuffer,
                BufferUsage::Stream,
                BufferSource::empty::<Instance>(self.capacity),
            );
        }
        ctx.buffer_update(self.instances, BufferSource::slice(instances));
        ctx.begin_pass(Some(pass), PassAction::Nothing);
        ctx.apply_pipeline(&self.pipeline);
        ctx.apply_bindings(&macroquad::miniquad::Bindings {
            vertex_buffers: vec![self.corners, self.instances],
            index_buffer: self.indices,
            images: vec![atlas_texture],
        });
        ctx.apply_uniforms(UniformsSource::table(&Uniforms {
            projection: projection.to_cols_array(),
            atlas_size,
        }));
        ctx.draw(0, 6, instances.len() as i32);
        ctx.end_render_pass();
    }
}
