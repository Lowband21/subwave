//! Wayland `wp-color-management-v1` integration for per-surface color tagging.
//!
//! When the compositor advertises `wp_color_manager_v1`, this module can tag the
//! **video surface** with its HDR image description (BT.2020 + PQ) so the
//! compositor knows to tone-map correctly.  The **subtitle surface** is left
//! **untagged** — per the protocol spec, untagged surfaces are treated as sRGB
//! by the compositor, which is exactly what we want for ARGB32 subtitle bitmaps.
//!
//! This eliminates the color-shift flicker that occurs when an SDR SHM subtitle
//! overlay is composited over an HDR DMABuf video plane without the compositor
//! knowing each surface's color space.

use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
    wp_color_manager_v1::{self, WpColorManagerV1},
    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
    wp_image_description_v1::{self, WpImageDescriptionV1},
};

use crate::Result;

/// HDR metadata extracted from GStreamer caps / tags.
#[derive(Debug, Clone)]
pub struct HdrMetadata {
    /// Mastering display colour volume primaries (CIE 1931 xy × 50000).
    /// Order: Rx, Ry, Gx, Gy, Bx, By, Wx, Wy
    /// e.g. from GStreamer: "34000:16000:13250:34500:7500:3000:15635:16450"
    pub mastering_primaries: Option<[u32; 8]>,
    /// Mastering display min luminance (× 10000) and max luminance (cd/m²).
    /// e.g. from "10000000:50" → min=50 (0.005 cd/m²), max=10000000 (1000 cd/m²)
    pub mastering_luminance_min: Option<u32>,
    pub mastering_luminance_max: Option<u32>,
    /// MaxCLL (cd/m²) from content-light-level first field
    pub max_cll: Option<u32>,
    /// MaxFALL (cd/m²) from content-light-level second field
    pub max_fall: Option<u32>,
}

impl HdrMetadata {
    /// Parse GStreamer's `mastering-display-info` string.
    ///
    /// Format: `"Rx:Ry:Gx:Gy:Bx:By:Wx:Wy:MaxLum:MinLum"`
    /// Values are CIE 1931 xy coordinates × 50000 and luminances in 1/10000 cd/m².
    pub fn parse_mastering_display(s: &str) -> Option<([u32; 8], u32, u32)> {
        let parts: Vec<u32> = s.split(':').filter_map(|p| p.parse().ok()).collect();
        if parts.len() >= 10 {
            let primaries = [
                parts[0], parts[1], parts[2], parts[3], parts[4], parts[5], parts[6], parts[7],
            ];
            // GStreamer gives max luminance first, then min
            let max_lum = parts[8];
            let min_lum = parts[9];
            Some((primaries, max_lum, min_lum))
        } else {
            None
        }
    }

    /// Parse GStreamer's `content-light-level` string.
    ///
    /// Format: `"MaxCLL:MaxFALL"` in cd/m².
    pub fn parse_content_light_level(s: &str) -> Option<(u32, u32)> {
        let parts: Vec<u32> = s.split(':').filter_map(|p| p.parse().ok()).collect();
        if parts.len() >= 2 {
            Some((parts[0], parts[1]))
        } else {
            None
        }
    }

    /// Detect whether a GStreamer colorimetry string indicates HDR (PQ/HLG).
    ///
    /// GStreamer colorimetry format: `"range:matrix:transfer:primaries"`
    /// where transfer=14 is SMPTE ST 2084 (PQ), transfer=15 is ARIB STD-B67 (HLG),
    /// and primaries=7 is BT.2020.
    pub fn is_hdr_colorimetry(colorimetry: &str) -> bool {
        let parts: Vec<&str> = colorimetry.split(':').collect();
        if parts.len() >= 4 {
            let transfer = parts[2];
            // 14 = SMPTE ST 2084 (PQ), 15 = ARIB STD-B67 (HLG)
            transfer == "14" || transfer == "15"
        } else {
            false
        }
    }

