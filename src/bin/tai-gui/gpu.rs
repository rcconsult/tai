//! The terminal's cell pass on wgpu: tai's GLSL text pass (`src/cells.rs`,
//! ADR 0008) as WGSL, drawn in an egui paint callback (ADR 0011).
//!
//! One instanced draw per pane: backgrounds and the cursor as solid quads,
//! then the glyphs from the pane's atlas. Each pane has its own atlas
//! texture, uniforms and instances (`PaneGpu`), since each keeps its own
//! glyph atlas, and egui prepares every callback before it paints any. egui sets the render pass's
//! viewport to the callback's rect, so instances are in logical points from
//! the rect's top-left corner.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use eframe::{
    egui,
    egui_wgpu::{self, CallbackResources, CallbackTrait, ScreenDescriptor},
    wgpu,
};

use crate::cells;

const SHADER: &str = r#"
struct Uniforms {
    // The pane's size in logical points, and the atlas's in pixels.
    view: vec2f,
    atlas: vec2f,
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct Out {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) color: vec4f,
    @location(2) solid: f32,
}

@vertex
fn vs(
    @builtin(vertex_index) vi: u32,
    @location(0) dest: vec4f,
    @location(1) src: vec4f,
    @location(2) color: vec4f,
) -> Out {
    var corners = array<vec2f, 6>(
        vec2f(0.0, 0.0), vec2f(1.0, 0.0), vec2f(1.0, 1.0),
        vec2f(0.0, 0.0), vec2f(1.0, 1.0), vec2f(0.0, 1.0),
    );
    let corner = corners[vi];
    let p = dest.xy + corner * dest.zw;
    var out: Out;
    out.pos = vec4f(p.x / u.view.x * 2.0 - 1.0, 1.0 - p.y / u.view.y * 2.0, 0.0, 1.0);
    out.uv = (src.xy + corner * src.zw) / u.atlas;
    out.color = color;
    out.solid = select(0.0, 1.0, src.z == 0.0);
    return out;
}

@fragment
fn fs(in: Out) -> @location(0) vec4f {
    let texel = textureSample(atlas, atlas_sampler, in.uv);
    let a = select(texel.a, 1.0, in.solid > 0.5);
    return vec4f(in.color.rgb, in.color.a * a);
}

// A Kitty image: its own colours. Its bind group's texture is the image,
// and `u.atlas` its size, so the same vertex stage maps its pixels.
@fragment
fn fs_image(in: Out) -> @location(0) vec4f {
    return textureSample(atlas, atlas_sampler, in.uv);
}
"#;

/// A terminal pane's key to its GPU objects. Dropping it frees them.
pub struct PaneId(pub u64);

static NEXT_PANE: AtomicU64 = AtomicU64::new(0);
/// Panes gone since the last prepare, whose GPU objects can go.
static RETIRED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

impl PaneId {
    pub fn new() -> Self {
        Self(NEXT_PANE.fetch_add(1, Ordering::Relaxed))
    }
}

impl Drop for PaneId {
    fn drop(&mut self) {
        if let Ok(mut retired) = RETIRED.lock() {
            retired.push(self.0);
        }
    }
}

/// The GPU objects, kept in egui-wgpu's callback resources: shared ones,
/// and each pane's.
pub struct CellPass {
    pipeline: wgpu::RenderPipeline,
    /// Kitty images: the cell pass's vertex stage with `fs_image`.
    image_pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    panes: HashMap<u64, PaneGpu>,
}

/// One pane's GPU objects.
struct PaneGpu {
    uniforms: wgpu::Buffer,
    atlas: Option<(wgpu::Texture, (u32, u32))>,
    bind_group: Option<wgpu::BindGroup>,
    instances: wgpu::Buffer,
    /// Instances `instances` holds, and how many the last frame drew.
    capacity: usize,
    count: u32,
    /// Where the frame's cell backgrounds and glyphs start in `instances`,
    /// so images can go between them (`ImageStage`).
    bg_start: u32,
    text_start: u32,
    /// Kitty images, each with its own texture and bind group, in the
    /// cache tai uses too (src/term/images.rs): made when an image's
    /// generation is new here, dropped after a frame that didn't draw it.
    images: crate::term::images::TextureCache<ImageGpu>,
    image_instances: wgpu::Buffer,
    image_capacity: usize,
    /// The frame's image draws: stage, image id, index in `image_instances`.
    image_draws: Vec<(ImageStage, u32, u32)>,
}

/// One Kitty image's GPU objects.
struct ImageGpu {
    _texture: wgpu::Texture,
    /// The pane's size and the image's, for the shared vertex stage.
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    size: (u32, u32),
}

