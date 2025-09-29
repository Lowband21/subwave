Acknowledgments (subwave_appsink)

This crate reuses and adapts small, well‑isolated parts of iced_video_player by jazzfool.

Upstream project:
- Repository: https://github.com/jazzfool/iced_video_player
- License: MIT OR Apache‑2.0
- Reference commit for diffs: a8656e8021f7a6c316760fffc84664b92e5abc61 (master)

What is directly derived
- NV12 WGSL shader
  - File: `subwave_appsink/src/shader.wgsl`
  - Status: content-identical to upstream `src/shader.wgsl` at the above commit; subwave adds a short provenance comment header.
  - Purpose: Sample Y (R8) + interleaved UV (RG8) textures and convert to RGB in the fragment stage.

- Core NV12 upload/draw pipeline structure (adapted)
  - File: `subwave_appsink/src/render_pipeline.rs`
  - Derived concepts preserved from upstream (`src/pipeline.rs`):
    - `Uniforms` with a `rect: [f32; 4]` and 256‑byte alignment padding for dynamic UBO offsets.
    - Bind group layout entries 0–3:
      - 0: Y texture view (R8Unorm)
      - 1: UV texture view (Rg8Unorm)
      - 2: filtering sampler
      - 3: uniform buffer with dynamic offset (per‑instance rect)
    - NV12 upload pattern:
      - Write Y plane: bytes_per_row = `width`, rows_per_image = `height`.
      - Write UV plane: interleaved RG8 at size `(width/2, height/2)`, bytes_per_row = `width`.
    - Prepare/render flow:
      - Write per‑primitive uniforms with dynamic offset, reset indices, begin render pass, bind group 0, scissor to clip, draw quad.
  - Our modifications:
    - WGPU API modernizations and type changes; renamed labels to `subwave …`.
    - Added binding 4 for a future HDR/video uniforms buffer and created the corresponding buffer resource.
    - Pipeline/log formatting adds target‑format diagnostics.
    - Type/struct naming aligned to subwave (e.g., `VideoRenderPipeline`).

What is only loosely inspired (substantially rewritten)
- GStreamer ingestion and runtime control
  - Files: `subwave_appsink/src/video.rs`, `subwave_appsink/src/internal.rs`, `subwave_appsink/src/video_player.rs`.
  - Major differences from upstream:
    - `playbin3` migration; explicit appsink bin construction; NV12 buffer copied into a shared `Vec<u8>` instead of storing `gst::Sample`.
    - Extended bus handling (e.g., Buffering, StreamCollection), reconnection heuristics, speed via `scaletempo`, cached position, audio/subtitle track management.
    - Different trait boundaries via `subwave_core` and a unified backend surface.

Licensing
- iced_video_player is MIT OR Apache‑2.0. This repository is also dual‑licensed MIT OR Apache‑2.0; the adapted portions are used under the same terms. See `LICENSE-MIT` and `LICENSE-APACHE`.
