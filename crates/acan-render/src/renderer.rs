//! The wgpu renderer. A frame has three passes, 4x MSAA where the device allows it. The scene
//! pass draws the ground, belts, contact shadows, machines and dots into colour and depth, and
//! resolves the colour into the surface. The line pass strokes every mesh edge that draws and
//! every painted line into a one-channel ink target, tested against the scene's depth, and
//! blends them by max so lines join without dark spots. The ink pass multiplies the ink onto the
//! surface once.
//! Model buffers are uploaded once per model set and per scene, never per frame.

use std::borrow::Cow;
use std::mem::size_of;
use std::ops::Range;

use bytemuck::{Pod, Zeroable};

use crate::camera::{Camera, NEAR_FRACTION, ORTHO_FAR, Projection};
use crate::geometry::{
    self, DotVertex, EdgeVertex, HaloVertex, HueVertex, InstanceRaw, LineVertex, MeshVertex,
    PadVertex, RibbonVertex, Strokes,
};
use crate::model::{Mesh, Model};
use crate::scene::{Lod, Scene};
use crate::theme::{self, Look, PALETTE_LEN, TINT_LEN};

pub type TintTable = [[f32; 4]; TINT_LEN];

const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// The per-instance attributes of an `InstanceRaw` are the offset and tint, the screen-size floor
/// at byte 16, then the turn at byte 24. `vertex_attr_array!` packs attributes in order, so these
/// name their offsets. Each pipeline reads the fields its shader names, at the locations below.
const MESH_INSTANCE: [wgpu::VertexAttribute; 4] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 0,
        shader_location: 3,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint32,
        offset: 12,
        shader_location: 4,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 16,
        shader_location: 5,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32,
        offset: 24,
        shader_location: 6,
    },
];
const EDGE_INSTANCE: [wgpu::VertexAttribute; 3] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 0,
        shader_location: 6,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 16,
        shader_location: 7,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32,
        offset: 24,
        shader_location: 8,
    },
];
/// The dot and line pipelines read the offset at location 5, the screen-size floor at 6, and the
/// turn at 7.
const STROKE_INSTANCE: [wgpu::VertexAttribute; 3] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 0,
        shader_location: 5,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 16,
        shader_location: 6,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32,
        offset: 24,
        shader_location: 7,
    },
];
/// Line ink, as how much it darkens the frame. One channel, so lines are grey: under MSAA the
/// line pass is bound by its target's bandwidth, and RGBA ink was measurably slower on a large
/// plant zoomed out.
const INK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;
const MSAA: u32 = 4;
/// Thinnest stroke on screen, in device pixels. Thinner strokes fade out instead.
const MIN_STROKE_PX: f32 = 0.75;
/// The depth cleared before a frame. The depth is reverse, so this is the farthest value.
const FAR_DEPTH: f32 = 0.0;
/// Machine faces are pushed back by this many device pixels' worth of their own depth slope.
/// A line that lies on a face steep to the view, such as the facet next to a cylinder's
/// contour, then wins the depth test across its whole width.
const MESH_SLOPE_BIAS: f32 = 2.0;

