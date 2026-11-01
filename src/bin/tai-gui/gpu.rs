//! The terminal's cell pass on wgpu: tai's GLSL text pass (`src/cells.rs`,
//! ADR 0008) as WGSL, drawn in an egui paint callback (ADR 0011).
//!
//! One instanced draw per pane: backgrounds and the cursor as solid quads,
//! then the glyphs from the shared atlas. egui sets the render pass's
//! viewport to the callback's rect, so instances are in logical points from
//! the rect's top-left corner.

use std::sync::Arc;

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
"#;

/// The GPU objects, kept in egui-wgpu's callback resources.
pub struct CellPass {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    atlas: Option<(wgpu::Texture, (u32, u32))>,
    bind_group: Option<wgpu::BindGroup>,
    instances: wgpu::Buffer,
    /// Instances `instances` holds, and how many the last frame drew.
    capacity: usize,
    count: u32,
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
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tai atlas"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tai cells uniforms"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let capacity = 4096;
        let instances = Self::instance_buffer(device, capacity);
        render.renderer.write().callback_resources.insert(Self {
            pipeline,
            layout,
            sampler,
            uniforms,
            atlas: None,
            bind_group: None,
            instances,
            capacity,
            count: 0,
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

/// One frame of a terminal pane, handed to the paint callback.
pub struct CellFrame {
    pub view: egui::Vec2,
    pub instances: Arc<Vec<cells::Instance>>,
    pub atlas: Option<AtlasUpload>,
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
        let Some(pass) = resources.get_mut::<CellPass>() else {
            return Vec::new();
        };
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
                    layout: &pass.layout,
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
                            resource: wgpu::BindingResource::Sampler(&pass.sampler),
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
        let Some(pass) = resources.get::<CellPass>() else {
            return;
        };
        let Some(bind_group) = &pass.bind_group else {
            return;
        };
        if pass.count == 0 {
            return;
        }
        render_pass.set_pipeline(&pass.pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.set_vertex_buffer(0, pass.instances.slice(..));
        render_pass.draw(0..6, 0..pass.count);
    }
}
