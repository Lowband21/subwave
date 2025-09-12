use iced::wgpu::TextureFormat;
use iced_wgpu::primitive::Primitive;
use iced_wgpu::wgpu;
use std::{
    collections::{BTreeMap, btree_map::Entry},
    num::NonZero,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

// Convert f32 to f16 bits (IEEE 754 half precision)
fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = (bits >> 16) & 0x8000;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x7fffff;

    if exponent == 0xff {
        // Infinity or NaN
        let mantissa_bits = if mantissa != 0 { 0x200 } else { 0 }; // Preserve NaN
        return (sign | 0x7c00 | mantissa_bits) as u16;
    }

    let exponent = exponent - 127 + 15;

    if exponent >= 31 {
        // Overflow - return infinity
        return (sign | 0x7c00) as u16;
    } else if exponent <= 0 {
        // Underflow - return zero
        return sign as u16;
    }

    let mantissa = mantissa >> 13;
    ((sign | (exponent << 10) as u32 | mantissa) & 0xffff) as u16
}

#[repr(C)]
struct Uniforms {
    rect: [f32; 4],
    // because wgpu min_uniform_buffer_offset_alignment
    _pad: [u8; 240],
}

struct VideoEntry {
    texture_y: wgpu::Texture,
    texture_uv: wgpu::Texture,
    instances: wgpu::Buffer,
    video_uniforms: wgpu::Buffer,
    bg0: wgpu::BindGroup,
    alive: Arc<AtomicBool>,
    //pixel_format: VideoPixelFormat,
    //tone_mapping_config: ToneMappingConfig,
    prepare_index: AtomicUsize,
    render_index: AtomicUsize,
}

struct VideoRenderPipeline {
    render_pipeline: wgpu::RenderPipeline,
    bg0_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    videos: BTreeMap<u64, VideoEntry>,
}