/// Why a renderer could not be created or a frame could not be drawn.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("no compatible GPU adapter: {0}")]
    Adapter(#[from] wgpu::RequestAdapterError),
    #[error("device request failed: {0}")]
    Device(#[from] wgpu::RequestDeviceError),
    #[error("the adapter cannot present to this surface")]
    UnsupportedSurface,
    #[error("surface lost; recreate the renderer")]
    SurfaceLost,
    #[error("validation error while acquiring the surface texture")]
    SurfaceValidation,
}

/// The per-frame camera, as the shader reads it. Every field is a `vec4` so the layout needs no
/// padding. See `plant.wgsl` for what each lane means.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
pub struct CameraUniform {
    pub right: [f32; 4],
    pub up: [f32; 4],
    pub toward: [f32; 4],
    pub eye: [f32; 4],
    pub pivot: [f32; 4],
    pub view: [f32; 4],
    pub depth: [f32; 4],
    pub fog: [f32; 4],
}

/// The shader source, with the theme's constants written in.
pub fn plant_source() -> String {
    include_str!("plant.wgsl")
        .replace("@@PALETTE_LEN@@", &PALETTE_LEN.to_string())
        .replace("@@TINT_LEN@@", &TINT_LEN.to_string())
        .replace("@@THEME@@", &theme::wgsl_consts())
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ThemeUniform {
    palette: [[f32; 4]; PALETTE_LEN],
    tints: [[f32; 4]; TINT_LEN],
}

struct MeshBuffers {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

struct StrokeBuffers {
    buffer: wgpu::Buffer,
    /// Cumulative vertex counts by level of detail, four to a quad.
    counts: [u32; 3],
}

struct ModelBuffers {
    levels: [MeshBuffers; 3],
    edges: StrokeBuffers,
    lines: StrokeBuffers,
    dots: StrokeBuffers,
}

/// The belt hue lines of the scene. Each piece takes four vertices and six indices.
struct HueBuffers {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

struct Pipelines {
    grid: wgpu::RenderPipeline,
    pad: wgpu::RenderPipeline,
    ribbon: wgpu::RenderPipeline,
    mesh: wgpu::RenderPipeline,
    silhouette: wgpu::RenderPipeline,
    halo: wgpu::RenderPipeline,
    dot: wgpu::RenderPipeline,
    hue: wgpu::RenderPipeline,
    edge: wgpu::RenderPipeline,
    line: wgpu::RenderPipeline,
    ink: wgpu::RenderPipeline,
}

/// The multisampled targets that resolve into the surface and the ink.
struct Multisampled {
    colour: wgpu::TextureView,
    ink: wgpu::TextureView,
}

struct Targets {
    /// `None` when the scene is drawn at one sample, straight into the surface and the ink.
    msaa: Option<Multisampled>,
    depth: wgpu::TextureView,
    /// The line ink at one sample per pixel, which the ink pass reads.
    ink: wgpu::TextureView,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    adapter_info: wgpu::AdapterInfo,
    samples: u32,
    camera_buf: wgpu::Buffer,
    theme_buf: wgpu::Buffer,
    srgb: bool,
    look: Look,
    uniform_bind: wgpu::BindGroup,
    ink_layout: wgpu::BindGroupLayout,
    ink_bind: wgpu::BindGroup,
    targets: Targets,
    pipelines: Pipelines,
    clear: wgpu::Color,
    models: Vec<ModelBuffers>,
    /// The indices of as many quads as the largest stroke buffer holds, shared by every stroke
    /// and dot draw.
    quads: Option<wgpu::Buffer>,
    /// Six halo vertices per kind, kind by kind.
    halos: Option<wgpu::Buffer>,
    pads: Option<(wgpu::Buffer, u32)>,
    ribbons: Option<(wgpu::Buffer, u32)>,
    hue: Option<HueBuffers>,
    /// The kinds whose hidden instances draw as silhouettes, as the scene named them.
    silhouettes: Vec<u16>,
    instances: Option<wgpu::Buffer>,
    /// Per kind: its range inside `instances`.
    ranges: Vec<Range<u32>>,
}

impl Renderer {
    /// `size` is the drawable size in physical pixels.
    pub async fn new(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        size: [u32; 2],
    ) -> Result<Self, RenderError> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("acan-render"),
                required_limits: wgpu::Limits::downlevel_webgl2_defaults()
                    .using_resolution(adapter.limits()),
                ..Default::default()
            })
            .await?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or(RenderError::UnsupportedSurface)?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size[0].max(1),
            height: size[1].max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .ok_or(RenderError::UnsupportedSurface)?,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let samples = sample_count(&adapter, format);
        let srgb = format.is_srgb();
        let page = theme::page(srgb);
        let theme_uniform = ThemeUniform {
            palette: theme::palette(srgb),
            tints: theme::tints().map(|c| theme::surface_colour(c, srgb)),
        };
        let camera_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("camera"),
            size: size_of::<CameraUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let theme_buf = upload(
            &device,
            &queue,
            "theme",
            wgpu::BufferUsages::UNIFORM,
            std::slice::from_ref(&theme_uniform),
        );
        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uniforms"),
            entries: &[uniform_entry(0), uniform_entry(1)],
        });
        let uniform_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("uniforms"),
            layout: &uniform_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: camera_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: theme_buf.as_entire_binding(),
                },
            ],
        });
        let ink_layout = ink_layout(&device);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("plant"),
            source: wgpu::ShaderSource::Wgsl(plant_source().into()),
        });
        let pipelines = Pipelines::new(
            &device,
            &shader,
            &uniform_layout,
            &ink_layout,
            format,
            samples,
        );

        let targets = targets(&device, format, samples, config.width, config.height);
        let ink_bind = ink_bind(&device, &ink_layout, &targets);
        Ok(Self {
            adapter_info: adapter.get_info(),
            device,
            queue,
            surface,
            config,
            samples,
            camera_buf,
            theme_buf,
            srgb,
            look: Look::Paper,
            uniform_bind,
            ink_layout,
            ink_bind,
            targets,
            pipelines,
            clear: wgpu::Color {
                r: page[0] as f64,
                g: page[1] as f64,
                b: page[2] as f64,
                a: page[3] as f64,
            },
            models: Vec::new(),
            quads: None,
            halos: None,
            pads: None,
            ribbons: None,
            hue: None,
            silhouettes: Vec::new(),
            instances: None,
            ranges: Vec::new(),
        })
    }

    /// The adapter in use, for reporting which backend drew the frames.
    pub fn adapter_info(&self) -> &wgpu::AdapterInfo {
        &self.adapter_info
    }

    /// The format of the surface the frames are presented to.
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// The MSAA sample count of the scene pass: 4, or 1 where the device lacks it.
    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// Resizes the surface and its targets. `size` is the drawable size in physical pixels.
    pub fn resize(&mut self, size: [u32; 2]) {
        self.config.width = size[0].max(1);
        self.config.height = size[1].max(1);
        self.surface.configure(&self.device, &self.config);
        self.targets = targets(
            &self.device,
            self.config.format,
            self.samples,
            self.config.width,
            self.config.height,
        );
        self.ink_bind = ink_bind(&self.device, &self.ink_layout, &self.targets);
    }

    /// Uploads every kind's three meshes, edges, lines, dots and halo once. Instances refer to
    /// kinds by index into this slice. Call `set_scene` afterwards, because the instance ranges
    /// depend on the kinds. Each level's edges are found here, from its mesh, and drawn at that
    /// level's detail.
    pub fn set_models(&mut self, models: &[Model]) {
        self.models = models
            .iter()
            .map(|model| ModelBuffers {
                levels: model.meshes.each_ref().map(|mesh| self.mesh_buffers(mesh)),
                edges: self.strokes("edges", geometry::edge_vertices(&model.meshes)),
                lines: self.strokes("lines", geometry::line_vertices(&model.lines)),
                dots: self.strokes("dots", geometry::dot_vertices(&model.dots)),
            })
            .collect();
        let most = self
            .models
            .iter()
            .flat_map(|m| [&m.edges, &m.lines, &m.dots])
            .map(|s| s.counts[2])
            .max()
            .unwrap_or(0);
        self.quads = Some(upload(
            &self.device,
            &self.queue,
            "stroke quads",
            wgpu::BufferUsages::INDEX,
            &geometry::quad_indices(most / geometry::QUAD_CORNERS),
        ));
        let halos: Vec<HaloVertex> = models
            .iter()
            .flat_map(|model| geometry::halo_vertices(&model.bounds))
            .collect();
        self.halos = Some(upload(
            &self.device,
            &self.queue,
            "halos",
            wgpu::BufferUsages::VERTEX,
            &halos,
        ));
        self.ranges.clear();
        self.instances = None;
    }

    /// Replaces the tint table. Each entry is sRGB, converted here to the surface's encoding. An
    /// instance's tint slot multiplies the colour of its mesh faces.
    pub fn set_tints(&mut self, tints: &TintTable) {
        let uniform = ThemeUniform {
            palette: theme::palette_of(self.look, self.srgb),
            tints: tints.map(|c| theme::surface_colour(c, self.srgb)),
        };
        self.queue
            .write_buffer(&self.theme_buf, 0, bytemuck::bytes_of(&uniform));
    }

    /// The look the page shows.
    pub fn look(&self) -> Look {
        self.look
    }

    /// Shows a look: its palette, its tints and its page colour. The next frame draws it.
    pub fn set_look(&mut self, look: Look) {
        self.look = look;
        let page = theme::page_of(look, self.srgb);
        self.clear = wgpu::Color {
            r: page[0] as f64,
            g: page[1] as f64,
            b: page[2] as f64,
            a: page[3] as f64,
        };
        let uniform = ThemeUniform {
            palette: theme::palette_of(look, self.srgb),
            tints: theme::tints_of(look).map(|c| theme::surface_colour(c, self.srgb)),
        };
        self.queue
            .write_buffer(&self.theme_buf, 0, bytemuck::bytes_of(&uniform));
    }

    /// Uploads the belts and the instances. Call again whenever an instance moves or appears.
    ///
    /// # Panics
    ///
    /// If an instance names a kind that `set_models` did not upload.
    pub fn set_scene(&mut self, scene: &Scene) {
        let pads = geometry::pad_vertices(&scene.pads);
        self.pads = (!pads.is_empty()).then(|| {
            (
                upload(
                    &self.device,
                    &self.queue,
                    "pads",
                    wgpu::BufferUsages::VERTEX,
                    &pads,
                ),
                pads.len() as u32,
            )
        });
        let ribbons = geometry::ribbon_vertices(&scene.ribbons);
        self.ribbons = Some((
            upload(
                &self.device,
                &self.queue,
                "ribbons",
                wgpu::BufferUsages::VERTEX,
                &ribbons,
            ),
            ribbons.len() as u32,
        ));
        let (raw, ranges) = geometry::instance_data(&scene.instances, self.models.len());
        self.instances = Some(upload(
            &self.device,
            &self.queue,
            "instances",
            wgpu::BufferUsages::VERTEX,
            &raw,
        ));
        self.ranges = ranges;

        let mut hue = geometry::hue_vertices(&scene.hue_lines);
        hue.extend(geometry::beam_vertices(&scene.beams));
        let quads = (hue.len() / geometry::QUAD_CORNERS as usize) as u32;
        let indices = geometry::quad_indices(quads);
        self.hue = (!hue.is_empty()).then(|| HueBuffers {
            vertices: upload(
                &self.device,
                &self.queue,
                "hue lines",
                wgpu::BufferUsages::VERTEX,
                &hue,
            ),
            indices: upload(
                &self.device,
                &self.queue,
                "hue indices",
                wgpu::BufferUsages::INDEX,
                &indices,
            ),
            index_count: indices.len() as u32,
        });
        self.silhouettes.clone_from(&scene.silhouettes);
    }

    /// Draws one frame at the given level of detail. Returns `Ok(false)` for a frame the surface
    /// skipped (timeout, occlusion, or a resize in flight).
    pub fn render(&mut self, camera: &Camera, lod: Lod) -> Result<bool, RenderError> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                self.surface.configure(&self.device, &self.config);
                f
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Lost => return Err(RenderError::SurfaceLost),
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(RenderError::SurfaceValidation);
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(false);
            }
        };
        self.queue.write_buffer(
            &self.camera_buf,
            0,
            bytemuck::bytes_of(&self.uniform(camera)),
        );
        let surface_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        let (scene_view, scene_resolve, ink_view, ink_resolve) = match &self.targets.msaa {
            Some(msaa) => (
                &msaa.colour,
                Some(&surface_view),
                &msaa.ink,
                Some(&self.targets.ink),
            ),
            None => (&surface_view, None, &self.targets.ink, None),
        };
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene"),
                color_attachments: &[Some(attachment(
                    scene_view,
                    scene_resolve,
                    wgpu::LoadOp::Clear(self.clear),
                ))],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.targets.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(FAR_DEPTH),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.draw_scene(&mut pass, lod);
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lines"),
                color_attachments: &[Some(attachment(
                    ink_view,
                    ink_resolve,
                    wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                ))],
                // Read-only: lines test against the scene's depth and leave it as it is.
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.targets.depth,
                    depth_ops: None,
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.uniform_bind, &[]);
            self.draw_strokes(&mut pass, &self.pipelines.edge, lod, |m| &m.edges);
            self.draw_strokes(&mut pass, &self.pipelines.line, lod, |m| &m.lines);
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ink"),
                color_attachments: &[Some(attachment(&surface_view, None, wgpu::LoadOp::Load))],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipelines.ink);
            pass.set_bind_group(0, &self.uniform_bind, &[]);
            pass.set_bind_group(1, &self.ink_bind, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        Ok(true)
    }

    fn draw_scene(&self, pass: &mut wgpu::RenderPass<'_>, lod: Lod) {
        pass.set_bind_group(0, &self.uniform_bind, &[]);
        pass.set_pipeline(&self.pipelines.grid);
        pass.draw(0..3, 0..1);
        if let Some((pads, count)) = &self.pads {
            pass.set_pipeline(&self.pipelines.pad);
            pass.set_vertex_buffer(0, pads.slice(..));
            pass.draw(0..*count, 0..1);
        }

        let Some(instances) = &self.instances else {
            return;
        };
        if let Some((ribbons, count)) = &self.ribbons
            && *count > 0
        {
            pass.set_pipeline(&self.pipelines.ribbon);
            pass.set_vertex_buffer(0, ribbons.slice(..));
            pass.draw(0..*count, 0..1);
        }

        if let Some(halos) = &self.halos {
            pass.set_pipeline(&self.pipelines.halo);
            pass.set_vertex_buffer(0, halos.slice(..));
            pass.set_vertex_buffer(1, instances.slice(..));
            for (kind, range) in self.ranges.iter().enumerate() {
                if !range.is_empty() {
                    let first = 6 * kind as u32;
                    pass.draw(first..first + 6, range.clone());
                }
            }
        }

        pass.set_pipeline(&self.pipelines.mesh);
        for (model, range) in self.models.iter().zip(&self.ranges) {
            if range.is_empty() {
                continue;
            }
            for mesh in &model.levels[..=lod as usize] {
                if mesh.index_count == 0 {
                    continue;
                }
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.set_vertex_buffer(1, instances.slice(..));
                pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, range.clone());
            }
        }

        self.draw_silhouettes(pass, lod, instances);
        self.draw_strokes(pass, &self.pipelines.dot, lod, |m| &m.dots);
        self.draw_hue(pass);
    }

    /// Draws each silhouette kind's instances where the scene in front hides them. The pass
    /// reverses the depth test, so a part that is not hidden is left to the mesh pass.
    fn draw_silhouettes(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        lod: Lod,
        instances: &wgpu::Buffer,
    ) {
        pass.set_pipeline(&self.pipelines.silhouette);
        for &kind in &self.silhouettes {
            let (Some(model), Some(range)) = (
                self.models.get(usize::from(kind)),
                self.ranges.get(usize::from(kind)),
            ) else {
                continue;
            };
            if range.is_empty() {
                continue;
            }
            for mesh in &model.levels[..=lod as usize] {
                if mesh.index_count == 0 {
                    continue;
                }
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.set_vertex_buffer(1, instances.slice(..));
                pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, range.clone());
            }
        }
    }

    /// Draws the belt hue lines, over the ground and under anything that stands in front of it.
    fn draw_hue(&self, pass: &mut wgpu::RenderPass<'_>) {
        let Some(hue) = &self.hue else {
            return;
        };
        pass.set_pipeline(&self.pipelines.hue);
        pass.set_vertex_buffer(0, hue.vertices.slice(..));
        pass.set_index_buffer(hue.indices.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..hue.index_count, 0, 0..1);
    }

    /// Draws one kind of stroke of every model, instanced over the model's machines.
    fn draw_strokes(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        pipeline: &wgpu::RenderPipeline,
        lod: Lod,
        strokes: impl Fn(&ModelBuffers) -> &StrokeBuffers,
    ) {
        let (Some(instances), Some(quads)) = (&self.instances, &self.quads) else {
            return;
        };
        pass.set_pipeline(pipeline);
        pass.set_index_buffer(quads.slice(..), wgpu::IndexFormat::Uint32);
        for (model, range) in self.models.iter().zip(&self.ranges) {
            let strokes = strokes(model);
            let quad_count = strokes.counts[lod as usize] / geometry::QUAD_CORNERS;
            if quad_count == 0 || range.is_empty() {
                continue;
            }
            pass.set_vertex_buffer(0, strokes.buffer.slice(..));
            pass.set_vertex_buffer(1, instances.slice(..));
            pass.draw_indexed(0..6 * quad_count, 0, range.clone());
        }
    }
    fn uniform(&self, camera: &Camera) -> CameraUniform {
        let b = camera.basis();
        let pivot = camera.pivot();
        let eye = camera.eye();
        let [vw, vh] = camera.viewport();
        let d = camera.eye_distance();
        let ortho = match camera.projection() {
            Projection::Perspective => 0.0,
            Projection::Orthographic => 1.0,
        };
        let (fog_near, fog_far) = camera.fog_range();
        let (angle_lo, angle_hi) = theme::ANGLE_FADE;
        let encoded = if self.config.format.is_srgb() {
            0.0
        } else {
            1.0
        };
        CameraUniform {
            right: [b.right[0], b.right[1], b.right[2], 0.0],
            up: [b.up[0], b.up[1], b.up[2], 0.0],
            toward: [b.toward[0], b.toward[1], b.toward[2], 0.0],
            eye: [eye[0], eye[1], eye[2], ortho],
            pivot: [pivot[0], pivot[1], d, encoded],
            view: [vw, vh, camera.zoom(), MIN_STROKE_PX],
            depth: [
                NEAR_FRACTION * d,
                ORTHO_FAR * d,
                camera.focal(),
                1.0 - 2.0 * camera.axis_y() / vh,
            ],
            fog: [fog_near, fog_far, angle_lo, angle_hi],
        }
    }

    fn mesh_buffers(&self, mesh: &Mesh) -> MeshBuffers {
        let vertices = geometry::mesh_vertices(mesh);
        MeshBuffers {
            vertices: upload(
                &self.device,
                &self.queue,
                "mesh vertices",
                wgpu::BufferUsages::VERTEX,
                &vertices,
            ),
            indices: upload(
                &self.device,
                &self.queue,
                "mesh indices",
                wgpu::BufferUsages::INDEX,
                &mesh.indices,
            ),
            index_count: mesh.indices.len() as u32,
        }
    }

    fn strokes<V: Pod>(&self, label: &str, strokes: Strokes<V>) -> StrokeBuffers {
        StrokeBuffers {
            buffer: upload(
                &self.device,
                &self.queue,
                label,
                wgpu::BufferUsages::VERTEX,
                &strokes.vertices,
            ),
            counts: strokes.counts,
        }
    }
}