    /// Returns true if the pixel format can actually carry HDR data.
    /// 8-bit formats like BGRx/BGRA/NV12 cannot — they indicate that
    /// vapostproc already tone-mapped the content to SDR.
    pub fn is_hdr_capable_format(format: &str) -> bool {
        // Formats that can carry >8-bit or HDR data
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
                | "DMA_DRM" // DMABuf — format negotiated separately
        )
    }
}

/// Tracks everything we need to set color descriptions on surfaces.
pub struct ColorManager {
    pub(crate) manager: WpColorManagerV1,
    /// HDR image description (for video surface) — created on demand
    hdr_desc: Option<WpImageDescriptionV1>,
    /// Color management surface wrapper for the video surface
    video_cm_surface: Option<WpColorManagementSurfaceV1>,
    /// Whether we have successfully tagged the video surface
    video_tagged: bool,
    /// Currently applied metadata (so we can detect changes)
    applied_colorimetry: Option<String>,
    /// Compositor-advertised optional features
    pub(crate) supports_set_luminances: bool,
    pub(crate) supports_set_mastering_primaries: bool,
}

/// State for tracking image-description readiness via events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptionRole {
    HdrPq,
}

impl ColorManager {
    /// Try to bind `wp_color_manager_v1` from the registry globals.
    ///
    /// Returns `None` if the compositor does not advertise the protocol.
    pub(crate) fn bind_if_available(
        globals: &[(u32, String, u32)],
        registry: &wayland_client::protocol::wl_registry::WlRegistry,
        qh: &QueueHandle<super::subsurface_manager::State>,
    ) -> Option<Self> {
        let global = globals
            .iter()
            .find(|(_, iface, _)| iface == "wp_color_manager_v1")?;

        let version = global.2.min(2); // protocol is at version 2
        let manager: WpColorManagerV1 = registry.bind(global.0, version, qh, ());
        log::info!(
            "[color-mgmt] Bound wp_color_manager_v1 v{version} (global name={})",
            global.0
        );

        Some(Self {
            manager,
            hdr_desc: None,
            video_cm_surface: None,
            video_tagged: false,
            applied_colorimetry: None,
            supports_set_luminances: false,
            supports_set_mastering_primaries: false,
        })
    }

    /// Create the color-management surface wrapper (needed before we can tag).
    /// Does NOT set any image description yet — the surface remains in its
    /// compositor-default state (sRGB) until `tag_video_hdr` is called.
    pub(crate) fn wrap_video_surface(
        &mut self,
        video_surface: &wayland_client::protocol::wl_surface::WlSurface,
        qh: &QueueHandle<super::subsurface_manager::State>,
    ) {
        if self.video_cm_surface.is_none() {
            let cm_surface: WpColorManagementSurfaceV1 =
                self.manager.get_surface(video_surface, qh, ());
            self.video_cm_surface = Some(cm_surface);
            log::info!("[color-mgmt] Created color management surface wrapper for video");
        }
        // NOTE: subtitle surface is deliberately NOT wrapped.
        // Per the protocol: "By default, a surface does not have an associated
        // image description… Compositors should handle such surfaces as sRGB"
        // This is exactly what we want for ARGB32 subtitle overlays.
    }

