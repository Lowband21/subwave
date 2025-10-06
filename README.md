# Subwave

Wayland‑first video playback for Iced via GStreamer. A unified API prefers a Wayland subsurface backend and falls back to an appsink + wgpu path when Wayland isn’t available.

<details>
  <summary><strong>Wayland caveat</strong></summary>

The Wayland integration uses a feature from my Iced fork called `wayland-hack`, which exposes the Wayland handles required for managing and creating subsurfaces.

I plan to rewrite the integration to expose a proper API when I have time.

</details>

**Status**: early/experimental. APIs will change.

**Crates**
- **Unified API** — one type, auto/forced backend selection.
- **Wayland backend** — renders with `playbin3` -> `vapostproc -> waylandsink` directly into a Wayland subsurface, targeting zero-copy output and HDR passthrough. Pipeline enables HDR tone‑mapping on `vapostproc` when available (Intel only and untested).
- **Appsink backend** — `playbin3` -> `videoconvertscale -> appsink` (NV12), frames uploaded and rendered via `iced_wgpu` with a small WGSL shader.
- **Controls** — play/pause, seek, rate, volume/mute, audio & subtitle track selection, external subtitle URI. Note that controls UI overlay is left up to the parent Iced application (use a transparent background layer and overlay on top of the video widget)

**Relationship to Ferrix**
- subwave is developed primarily to power video playback for Ferrix, a performance-first, self‑hosted media server and desktop player focused on responsive browsing and playback matching the performance and compatibility of native video players. Ferrix emphasizes desktop feel, aiming to be enjoyable to use, pleasant to look at, and to support HDR playback by leveraging bleeding-edge Wayland + GStreamer integration.

**Requirements**:
- Base GStreamer: playbin3 + videoconvertscale + appsink
- HDR Passthrough: vapostproc and waylandsink built from 1.27.x development releases
 - No pre-built binaries are available, refer to: [Building from source](https://gstreamer.freedesktop.org/documentation/installing/building-from-source-using-meson.html?gi-language=c)
- An HDR compatible Wayland desktop environment to use the wayland backend with HDR passthrough (tested on Hyprland)
- Iced fork ‘iced-ferrix’ with wgpu renderer; disable tiny-skia fallback.

**Notes**:
- Wayland environments prefer the subsurface backend by detecting the presense of the `WAYLAND_DISPLAY` env variable; otherwise the appsink backend is used.
- Wayland backend is WGPU-only: lock Iced to the `wgpu` renderer and disable the default tiny-skia fallback in your Iced application.

**Acknowledgments**
- The NV12 WGSL shader and the core NV12 upload/draw pipeline in `subwave_appsink` are adapted from `iced_video_player` by jazzfool and contributors. See `subwave_appsink/ACKNOWLEDGMENTS.md` for specific files, upstream commit, and details.
- `subwave_wayland` is a bespoke subsurface-based implementation designed for zero‑copy Wayland output and HDR passthrough, but intentionally exposes a similar API.

**License**
- MIT OR Apache‑2.0. See `LICENSE-MIT` and `LICENSE-APACHE`.