/// 4x when the surface format and both targets support it with resolve, otherwise 1.
fn sample_count(adapter: &wgpu::Adapter, surface: wgpu::TextureFormat) -> u32 {
    let supports = |format: wgpu::TextureFormat| {
        let flags = adapter.get_texture_format_features(format).flags;
        flags.sample_count_supported(MSAA)
    };
    let resolves = |format: wgpu::TextureFormat| {
        let flags = adapter.get_texture_format_features(format).flags;
        flags.contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_RESOLVE)
    };
    if resolves(surface)
        && resolves(INK_FORMAT)
        && supports(surface)
        && supports(INK_FORMAT)
        && supports(DEPTH_FORMAT)
    {
        MSAA
    } else {
        1
    }
}

fn targets(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    samples: u32,
    width: u32,
    height: u32,
) -> Targets {
    let make = |label: &str,
                format: wgpu::TextureFormat,
                sample_count: u32,
                usage: wgpu::TextureUsages| {
        device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: width.max(1),
                    height: height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default())
    };
    let drawn = wgpu::TextureUsages::RENDER_ATTACHMENT;
    Targets {
        msaa: (samples > 1).then(|| Multisampled {
            colour: make("scene colour", format, samples, drawn),
            ink: make("line ink", INK_FORMAT, samples, drawn),
        }),
        depth: make("scene depth", DEPTH_FORMAT, samples, drawn),
        ink: make(
            "resolved ink",
            INK_FORMAT,
            1,
            drawn | wgpu::TextureUsages::TEXTURE_BINDING,
        ),
    }
}

