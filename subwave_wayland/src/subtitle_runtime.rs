use std::time::Duration;

use crate::{
    geometry::VideoRectangle,
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
    video_rectangle: VideoRectangle,
) -> Option<SubtitleBitmap> {
    if frames.is_empty() {
        return None;
    }

    let surf_w = surface_width.max(1) as usize;
    let surf_h = surface_height.max(1) as usize;
    let pgs_w = pgs_width.max(1) as f64;
    let pgs_h = pgs_height.max(1) as f64;
    let scale_x = video_rectangle.width.max(1) as f64 / pgs_w;
    let scale_y = video_rectangle.height.max(1) as f64 / pgs_h;
    let stride = surf_w * 4;
    let mut canvas = vec![0u8; stride * surf_h];

    for frame in frames {
        let frame_w = frame.width as usize;
        let frame_h = frame.height as usize;
        if frame_w == 0 || frame_h == 0 {
            continue;
        }

        let frame_left = video_rectangle.x as f64 + frame.x as f64 * scale_x;
        let frame_top = video_rectangle.y as f64 + frame.y as f64 * scale_y;
        let frame_right = frame_left + frame.width as f64 * scale_x;
        let frame_bottom = frame_top + frame.height as f64 * scale_y;
        let dest_x_start = (frame_left.floor() as i32).max(0) as usize;
        let dest_y_start = (frame_top.floor() as i32).max(0) as usize;
        let dest_x_end = (frame_right.ceil() as i32).max(0) as usize;
        let dest_y_end = (frame_bottom.ceil() as i32).max(0) as usize;
        let src_stride = frame_w * 4;

        for canvas_y in dest_y_start..dest_y_end.min(surf_h) {
            let source_y = ((canvas_y as f64 - frame_top) / scale_y).floor();
            if source_y < 0.0 || source_y >= frame_h as f64 {
                continue;
            }
            let src_row_offset = source_y as usize * src_stride;

            for canvas_x in dest_x_start..dest_x_end.min(surf_w) {
                let source_x = ((canvas_x as f64 - frame_left) / scale_x).floor();
                if source_x < 0.0 || source_x >= frame_w as f64 {
                    continue;
                }

                let src_offset = src_row_offset + source_x as usize * 4;
                let dst_offset = canvas_y * stride + canvas_x * 4;
                if src_offset + 4 <= frame.argb.len() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(bitmap: &SubtitleBitmap, x: usize, y: usize) -> &[u8] {
        let offset = y * bitmap.stride as usize + x * 4;
        &bitmap.data[offset..offset + 4]
    }

    #[test]
    fn pgs_composition_uses_centered_video_rectangle() {
        let frame = PgsFrame {
            argb: vec![1, 2, 3, 4],
            width: 1,
            height: 1,
            x: 0,
            y: 0,
        };
        let bitmap = compose_pgs_bitmap(
            &[frame],
            2,
            2,
            6,
            4,
            VideoRectangle {
                x: 1,
                y: 1,
                width: 4,
                height: 2,
            },
        )
        .expect("subtitle bitmap");

        assert_eq!(pixel(&bitmap, 0, 0), &[0, 0, 0, 0]);
        assert_eq!(pixel(&bitmap, 1, 1), &[1, 2, 3, 4]);
        assert_eq!(pixel(&bitmap, 2, 1), &[1, 2, 3, 4]);
        assert_eq!(pixel(&bitmap, 3, 1), &[0, 0, 0, 0]);
    }

    #[test]
    fn pgs_composition_clips_cover_rectangle() {
        let frame = PgsFrame {
            argb: vec![1, 0, 0, 255, 2, 0, 0, 255],
            width: 2,
            height: 1,
            x: 0,
            y: 0,
        };
        let bitmap = compose_pgs_bitmap(
            &[frame],
            2,
            1,
            2,
            1,
            VideoRectangle {
                x: -1,
                y: 0,
                width: 4,
                height: 1,
            },
        )
        .expect("subtitle bitmap");

        assert_eq!(pixel(&bitmap, 0, 0), &[1, 0, 0, 255]);
        assert_eq!(pixel(&bitmap, 1, 0), &[2, 0, 0, 255]);
    }
}