impl VideoRenderPipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        // Log the format we're using
        log::warn!("=== ICED VIDEO PIPELINE FORMAT ===");
        log::warn!("Creating pipeline with render target format: {:?}", format);
        log::warn!("Format details:");
        match format {
            wgpu::TextureFormat::Bgra8Unorm => log::warn!("  8-bit BGRA (standard)"),
            wgpu::TextureFormat::Bgra8UnormSrgb => log::warn!("  8-bit BGRA sRGB"),
            wgpu::TextureFormat::Rgba8Unorm => log::warn!("  8-bit RGBA"),
            wgpu::TextureFormat::Rgba8UnormSrgb => log::warn!("  8-bit RGBA sRGB"),
            wgpu::TextureFormat::Rgb10a2Unorm => log::warn!("  10-bit RGB with 2-bit alpha"),
            wgpu::TextureFormat::Rg11b10Ufloat => log::warn!("  11-11-10 float (HDR)"),
            wgpu::TextureFormat::Rgba16Float => log::warn!("  16-bit float RGBA (HDR)"),
            _ => log::warn!("  Other format: {:?}", format),
        }
        log::warn!("==================================");

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("video shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let bg0_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("iced_video_player bind group 0 layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
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
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // Add video uniforms for HDR shader (binding 4)
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("subwave render pipeline layout"),
            bind_group_layouts: &[&bg0_layout],
            push_constant_ranges: &[],
        });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("subwave render pipeline"),
            layout: Some(&layout),
            cache: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("subwave sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            lod_min_clamp: 0.0,
            lod_max_clamp: 1.0,
            compare: None,
            anisotropy_clamp: 1,
            border_color: None,
        });

        VideoRenderPipeline {
            render_pipeline,
            bg0_layout,
            sampler,
            videos: BTreeMap::new(),
        }
    }

    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        video_id: u64,
        alive: &Arc<AtomicBool>,
        (width, height): (u32, u32),
        frame: &[u8],
        format: TextureFormat,
        //color_range: crate::video_properties::ColorRange,
        //matrix_coefficients: crate::gst_utils::colorimetry::MatrixCoefficients,
        //transfer_function: crate::gst_utils::colorimetry::TransferFunction,
        //tone_mapping_config: &ToneMappingConfig,
    ) {
        if let Entry::Vacant(entry) = self.videos.entry(video_id) {
            // For now we assume NV12 input from appsink: Y plane (R8) and interleaved UV plane (RG8)
            // In the future, detect caps and pick from pixel_format.rs
            let y_format = wgpu::TextureFormat::R8Unorm;
            let uv_format = wgpu::TextureFormat::Rg8Unorm;

            log::debug!(
                "Creating textures for NV12: Y={:?}, UV={:?}, frame={}x{}",
                y_format,
                uv_format,
                width,
                height
            );

            let texture_y = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("subwave texture Y (R8)"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: y_format,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });

            let texture_uv = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("subwave texture UV (RG8)"),
                size: wgpu::Extent3d {
                    width: width / 2,
                    height: height / 2,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: uv_format,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });

            let view_y = texture_y.create_view(&wgpu::TextureViewDescriptor {
                label: Some("subwave texture view"),
                format: None,
                dimension: None,
                aspect: wgpu::TextureAspect::All,
                base_mip_level: 0,
                mip_level_count: None,
                base_array_layer: 0,
                array_layer_count: None,
                usage: None,
            });

            let view_uv = texture_uv.create_view(&wgpu::TextureViewDescriptor {
                label: Some("subwave texture view"),
                format: None,
                dimension: None,
                aspect: wgpu::TextureAspect::All,
                base_mip_level: 0,
                mip_level_count: None,
                base_array_layer: 0,
                array_layer_count: None,
                usage: None,
            });

            let instances = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("subwave uniform buffer"),
                size: 256 * std::mem::size_of::<Uniforms>() as u64, // max 256 video players per frame
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
                mapped_at_creation: false,
            });

            // Create video uniforms buffer for HDR parameters
            // VideoUniforms struct in shader:
            // - color_matrix_r: vec4<f32> (16 bytes)
            // - color_matrix_g: vec4<f32> (16 bytes)
            // - color_matrix_b: vec4<f32> (16 bytes)
            // - range_y: vec2<f32> (8 bytes)
            // - range_uv: vec2<f32> (8 bytes)
            // - tone_map_params: vec4<f32> (16 bytes)
            // - algorithm_params: vec4<f32> (16 bytes)
            // - transfer_func_info: vec4<f32> (16 bytes)
            // Total: 112 bytes
            let video_uniforms = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("subwave video uniforms"),
                size: 112, // Size accounting for WGSL alignment rules
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
                mapped_at_creation: false,
            });

            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("iced_video_player bind group"),
                layout: &self.bg0_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view_y),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view_uv),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &instances,
                            offset: 0,
                            size: Some(NonZero::new(std::mem::size_of::<Uniforms>() as _).unwrap()),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &video_uniforms,
                            offset: 0,
                            size: None,
                        }),
                    },
                ],
            });

            entry.insert(VideoEntry {
                texture_y,
                texture_uv,
                instances,
                video_uniforms,
                bg0: bind_group,
                alive: Arc::clone(alive),
                //pixel_format,
                //tone_mapping_config: tone_mapping_config.clone(),
                prepare_index: AtomicUsize::new(0),
                render_index: AtomicUsize::new(0),
            });
        }

        let VideoEntry {
            texture_y,
            texture_uv,
            ..
        } = self.videos.get(&video_id).unwrap();

        // Write Y plane (R8), bytes_per_row = width bytes
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: texture_y,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame[..(width * height) as usize],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        // Write interleaved UV plane (RG8), bytes_per_row = (width/2) * 2 = width
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: texture_uv,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame[(width * height) as usize..],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height / 2),
            },
            wgpu::Extent3d {
                width: width / 2,
                height: height / 2,
                depth_or_array_layers: 1,
            },
        );
    }

    fn cleanup(&mut self) {
        let ids: Vec<_> = self
            .videos
            .iter()
            .filter_map(|(id, entry)| (!entry.alive.load(Ordering::SeqCst)).then_some(*id))
            .collect();
        for id in ids {
            if let Some(video) = self.videos.remove(&id) {
                video.texture_y.destroy();
                video.texture_uv.destroy();
                video.instances.destroy();
            }
        }
    }

    fn reset_textures(&mut self, video_id: u64) {
        // Force texture recreation by removing the video entry
        if let Some(video) = self.videos.remove(&video_id) {
            video.texture_y.destroy();
            video.texture_uv.destroy();
            video.instances.destroy();
            video.video_uniforms.destroy();
            log::info!("Reset textures for video {}", video_id);
        }
    }

    fn prepare(&mut self, queue: &wgpu::Queue, video_id: u64, bounds: &iced::Rectangle) {
        if let Some(video) = self.videos.get_mut(&video_id) {
            let uniforms = Uniforms {
                rect: [
                    bounds.x,
                    bounds.y,
                    bounds.x + bounds.width,
                    bounds.y + bounds.height,
                ],
                _pad: [0; 240],
            };
            queue.write_buffer(
                &video.instances,
                (video.prepare_index.load(Ordering::Relaxed) * std::mem::size_of::<Uniforms>())
                    as u64,
                unsafe {
                    std::slice::from_raw_parts(
                        &uniforms as *const _ as *const u8,
                        std::mem::size_of::<Uniforms>(),
                    )
                },
            );
            video.prepare_index.fetch_add(1, Ordering::Relaxed);
            video.render_index.store(0, Ordering::Relaxed);
        }

        self.cleanup();
    }

    fn draw(
        &self,
        target: &wgpu::TextureView,
        encoder: &mut wgpu::CommandEncoder,
        clip: &iced::Rectangle<u32>,
        video_id: u64,
    ) {
        if let Some(video) = self.videos.get(&video_id) {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("iced_video_player render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            pass.set_pipeline(&self.render_pipeline);
            pass.set_bind_group(
                0,
                &video.bg0,
                &[
                    (video.render_index.load(Ordering::Relaxed) * std::mem::size_of::<Uniforms>())
                        as u32,
                ],
            );
            pass.set_scissor_rect(clip.x as _, clip.y as _, clip.width as _, clip.height as _);
            pass.draw(0..6, 0..1);

            video.prepare_index.store(0, Ordering::Relaxed);
            video.render_index.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct VideoPrimitive {
    video_id: u64,
    alive: Arc<AtomicBool>,
    frame: Arc<Mutex<Vec<u8>>>,
    size: (u32, u32),
    upload_frame: bool,
    format: TextureFormat,
}

impl VideoPrimitive {
    pub fn new(
        video_id: u64,
        alive: Arc<AtomicBool>,
        frame: Arc<Mutex<Vec<u8>>>,
        size: (u32, u32),
        upload_frame: bool,
        format: TextureFormat,
    ) -> Self {
        VideoPrimitive {
            video_id,
            alive,
            frame,
            size,
            upload_frame,
            format,
        }
    }
}

impl Primitive for VideoPrimitive {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        storage: &mut iced_wgpu::primitive::Storage,
        bounds: &iced::Rectangle,
        viewport: &iced_wgpu::graphics::Viewport,
    ) {
        if !storage.has::<VideoRenderPipeline>() {
            log::warn!(
                "VideoPrimitive::prepare creating new pipeline with format: {:?}",
                format
            );
            eprintln!("=== CREATING VIDEO PIPELINE ===");
            eprintln!("Surface format: {:?}", format);
            eprintln!("===============================");
            storage.store(VideoRenderPipeline::new(device, format));
        }

        let pipeline = storage.get_mut::<VideoRenderPipeline>().unwrap();

        if self.upload_frame {
            let frame = self.frame.lock().expect("lock frame mutex");
            if !frame.is_empty() {
                pipeline.upload(
                    device,
                    queue,
                    self.video_id,
                    &self.alive,
                    self.size,
                    &frame,
                    format,
                );
            }
        }

        pipeline.prepare(
            queue,
            self.video_id,
            &(*bounds
                * iced::Transformation::orthographic(
                    viewport.logical_size().width as _,
                    viewport.logical_size().height as _,
                )),
        );
    }

    fn render(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        storage: &iced_wgpu::primitive::Storage,
        target: &wgpu::TextureView,
        clip_bounds: &iced::Rectangle<u32>,
    ) {
        let pipeline = storage.get::<VideoRenderPipeline>().unwrap();
        pipeline.draw(target, encoder, clip_bounds, self.video_id);
    }
}