/// A colour attachment that resolves into \`resolve\` when there is one and then keeps nothing
/// of its own.
fn attachment<'a>(
    view: &'a wgpu::TextureView,
    resolve: Option<&'a wgpu::TextureView>,
    load: wgpu::LoadOp<wgpu::Color>,
) -> wgpu::RenderPassColorAttachment<'a> {
    wgpu::RenderPassColorAttachment {
        view,
        depth_slice: None,
        resolve_target: resolve,
        ops: wgpu::Operations {
            load,
            store: if resolve.is_some() {
                wgpu::StoreOp::Discard
            } else {
                wgpu::StoreOp::Store
            },
        },
    }
}

fn ink_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("ink input"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        }],
    })
}

fn ink_bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    targets: &Targets,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("ink input"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(&targets.ink),
        }],
    })
}

impl Pipelines {
    fn new(
        device: &wgpu::Device,
        shader: &wgpu::ShaderModule,
        uniform_layout: &wgpu::BindGroupLayout,
        ink_layout: &wgpu::BindGroupLayout,
        format: wgpu::TextureFormat,
        samples: u32,
    ) -> Self {
        let scene_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene"),
            bind_group_layouts: &[Some(uniform_layout)],
            immediate_size: 0,
        });
        let ink_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ink"),
            bind_group_layouts: &[Some(uniform_layout), Some(ink_layout)],
            immediate_size: 0,
        });

        let colour = |blend: Option<wgpu::BlendState>| {
            Some(wgpu::ColorTargetState {
                format,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })
        };
        let max = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Max,
        };
        let ink = Some(wgpu::ColorTargetState {
            format: INK_FORMAT,
            blend: Some(wgpu::BlendState {
                color: max,
                alpha: max,
            }),
            write_mask: wgpu::ColorWrites::ALL,
        });
        let multiply = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::OneMinusSrc,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let depth = |write: bool, compare: wgpu::CompareFunction| wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(write),
            depth_compare: Some(compare),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        };

        let mesh_vertex = wgpu::VertexBufferLayout {
            array_stride: size_of::<MeshVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Uint32],
        };
        let mesh_instance = wgpu::VertexBufferLayout {
            array_stride: size_of::<InstanceRaw>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &MESH_INSTANCE,
        };
        let edge_vertex = wgpu::VertexBufferLayout {
            array_stride: size_of::<EdgeVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x3, 3 => Float32x3, 4 => Float32x2, 5 => Uint32],
        };
        let edge_instance = wgpu::VertexBufferLayout {
            array_stride: size_of::<InstanceRaw>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &EDGE_INSTANCE,
        };
        let line_vertex = wgpu::VertexBufferLayout {
            array_stride: size_of::<LineVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2, 3 => Float32, 4 => Uint32],
        };
        let dot_vertex = wgpu::VertexBufferLayout {
            array_stride: size_of::<DotVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x3, 3 => Float32x2, 4 => Uint32],
        };
        let ribbon_vertex = wgpu::VertexBufferLayout {
            array_stride: size_of::<RibbonVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32, 2 => Float32, 3 => Float32, 4 => Float32],
        };
        let pad_vertex = wgpu::VertexBufferLayout {
            array_stride: size_of::<PadVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
        };
        let halo_vertex = wgpu::VertexBufferLayout {
            array_stride: size_of::<HaloVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4],
        };
        let hue_vertex = wgpu::VertexBufferLayout {
            array_stride: size_of::<HueVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2, 3 => Float32, 4 => Float32, 5 => Uint32, 6 => Float32x3, 7 => Float32x2, 8 => Float32],
        };
        let stroke_instance = wgpu::VertexBufferLayout {
            array_stride: size_of::<InstanceRaw>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &STROKE_INSTANCE,
        };
        let halo_instance = wgpu::VertexBufferLayout {
            array_stride: size_of::<InstanceRaw>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![2 => Float32x3],
        };

        let scene_samples = wgpu::MultisampleState {
            count: samples,
            ..Default::default()
        };
        let make = |label: &str,
                    layout: &wgpu::PipelineLayout,
                    (vs, fs): (&str, &str),
                    buffers: &[Option<wgpu::VertexBufferLayout>],
                    primitive: wgpu::PrimitiveState,
                    depth_state: Option<wgpu::DepthStencilState>,
                    multisample: wgpu::MultisampleState,
                    targets: &[Option<wgpu::ColorTargetState>]|
         -> wgpu::RenderPipeline {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive,
                depth_stencil: depth_state,
                multisample,
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets,
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let alpha = Some(wgpu::BlendState::ALPHA_BLENDING);
        let triangles = wgpu::PrimitiveState::default();
        // Depth is reverse, so a nearer surface has the larger value.
        let nearer = wgpu::CompareFunction::GreaterEqual;
        let mesh_depth = wgpu::DepthStencilState {
            // Reverse depth again: a push away from the eye is negative.
            bias: wgpu::DepthBiasState {
                constant: 0,
                slope_scale: -MESH_SLOPE_BIAS,
                clamp: 0.0,
            },
            ..depth(true, nearer)
        };
        Self {
            grid: make(
                "grid",
                &scene_layout,
                ("vs_full", "fs_grid"),
                &[],
                triangles,
                Some(depth(true, wgpu::CompareFunction::Always)),
                scene_samples,
                &[colour(None)],
            ),
            // A pad lies on the ground under the belts, blended over the grid, and writes no depth.
            pad: make(
                "pads",
                &scene_layout,
                ("vs_pad", "fs_pad"),
                &[Some(pad_vertex)],
                triangles,
                Some(depth(false, wgpu::CompareFunction::Always)),
                scene_samples,
                &[colour(alpha)],
            ),
            // Belts and shadows lie on the ground, which is drawn first and lies under them, so
            // they skip the depth test. Testing them against the ground's per-pixel depth lets
            // the grid through at the far samples of a pixel.
            ribbon: make(
                "ribbons",
                &scene_layout,
                ("vs_ribbon", "fs_ribbon"),
                &[Some(ribbon_vertex)],
                triangles,
                Some(depth(true, wgpu::CompareFunction::Always)),
                scene_samples,
                &[colour(None)],
            ),
            // Silhouettes read the mesh's vertices with the depth test reversed. They draw only
            // where something nearer hides them. Back faces are culled, as in the mesh pass.
            silhouette: make(
                "silhouette",
                &scene_layout,
                ("vs_mesh", "fs_silhouette"),
                &[Some(mesh_vertex.clone()), Some(mesh_instance.clone())],
                wgpu::PrimitiveState {
                    front_face: wgpu::FrontFace::Cw,
                    cull_mode: Some(wgpu::Face::Back),
                    ..triangles
                },
                Some(depth(false, wgpu::CompareFunction::Less)),
                scene_samples,
                &[colour(alpha)],
            ),
            // Back faces are culled. With the slope bias, a back wall could otherwise win the
            // depth test along the far edge of the top in front of it. The camera's screen axes
            // turn a face that winds counter-clockwise from outside clockwise on screen.
            mesh: make(
                "mesh",
                &scene_layout,
                ("vs_mesh", "fs_mesh"),
                &[Some(mesh_vertex), Some(mesh_instance)],
                wgpu::PrimitiveState {
                    front_face: wgpu::FrontFace::Cw,
                    cull_mode: Some(wgpu::Face::Back),
                    ..triangles
                },
                Some(mesh_depth),
                scene_samples,
                &[colour(None)],
            ),
            halo: make(
                "halo",
                &scene_layout,
                ("vs_halo", "fs_halo"),
                &[Some(halo_vertex), Some(halo_instance)],
                triangles,
                Some(depth(false, wgpu::CompareFunction::Always)),
                scene_samples,
                &[colour(alpha)],
            ),
            dot: make(
                "dots",
                &scene_layout,
                ("vs_dot", "fs_dot"),
                &[Some(dot_vertex), Some(stroke_instance.clone())],
                triangles,
                Some(depth(false, nearer)),
                scene_samples,
                &[colour(alpha)],
            ),
            // Hue lines lie on the ground, lifted toward the eye like the painted lines. They test
            // against the depth the machines left, so a machine in front hides them.
            hue: make(
                "hue",
                &scene_layout,
                ("vs_hue", "fs_hue"),
                &[Some(hue_vertex)],
                triangles,
                Some(depth(false, nearer)),
                scene_samples,
                &[colour(alpha)],
            ),
            edge: make(
                "edges",
                &scene_layout,
                ("vs_edge", "fs_stroke"),
                &[Some(edge_vertex), Some(edge_instance)],
                triangles,
                Some(depth(false, nearer)),
                scene_samples,
                std::slice::from_ref(&ink),
            ),
            line: make(
                "lines",
                &scene_layout,
                ("vs_line", "fs_stroke"),
                &[Some(line_vertex), Some(stroke_instance)],
                triangles,
                Some(depth(false, nearer)),
                scene_samples,
                &[ink],
            ),
            ink: make(
                "ink",
                &ink_pipeline_layout,
                ("vs_full", "fs_ink"),
                &[],
                triangles,
                None,
                wgpu::MultisampleState::default(),
                &[colour(Some(multiply))],
            ),
        }
    }
}
fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