impl ImageGpu {
    /// An image's texture, uniforms and bind group, from its pixels.
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        upload: &ImageUpload,
    ) -> Self {
        let (w, h) = upload.size;
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tai image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &upload.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            size,
        );
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tai image uniforms"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tai image"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        Self {
            _texture: texture,
            uniforms,
            bind_group,
            size: upload.size,
        }
    }
}

impl PaneGpu {
    fn new(device: &wgpu::Device) -> Self {
        let capacity = 4096;
        Self {
            uniforms: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tai cells uniforms"),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            atlas: None,
            bind_group: None,
            instances: CellPass::instance_buffer(device, capacity),
            capacity,
            count: 0,
            bg_start: 0,
            text_start: 0,
            images: Default::default(),
            image_instances: CellPass::instance_buffer(device, 16),
            image_capacity: 16,
            image_draws: Vec::new(),
        }
    }
}

impl CellPass {
    pub fn install(render: &egui_wgpu::RenderState) {
        let device = &render.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tai cells"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tai cells"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tai cells"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tai cells"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<cells::Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x4, 1 => Float32x4, 2 => Unorm8x4
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                // egui-wgpu draws in gamma space to a non-sRGB target, so
                // the cells' sRGB bytes go through as they are.
                targets: &[Some(wgpu::ColorTargetState {
                    format: render.target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tai images"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<cells::Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x4, 1 => Float32x4, 2 => Unorm8x4
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_image"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: render.target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tai atlas"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        render.renderer.write().callback_resources.insert(Self {
            pipeline,
            image_pipeline,
            layout,
            sampler,
            panes: HashMap::new(),
        });
    }

    fn instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tai cells instances"),
            size: (capacity * std::mem::size_of::<cells::Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }
}

/// Atlas pixels to upload this frame: rows `first..first + rows` of an
/// atlas `size` big. A changed size means a new texture, uploaded whole.
pub struct AtlasUpload {
    pub size: (u32, u32),
    pub first: u32,
    pub pixels: Vec<u8>,
}

/// Where an image draws among the cells (src/term/images.rs).
pub use crate::term::images::Stage as ImageStage;

/// A Kitty image's pixels at one generation, sent only when the GPU side
/// has no texture for that generation (`ImagesHeld`).
pub struct ImageUpload {
    pub id: u32,
    pub generation: u64,
    pub size: (u32, u32),
    pub rgba: Vec<u8>,
}

/// The images, by id, that a pane's GPU side holds textures for, and at
/// which generation. The GPU side writes it after each prepare; the pane
/// reads it to send pixels only for what's missing. An upload lost with a
/// frame whose callback egui skipped is sent again the next frame, since
/// the map still lacks it.
pub type ImagesHeld = Arc<Mutex<HashMap<u32, u64>>>;

/// One frame of a terminal pane, handed to the paint callback.
pub struct CellFrame {
    /// The pane, by its `PaneId`.
    pub pane: u64,
    pub view: egui::Vec2,
    pub instances: Arc<Vec<cells::Instance>>,
    pub atlas: Option<AtlasUpload>,
    /// Where cell backgrounds and glyphs start in `instances`.
    pub bg_start: u32,
    pub text_start: u32,
    /// The frame's images in drawing order: stage, image id, generation.
    pub images: Vec<(ImageStage, u32, u64, cells::Instance)>,
    /// Pixels of the images the GPU side lacks.
    pub image_uploads: Vec<ImageUpload>,
    /// Written back after prepare: the images the GPU side holds.
    pub images_held: ImagesHeld,
}

impl CallbackTrait for CellFrame {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(cells) = resources.get_mut::<CellPass>() else {
            return Vec::new();
        };
        if let Ok(mut retired) = RETIRED.lock() {
            for id in retired.drain(..) {
                cells.panes.remove(&id);
            }
        }
        let pass = cells
            .panes
            .entry(self.pane)
            .or_insert_with(|| PaneGpu::new(device));
        if let Some(upload) = &self.atlas {
            let (w, h) = upload.size;
            if pass
                .atlas
                .as_ref()
                .is_none_or(|(_, size)| *size != upload.size)
            {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("tai atlas"),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                pass.bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("tai cells"),
                    layout: &cells.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: pass.uniforms.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&cells.sampler),
                        },
                    ],
                }));
                pass.atlas = Some((texture, upload.size));
            }
            let rows = upload.pixels.len() as u32 / (w * 4);
            if let Some((texture, _)) = &pass.atlas
                && rows > 0
            {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: upload.first,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &upload.pixels,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(w * 4),
                        rows_per_image: Some(rows),
                    },
                    wgpu::Extent3d {
                        width: w,
                        height: rows,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
        // Kitty images, through the cache tai uses too: a texture is made
        // when the frame's generation of an image is new here and its
        // pixels came with the frame. Without them, it waits a frame.
        let mut image_instances = Vec::with_capacity(self.images.len());
        pass.image_draws.clear();
        for (stage, id, generation, instance) in &self.images {
            let upload = self
                .image_uploads
                .iter()
                .find(|u| u.id == *id && u.generation == *generation);
            if !pass.images.has(*id, *generation) && upload.is_none() {
                continue;
            }
            let (image, _) = pass.images.texture(*id, *generation, || {
                let upload = upload.expect("checked above: the pixels came with the frame");
                ImageGpu::new(device, queue, &cells.layout, &cells.sampler, upload)
            });
            let (w, h) = image.size;
            let uniforms = [self.view.x, self.view.y, w as f32, h as f32];
            let bytes: Vec<u8> = uniforms.iter().flat_map(|f| f.to_ne_bytes()).collect();
            queue.write_buffer(&image.uniforms, 0, &bytes);
            pass.image_draws
                .push((*stage, *id, image_instances.len() as u32));
            image_instances.push(*instance);
        }
        pass.images.end_frame();
        if let Ok(mut held) = self.images_held.lock() {
            held.clear();
            held.extend(pass.images.generations());
        }
        if image_instances.len() > pass.image_capacity {
            pass.image_capacity = image_instances.len().next_power_of_two();
            pass.image_instances = CellPass::instance_buffer(device, pass.image_capacity);
        }
        if !image_instances.is_empty() {
            queue.write_buffer(
                &pass.image_instances,
                0,
                cells::Instance::as_bytes(&image_instances),
            );
        }
        pass.bg_start = self.bg_start;
        pass.text_start = self.text_start;

        let Some((_, (aw, ah))) = pass.atlas else {
            return Vec::new();
        };
        let uniforms = [self.view.x, self.view.y, aw as f32, ah as f32];
        let bytes: Vec<u8> = uniforms.iter().flat_map(|f| f.to_ne_bytes()).collect();
        queue.write_buffer(&pass.uniforms, 0, &bytes);
        if self.instances.len() > pass.capacity {
            pass.capacity = self.instances.len().next_power_of_two();
            pass.instances = CellPass::instance_buffer(device, pass.capacity);
        }
        queue.write_buffer(
            &pass.instances,
            0,
            cells::Instance::as_bytes(&self.instances),
        );
        pass.count = self.instances.len() as u32;
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &CallbackResources,
    ) {
        let Some(cells) = resources.get::<CellPass>() else {
            return;
        };
        let Some(pass) = cells.panes.get(&self.pane) else {
            return;
        };
        let Some(bind_group) = &pass.bind_group else {
            return;
        };
        if pass.count == 0 {
            return;
        }
        // The window background and padding, images below the background,
        // the cell backgrounds, images below the text, the glyphs, then
        // images above the text, as tai draws them (src/main.rs).
        let bg_start = pass.bg_start.min(pass.count);
        let text_start = pass.text_start.clamp(bg_start, pass.count);
        let cells_range = |render_pass: &mut wgpu::RenderPass<'static>,
                           range: std::ops::Range<u32>| {
            if range.is_empty() {
                return;
            }
            render_pass.set_pipeline(&cells.pipeline);
            render_pass.set_bind_group(0, bind_group, &[]);
            render_pass.set_vertex_buffer(0, pass.instances.slice(..));
            render_pass.draw(0..6, range);
        };
        let images = |render_pass: &mut wgpu::RenderPass<'static>, stage: ImageStage| {
            for &(draw_stage, id, index) in &pass.image_draws {
                let Some(image) = pass.images.get(id).filter(|_| draw_stage == stage) else {
                    continue;
                };
                render_pass.set_pipeline(&cells.image_pipeline);
                render_pass.set_bind_group(0, &image.bind_group, &[]);
                render_pass.set_vertex_buffer(0, pass.image_instances.slice(..));
                render_pass.draw(0..6, index..index + 1);
            }
        };
        cells_range(render_pass, 0..bg_start);
        images(render_pass, ImageStage::BelowBackground);
        cells_range(render_pass, bg_start..text_start);
        images(render_pass, ImageStage::BelowText);
        cells_range(render_pass, text_start..pass.count);
        images(render_pass, ImageStage::AboveText);
    }
}
