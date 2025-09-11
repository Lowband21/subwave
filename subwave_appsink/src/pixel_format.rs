#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VideoPixelFormat {
    Nv12,   // 8-bit 4:2:0
    P010Le, // 10-bit 4:2:0
    P012Le, // 12-bit 4:2:0
    P016Le, // 16-bit 4:2:0
}

impl VideoPixelFormat {
    pub fn bit_depth(&self) -> u8 {
        match self {
            VideoPixelFormat::Nv12 => 8,
            VideoPixelFormat::P010Le => 10,
            VideoPixelFormat::P012Le => 12,
            VideoPixelFormat::P016Le => 16,
        }
    }

    pub fn y_texture_format(&self, _device: &wgpu::Device) -> wgpu::TextureFormat {
        match self {
            VideoPixelFormat::Nv12 => wgpu::TextureFormat::R8Unorm,
            VideoPixelFormat::P010Le | VideoPixelFormat::P012Le | VideoPixelFormat::P016Le => {
                // Try different formats for HDR support
                // First try R16Float which should be filterable
                wgpu::TextureFormat::R16Float
            }
        }
    }

    pub fn uv_texture_format(&self, _device: &wgpu::Device) -> wgpu::TextureFormat {
        match self {
            VideoPixelFormat::Nv12 => wgpu::TextureFormat::Rg8Unorm,
            VideoPixelFormat::P010Le | VideoPixelFormat::P012Le | VideoPixelFormat::P016Le => {
                // Try Rg16Float for HDR UV data
                wgpu::TextureFormat::Rg16Float
            }
        }
    }

    pub fn bytes_per_pixel(&self) -> usize {
        match self {
            VideoPixelFormat::Nv12 => 1,
            VideoPixelFormat::P010Le | VideoPixelFormat::P012Le | VideoPixelFormat::P016Le => 2,
        }
    }
}