const EMPTY: [u8; 16] = [0; 16];

/// Uploads `data`. An empty list gets a placeholder so the buffer is never zero sized. The bytes
/// go in with `write_buffer` rather than at creation: a browser's WebGPU can refuse a
/// mapped-at-creation buffer that it would take otherwise, and wgpu panics on that refusal.
fn upload<T: Pod>(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    usage: wgpu::BufferUsages,
    data: &[T],
) -> wgpu::Buffer {
    let bytes = bytemuck::cast_slice::<T, u8>(data);
    let contents = aligned(if bytes.is_empty() { &EMPTY[..] } else { bytes });
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: contents.len() as u64,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&buffer, 0, &contents);
    buffer
}

/// `bytes` with zeros after them up to a multiple of four, the alignment `write_buffer` takes.
fn aligned(bytes: &[u8]) -> Cow<'_, [u8]> {
    let pad = (4 - bytes.len() % 4) % 4;
    if pad == 0 {
        return Cow::Borrowed(bytes);
    }
    let mut padded = bytes.to_vec();
    padded.resize(bytes.len() + pad, 0);
    Cow::Owned(padded)
}

#[cfg(test)]
mod tests {
    use super::aligned;

    #[test]
    fn aligned_pads_with_zeros_to_four_bytes() {
        for len in 0usize..=9 {
            let bytes: Vec<u8> = (1..=len as u8).collect();
            let out = aligned(&bytes);
            assert_eq!(out.len() % 4, 0, "length {len}");
            assert_eq!(out.len(), len.div_ceil(4) * 4, "length {len}");
            assert_eq!(&out[..len], &bytes[..], "length {len}");
            assert!(out[len..].iter().all(|&b| b == 0), "length {len}");
        }
    }
}
