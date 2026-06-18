use std::time::Duration;

use crate::{
    pgs_decoder::PgsFrame,
    subtitle_scheduler::{DecodedSubtitleEvent, SubtitleAction, SubtitleScheduler},
};

/// The subtitle stream currently allowed to enqueue Wayland subtitle cues.
#[derive(Debug, Default)]
pub(crate) struct ActiveSubtitleSelection {
    pub(crate) stream_id: Option<String>,
    pub(crate) generation: u64,
}

impl ActiveSubtitleSelection {
    pub(crate) fn set_stream(&mut self, stream_id: Option<String>) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.stream_id = stream_id;
        self.generation
    }

    pub(crate) fn flush(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.generation
    }
}

/// Subtitle payloads decoded by GStreamer pad probes and presented later on the UI tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WaylandSubtitlePayload {
    /// One complete PGS display set, in original PGS/video coordinates.
    Pgs {
        frames: Vec<PgsFrame>,
        video_width: u16,
        video_height: u16,
    },
    /// A text cue that should be rendered at presentation time using the current surface size.
    Text(String),
}

/// A fully rasterized subtitle buffer ready for `wl_shm` attachment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubtitleBitmap {
    pub(crate) data: Vec<u8>,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) stride: i32,
}

/// Events sent from subtitle pad probes to the UI/tick thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SubtitleProbeEvent {
    Decoded(DecodedSubtitleEvent<WaylandSubtitlePayload>),
    /// A downstream flush/seek/rate discontinuity invalidated pending cues for this generation.
    Invalidate {
        stream_id: String,
        generation: u64,
    },
}

pub(crate) type WaylandSubtitleScheduler = SubtitleScheduler<WaylandSubtitlePayload>;
pub(crate) type WaylandSubtitleAction = SubtitleAction<WaylandSubtitlePayload>;

pub(crate) fn compose_pgs_bitmap(
    frames: &[PgsFrame],
    pgs_width: u16,
    pgs_height: u16,
    surface_width: i32,
    surface_height: i32,
) -> Option<SubtitleBitmap> {
    if frames.is_empty() {
        return None;
    }

    let surf_w = surface_width.max(1) as usize;
    let surf_h = surface_height.max(1) as usize;
    let pgs_w = pgs_width.max(1) as f64;
    let pgs_h = pgs_height.max(1) as f64;
    let scale_x = surf_w as f64 / pgs_w;
    let scale_y = surf_h as f64 / pgs_h;
    let stride = surf_w * 4;
    let mut canvas = vec![0u8; stride * surf_h];

    for frame in frames {
        let frame_w = frame.width as usize;
        let frame_h = frame.height as usize;
        if frame_w == 0 || frame_h == 0 {
            continue;
        }

        let fx = (frame.x as f64 * scale_x) as usize;
        let fy = (frame.y as f64 * scale_y) as usize;
        let scaled_fw = ((frame.width as f64) * scale_x).ceil().max(1.0) as usize;
        let scaled_fh = ((frame.height as f64) * scale_y).ceil().max(1.0) as usize;
        let src_stride = frame_w * 4;

        for dy in 0..scaled_fh {
            let canvas_y = fy + dy;
            if canvas_y >= surf_h {
                break;
            }
            let src_row = ((dy as f64) / scale_y).floor() as usize;
            if src_row >= frame_h {
                continue;
            }
            let src_row_offset = src_row * src_stride;

            for dx in 0..scaled_fw {
                let canvas_x = fx + dx;
                if canvas_x >= surf_w {
                    break;
                }
                let src_col = ((dx as f64) / scale_x).floor() as usize;
                if src_col >= frame_w {
                    continue;
                }

                let src_offset = src_row_offset + src_col * 4;
                let dst_offset = canvas_y * stride + canvas_x * 4;
                if src_offset + 4 <= frame.argb.len() && dst_offset + 4 <= canvas.len() {
                    canvas[dst_offset..dst_offset + 4]
                        .copy_from_slice(&frame.argb[src_offset..src_offset + 4]);
                }
            }
        }
    }

    Some(SubtitleBitmap {
        data: canvas,
        width: surf_w as i32,
        height: surf_h as i32,
        stride: stride as i32,
    })
}

pub(crate) fn duration_from_clock_time(clock_time: gstreamer::ClockTime) -> Duration {
    Duration::from_nanos(clock_time.nseconds())
}