    /// Tag the video surface with a BT.2020 + PQ HDR image description.
    ///
    /// Call this when the video caps indicate HDR content.  The description
    /// is created with the provided metadata (or sensible defaults) and applied
    /// to the video surface.  The subtitle surface is left untagged (sRGB).
    ///
    /// `colorimetry` is the GStreamer colorimetry string for change detection.
    pub(crate) fn tag_video_hdr(
        &mut self,
        colorimetry: &str,
        metadata: Option<&HdrMetadata>,
        video_surface: &wayland_client::protocol::wl_surface::WlSurface,
        qh: &QueueHandle<super::subsurface_manager::State>,
        event_queue: &mut wayland_client::EventQueue<super::subsurface_manager::State>,
    ) -> Result<()> {
        // Skip if we already applied the same colorimetry
        if self.applied_colorimetry.as_deref() == Some(colorimetry) {
            return Ok(());
        }

        // Ensure we have a CM surface wrapper
        self.wrap_video_surface(video_surface, qh);

        let cm_surface = self
            .video_cm_surface
            .as_ref()
            .ok_or_else(|| crate::Error::Wayland("No color management surface".into()))?;

        // Destroy old description if any
        if let Some(old) = self.hdr_desc.take() {
            old.destroy();
        }

        // Create new parametric HDR description
        let creator: WpImageDescriptionCreatorParamsV1 =
            self.manager.create_parametric_creator(qh, ());

        // Primaries: BT.2020
        creator.set_primaries_named(wp_color_manager_v1::Primaries::Bt2020);

        // Transfer function: ST 2084 (PQ)
        creator.set_tf_named(wp_color_manager_v1::TransferFunction::St2084Pq);

        // Luminances: reference white, min, max in cd/m²
        // set_luminances(min_lum, max_lum, reference_lum)
        //   min_lum:       minimum luminance (cd/m²) × 10000
        //   max_lum:       maximum luminance (cd/m²) unscaled
        //   reference_lum: reference white luminance (cd/m²) unscaled
        //
        // With ST 2084 PQ the protocol says max_lum is ignored and taken as
        // min_lum + 10000, but we still set it for correctness.
        //
        // GStreamer mastering-display-info format:
        //   "Rx:Ry:Gx:Gy:Bx:By:Wx:Wy:MaxLum:MinLum"
        //   MaxLum and MinLum are in units of 1/10000 cd/m²

        // ── Optional features (only if compositor advertises support) ──
        // Calling these without compositor support raises protocol error
        // `unsupported_feature`, which can corrupt the description or
        // disconnect us entirely.

        if self.supports_set_luminances {
            if let Some(meta) = metadata {
                if let (Some(max_raw), Some(min_raw)) = (meta.mastering_luminance_max, meta.mastering_luminance_min) {
                    let min_lum = min_raw;                     // already × 10000
                    let max_lum = (max_raw / 10000).max(1);   // convert to cd/m²
                    let reference_lum = 203_u32;               // PQ reference white
                    creator.set_luminances(min_lum, max_lum, reference_lum);
                    log::info!(
                        "[color-mgmt] Mastering luminance: min={min_raw}/10000 cd/m², max={max_lum} cd/m²"
                    );
                } else {
                    creator.set_luminances(50, 1000, 203);
                }
            } else {
                creator.set_luminances(50, 1000, 203);
            }
        } else {
            log::debug!("[color-mgmt] Skipping set_luminances (not advertised by compositor)");
        }

        if self.supports_set_mastering_primaries {
            if let Some(meta) = metadata {
                if let Some(prims) = meta.mastering_primaries {
                    let scale = |v: u32| -> i32 { (v * 20) as i32 };
                    creator.set_mastering_display_primaries(
                        scale(prims[0]), scale(prims[1]),
                        scale(prims[2]), scale(prims[3]),
                        scale(prims[4]), scale(prims[5]),
                        scale(prims[6]), scale(prims[7]),
                    );
                    log::info!("[color-mgmt] Set mastering display primaries from stream metadata");
                }

                if let Some(max_cll) = meta.max_cll {
                    creator.set_max_cll(max_cll);
                }
                if let Some(max_fall) = meta.max_fall {
                    creator.set_max_fall(max_fall);
                }
            }
        } else {
            log::debug!("[color-mgmt] Skipping mastering primaries/CLL/FALL (not advertised by compositor)");
        }

        let desc = creator.create(qh, DescriptionRole::HdrPq);

        // Roundtrip to receive the ready/failed event for the description
        let mut state = super::subsurface_manager::State::new();
        event_queue.roundtrip(&mut state).map_err(|e| {
            crate::Error::Wayland(format!("Roundtrip for HDR desc: {}", e))
        })?;

        // Apply to the video surface
        cm_surface.set_image_description(&desc, wp_color_manager_v1::RenderIntent::Perceptual);
        video_surface.commit();

        self.hdr_desc = Some(desc);
        self.video_tagged = true;
        self.applied_colorimetry = Some(colorimetry.to_string());

        log::info!(
            "[color-mgmt] Tagged video surface with BT.2020+PQ (colorimetry={colorimetry})"
        );

        Ok(())
    }

