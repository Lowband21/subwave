//! Wayland `wp-color-management-v1` capability discovery.
//!
//! The surface Subwave gives to `waylandsink` is only a transparent mapping
//! ancestor. It does not carry video pixels and must not receive the stream's
//! HDR image description. GStreamer 1.28+ applies color metadata to the nested
//! surface that carries the actual video buffer. Subwave keeps both its mapping
//! anchor and subtitle surface untagged so compositors treat them as SDR/sRGB.

use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::color_management::v1::client::wp_color_manager_v1::{
    self, WpColorManagerV1,
};

/// HDR metadata extracted from GStreamer caps / tags.
///
/// Retained as public API for callers that inspect stream metadata. Subwave no
/// longer applies this metadata to its transparent Wayland host surface.
#[derive(Debug, Clone)]
pub struct HdrMetadata {
    /// Mastering display colour volume primaries (CIE 1931 xy × 50000).
    /// Order: Rx, Ry, Gx, Gy, Bx, By, Wx, Wy
    /// e.g. from GStreamer: "34000:16000:13250:34500:7500:3000:15635:16450"
    pub mastering_primaries: Option<[u32; 8]>,
    /// Mastering display minimum luminance (× 10000).
    pub mastering_luminance_min: Option<u32>,
    /// Mastering display maximum luminance (× 10000).
    pub mastering_luminance_max: Option<u32>,
    /// MaxCLL (cd/m²) from content-light-level first field.
    pub max_cll: Option<u32>,
    /// MaxFALL (cd/m²) from content-light-level second field.
    pub max_fall: Option<u32>,
}

impl HdrMetadata {
    /// Parse GStreamer's `mastering-display-info` string.
    ///
    /// Format: `"Rx:Ry:Gx:Gy:Bx:By:Wx:Wy:MaxLum:MinLum"`.
    /// Coordinates use units of 1/50000 and luminances use units of
    /// 1/10000 cd/m².
    pub fn parse_mastering_display(s: &str) -> Option<([u32; 8], u32, u32)> {
        let parts: Vec<u32> = s.split(':').filter_map(|part| part.parse().ok()).collect();
        if parts.len() < 10 {
            return None;
        }

        let primaries = [
            parts[0], parts[1], parts[2], parts[3], parts[4], parts[5], parts[6], parts[7],
        ];
        Some((primaries, parts[8], parts[9]))
    }

    /// Parse GStreamer's `content-light-level` string.
    ///
    /// Format: `"MaxCLL:MaxFALL"` in cd/m².
    pub fn parse_content_light_level(s: &str) -> Option<(u32, u32)> {
        let parts: Vec<u32> = s.split(':').filter_map(|part| part.parse().ok()).collect();
        if parts.len() < 2 {
            return None;
        }

        Some((parts[0], parts[1]))
    }

    /// Detect whether a GStreamer colorimetry string uses PQ or HLG transfer.
    pub fn is_hdr_colorimetry(colorimetry: &str) -> bool {
        let parts: Vec<&str> = colorimetry.split(':').collect();
        if parts.len() < 4 {
            return false;
        }

        // 14 = SMPTE ST 2084 (PQ), 15 = ARIB STD-B67 (HLG).
        matches!(parts[2], "14" | "15")
    }

    /// Return whether a pixel format can carry HDR data.
    pub fn is_hdr_capable_format(format: &str) -> bool {
        matches!(
            format,
            "P010_10LE"
                | "P010_10BE"
                | "P012_LE"
                | "P012_BE"
                | "P016_LE"
                | "P016_BE"
                | "Y410"
                | "Y412_LE"
                | "Y412_BE"
                | "Y210"
                | "Y212_LE"
                | "Y212_BE"
                | "VUYA"
                | "BGR10A2_LE"
                | "RGB10A2_LE"
                | "DMA_DRM"
        )
    }
}

/// Retains the compositor color-manager global as a capability signal.
pub struct ColorManager {
    manager: WpColorManagerV1,
}

impl ColorManager {
    /// Bind `wp_color_manager_v1` when advertised by the compositor.
    pub(crate) fn bind_if_available(
        globals: &[(u32, String, u32)],
        registry: &wayland_client::protocol::wl_registry::WlRegistry,
        qh: &QueueHandle<super::subsurface_manager::State>,
    ) -> Option<Self> {
        let global = globals
            .iter()
            .find(|(_, interface, _)| interface == "wp_color_manager_v1")?;
        let version = global.2.min(2);
        let manager: WpColorManagerV1 = registry.bind(global.0, version, qh, ());
        log::info!(
            "[color-mgmt] Bound wp_color_manager_v1 v{version}; waylandsink owns video tagging"
        );
        Some(Self { manager })
    }

    /// Destroy the capability handle.
    pub fn destroy(&mut self) {
        self.manager.destroy();
        log::debug!("[color-mgmt] Destroyed color management capability handle");
    }
}

impl Dispatch<WpColorManagerV1, ()> for super::subsurface_manager::State {
    fn event(
        _state: &mut Self,
        _proxy: &WpColorManagerV1,
        event: wp_color_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wp_color_manager_v1::Event::SupportedIntent { render_intent } => {
                log::debug!("[color-mgmt] Compositor supports render intent: {render_intent:?}");
            }
            wp_color_manager_v1::Event::SupportedFeature { feature } => {
                log::debug!("[color-mgmt] Compositor supports feature: {feature:?}");
            }
            wp_color_manager_v1::Event::SupportedTfNamed { tf } => {
                log::debug!("[color-mgmt] Compositor supports transfer function: {tf:?}");
            }
            wp_color_manager_v1::Event::SupportedPrimariesNamed { primaries } => {
                log::debug!("[color-mgmt] Compositor supports primaries: {primaries:?}");
            }
            wp_color_manager_v1::Event::Done => {
                log::info!("[color-mgmt] Compositor capability advertisement complete");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HdrMetadata;

    #[test]
    fn detects_hdr_transfer_functions() {
        assert!(HdrMetadata::is_hdr_colorimetry("0:0:14:7"));
        assert!(HdrMetadata::is_hdr_colorimetry("0:0:15:7"));
        assert!(!HdrMetadata::is_hdr_colorimetry("0:0:1:1"));
        assert!(!HdrMetadata::is_hdr_colorimetry("invalid"));
    }

    #[test]
    fn distinguishes_hdr_capable_formats() {
        assert!(HdrMetadata::is_hdr_capable_format("P010_10LE"));
        assert!(HdrMetadata::is_hdr_capable_format("DMA_DRM"));
        assert!(!HdrMetadata::is_hdr_capable_format("BGRA"));
        assert!(!HdrMetadata::is_hdr_capable_format("NV12"));
    }
}