    /// Remove the HDR tag from the video surface, returning it to compositor
    /// default (sRGB).  Call when switching to SDR content.
    pub(crate) fn untag_video(
        &mut self,
        video_surface: &wayland_client::protocol::wl_surface::WlSurface,
    ) {
        if !self.video_tagged {
            return;
        }
        if let Some(ref cm) = self.video_cm_surface {
            cm.unset_image_description();
            video_surface.commit();
            log::info!("[color-mgmt] Removed HDR tag from video surface (back to sRGB default)");
        }
        if let Some(desc) = self.hdr_desc.take() {
            desc.destroy();
        }
        self.video_tagged = false;
        self.applied_colorimetry = None;
    }

    /// Returns true if the video surface is currently tagged with an HDR description.
    pub fn is_video_tagged_hdr(&self) -> bool {
        self.video_tagged
    }

    /// Clean up color management resources.
    pub fn destroy(&mut self) {
        if let Some(cm) = self.video_cm_surface.take() {
            cm.destroy();
        }
        if let Some(desc) = self.hdr_desc.take() {
            desc.destroy();
        }
        self.manager.destroy();
        self.video_tagged = false;
        self.applied_colorimetry = None;
        log::debug!("[color-mgmt] Destroyed color management resources");
    }
}

// ── Dispatch implementations ──────────────────────────────────────────────

impl Dispatch<WpColorManagerV1, ()> for super::subsurface_manager::State {
    fn event(
        state: &mut Self,
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
                log::info!("[color-mgmt] Compositor supports feature: {feature:?}");
                // Track features we care about
                use wayland_client::WEnum;
                match feature {
                    WEnum::Value(wp_color_manager_v1::Feature::SetLuminances) => {
                        state.cm_supports_set_luminances = true;
                    }
                    WEnum::Value(wp_color_manager_v1::Feature::SetMasteringDisplayPrimaries) => {
                        state.cm_supports_set_mastering_primaries = true;
                    }
                    _ => {}
                }
            }
            wp_color_manager_v1::Event::SupportedTfNamed { tf } => {
                log::debug!("[color-mgmt] Compositor supports transfer function: {tf:?}");
            }
            wp_color_manager_v1::Event::SupportedPrimariesNamed { primaries } => {
                log::debug!("[color-mgmt] Compositor supports primaries: {primaries:?}");
            }
            wp_color_manager_v1::Event::Done => {
                log::info!(
                    "[color-mgmt] Compositor capabilities done (luminances={}, mastering_primaries={})",
                    state.cm_supports_set_luminances,
                    state.cm_supports_set_mastering_primaries,
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagementSurfaceV1, ()> for super::subsurface_manager::State {
    fn event(
        _state: &mut Self,
        _proxy: &WpColorManagementSurfaceV1,
        _event: <WpColorManagementSurfaceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wp_color_management_surface_v1 has no events in the current protocol version
    }
}

impl Dispatch<WpImageDescriptionCreatorParamsV1, ()> for super::subsurface_manager::State {
    fn event(
        _state: &mut Self,
        _proxy: &WpImageDescriptionCreatorParamsV1,
        _event: <WpImageDescriptionCreatorParamsV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The params creator has no events (it is destroyed by the `create` request)
    }
}

impl Dispatch<WpImageDescriptionV1, DescriptionRole> for super::subsurface_manager::State {
    fn event(
        _state: &mut Self,
        _proxy: &WpImageDescriptionV1,
        event: wp_image_description_v1::Event,
        data: &DescriptionRole,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wp_image_description_v1::Event::Failed { msg, cause } => {
                log::error!(
                    "[color-mgmt] Image description {:?} FAILED: cause={cause:?} msg={msg}",
                    data
                );
            }
            _ => {
                // Ready events (ready / ready2) — logged for diagnostics
                log::info!("[color-mgmt] Image description {:?} event: {event:?}", data);
            }
        }
    }
}
