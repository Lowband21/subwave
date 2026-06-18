//! Streaming PGS (Presentation Graphic Stream) subtitle decoder.
//!
//! Decodes `subpicture/x-pgs` buffers received from GStreamer into ARGB
//! bitmaps suitable for the Wayland subtitle subsurface.
//!
//! PGS segments arrive individually as GStreamer buffers.  Each buffer
//! contains one segment (no 2-byte PG sync header — GStreamer strips it).
//! A complete subtitle frame ("display set") is assembled from multiple
//! segments: PCS → WDS → PDS → ODS → END.

use std::time::Duration;

/// A fully decoded PGS subtitle image ready for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgsFrame {
    /// ARGB pixel data (premultiplied alpha, row-major).
    pub argb: Vec<u8>,
    /// Width of the image in pixels.
    pub width: u32,
    /// Height of the image in pixels.
    pub height: u32,
    /// X position on the video frame.
    pub x: u16,
    /// Y position on the video frame.
    pub y: u16,
}

/// Presentation action represented by a complete PGS display set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PgsDisplaySet {
    /// Present one or more decoded composition objects.
    Show(Vec<PgsFrame>),
    /// Clear the currently visible subtitle composition.
    Clear,
}

/// A decoded PGS display set with an optional raw PGS PTS timestamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimedPgsDisplaySet {
    pub action: PgsDisplaySet,
    /// Raw PGS segment PTS converted from 90 kHz ticks to stream time.
    pub pts: Option<Duration>,
}

/// Accumulates PGS segments and produces decoded display sets.
pub struct PgsDecoder {
    palette: Vec<[u8; 4]>, // 256 ARGB entries
    objects: Vec<OdsData>,
    compositions: Vec<CompositionObject>,
    seen_pcs: bool,
    pcs_composition_state: u8,
    pcs_object_count: u8,
    display_set_pts: Option<Duration>,
    /// Video dimensions from PCS (used for coordinate scaling)
    pub video_width: u16,
    pub video_height: u16,
}

struct OdsData {
    id: u16,
    width: u16,
    height: u16,
    rle: Vec<u8>,
}

#[derive(Clone)]
struct CompositionObject {
    object_id: u16,
    x: u16,
    y: u16,
}

// PGS segment types (the type byte within the segment header)
const SEG_PCS: u8 = 0x16; // Presentation Composition Segment
const SEG_WDS: u8 = 0x17; // Window Definition Segment
const SEG_PDS: u8 = 0x14; // Palette Definition Segment
const SEG_ODS: u8 = 0x15; // Object Definition Segment
const SEG_END: u8 = 0x80; // End of Display Set

fn pgs_timestamp_to_duration(timestamp_90khz: u32) -> Duration {
    Duration::from_nanos((timestamp_90khz as u64).saturating_mul(1_000_000_000) / 90_000)
}

impl Default for PgsDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PgsDecoder {
    pub fn new() -> Self {
        Self {
            palette: vec![[0, 0, 0, 0]; 256],
            objects: Vec::new(),
            compositions: Vec::new(),
            seen_pcs: false,
            pcs_composition_state: 0,
            pcs_object_count: 0,
            display_set_pts: None,
            video_width: 1920,
            video_height: 1080,
        }
    }

    /// Feed a raw PGS buffer from GStreamer.
    ///
    /// Matroska containers deliver PGS as complete display sets where all
    /// segments are concatenated in a single buffer.  Each segment has:
    ///   [1B type][2B size][...payload...]
    ///
    /// This method loops through all segments in the buffer and returns
    /// decoded display-set actions when an END segment is encountered. Display
    /// sets without a PCS are resource updates/preloads and do not emit actions.
    pub fn feed(&mut self, data: &[u8]) -> Vec<TimedPgsDisplaySet> {
        let mut results = Vec::new();
        let mut offset = 0usize;

        while offset < data.len() {
            // Try to parse a segment at the current offset.
            // PGS segments: [type:1][size:2][payload:size]
            // Some muxers include the 2-byte "PG" sync (0x50 0x47) before
            // each segment — skip it if present.
            let mut segment_pts = None;
            if offset + 2 <= data.len() && data[offset] == 0x50 && data[offset + 1] == 0x47 {
                // Skip "PG" sync marker
                offset += 2;
                // After sync: [PTS:4][DTS:4][type:1][size:2][payload...]
                if offset + 11 > data.len() {
                    break;
                }
                let pts = u32::from_be_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ]);
                segment_pts = Some(pgs_timestamp_to_duration(pts));
                // Skip PTS(4) + DTS(4) to get to type
                offset += 8;
            }

            if offset + 3 > data.len() {
                break;
            }

            let seg_type = data[offset];
            let seg_size = u16::from_be_bytes([data[offset + 1], data[offset + 2]]) as usize;
            let payload_start = offset + 3;
            let payload_end = payload_start + seg_size;

            if payload_end > data.len() {
                log::debug!(
                    "[pgs] Segment truncated: type=0x{:02x} size={} but only {} bytes remain",
                    seg_type,
                    seg_size,
                    data.len() - payload_start
                );
                break;
            }

            let payload = &data[payload_start..payload_end];

            if let Some(pts) = segment_pts {
                if self.display_set_pts.is_none() || seg_type == SEG_PCS {
                    self.display_set_pts = Some(pts);
                }
            }

            match seg_type {
                SEG_PCS => {
                    log::debug!("[pgs] PCS segment ({} bytes)", payload.len());
                    self.parse_pcs(payload);
                }
                SEG_WDS => {
                    log::debug!("[pgs] WDS segment ({} bytes)", payload.len());
                }
                SEG_PDS => {
                    log::debug!(
                        "[pgs] PDS segment ({} bytes, {} entries)",
                        payload.len(),
                        (payload.len().saturating_sub(2)) / 5
                    );
                    self.parse_pds(payload);
                }
                SEG_ODS => {
                    log::debug!("[pgs] ODS segment ({} bytes)", payload.len());
                    self.parse_ods(payload);
                }
                SEG_END => {
                    let pts = self.display_set_pts;
                    log::info!(
                        "[pgs] END — display set complete ({} objects, {} compositions, pcs={}, state=0x{:02x}, pts={:?})",
                        self.objects.len(),
                        self.compositions.len(),
                        self.seen_pcs,
                        self.pcs_composition_state,
                        pts
                    );
                    if let Some(action) = self.finish_display_set() {
                        results.push(TimedPgsDisplaySet { action, pts });
                    }
                }
                _ => {
                    log::debug!(
                        "[pgs] Unknown segment type: 0x{:02x} at offset {}",
                        seg_type,
                        offset
                    );
                    // Can't determine size — bail out of this buffer
                    break;
                }
            }

            offset = payload_end;
        }

        results
    }

    fn parse_pcs(&mut self, data: &[u8]) {
        if data.len() < 11 {
            return;
        }
        self.video_width = u16::from_be_bytes([data[0], data[1]]);
        self.video_height = u16::from_be_bytes([data[2], data[3]]);
        // data[4] = frame_rate
        // data[5..7] = composition_number (u16)
        let composition_state = data[7];
        // data[8] = palette_update_flag
        // data[9] = palette_id
        let num_objects = data[10];

        self.seen_pcs = true;
        self.pcs_composition_state = composition_state;
        self.pcs_object_count = num_objects;
        self.compositions.clear();

        if composition_state == 0x00 && num_objects == 0 {
            // Normal update with 0 objects = clear subtitle
            return;
        }

        let mut offset = 11;
        for _ in 0..num_objects {
            if offset + 8 > data.len() {
                break;
            }
            let object_id = u16::from_be_bytes([data[offset], data[offset + 1]]);
            // offset+2 = window_id
            let _cropped = data[offset + 3];
            let x = u16::from_be_bytes([data[offset + 4], data[offset + 5]]);
            let y = u16::from_be_bytes([data[offset + 6], data[offset + 7]]);
            self.compositions
                .push(CompositionObject { object_id, x, y });
            offset += 8;
            // If cropped flag is set (0x40), skip 8 more bytes of crop data
            if _cropped & 0x40 != 0 {
                offset += 8;
            }
        }
    }

    fn parse_pds(&mut self, data: &[u8]) {
        if data.len() < 2 {
            return;
        }
        // data[0] = palette_id, data[1] = palette_version
        let entries = &data[2..];
        // Each entry: [id:1][Y:1][Cr:1][Cb:1][A:1] = 5 bytes
        let mut i = 0;
        while i + 5 <= entries.len() {
            let idx = entries[i] as usize;
            let y_val = entries[i + 1] as f32;
            let cr = entries[i + 2] as f32;
            let cb = entries[i + 3] as f32;
            let a = entries[i + 4];

            // YCbCr (BT.709) → RGB conversion
            let r = (y_val + 1.402 * (cr - 128.0)).clamp(0.0, 255.0) as u8;
            let g =
                (y_val - 0.344136 * (cb - 128.0) - 0.714136 * (cr - 128.0)).clamp(0.0, 255.0) as u8;
            let b = (y_val + 1.772 * (cb - 128.0)).clamp(0.0, 255.0) as u8;

            if idx < 256 {
                // ARGB format (premultiplied for Wayland)
                let pa = a as u16;
                let pr = ((r as u16) * pa / 255) as u8;
                let pg = ((g as u16) * pa / 255) as u8;
                let pb = ((b as u16) * pa / 255) as u8;
                self.palette[idx] = [a, pr, pg, pb];
            }
            i += 5;
        }
    }

    fn parse_ods(&mut self, data: &[u8]) {
        if data.len() < 7 {
            return;
        }
        let object_id = u16::from_be_bytes([data[0], data[1]]);
        // data[2] = object_version
        let seq_flag = data[3];
        // data[4..7] = data_length (24-bit)

        let is_first = seq_flag & 0xC0 == 0xC0 || seq_flag & 0x80 != 0;
        let is_last = seq_flag & 0xC0 == 0xC0 || seq_flag & 0x40 != 0;

        if is_first {
            // First (or only) segment — contains width/height
            if data.len() < 11 {
                return;
            }
            let width = u16::from_be_bytes([data[7], data[8]]);
            let height = u16::from_be_bytes([data[9], data[10]]);
            let rle = data[11..].to_vec();

            // Replace or insert
            if let Some(obj) = self.objects.iter_mut().find(|o| o.id == object_id) {
                obj.width = width;
                obj.height = height;
                obj.rle = rle;
            } else {
                self.objects.push(OdsData {
                    id: object_id,
                    width,
                    height,
                    rle,
                });
            }
        } else {
            // Continuation segment — append RLE data
            let _rle_data = &data[4..]; // skip object_id(2) + version(1) + seq_flag(1)
                                        // Actually, continuation starts at offset 7 (after 3-byte data_length)
            let rle_data = if data.len() > 7 {
                &data[7..]
            } else {
                &data[4..]
            };
            if let Some(obj) = self.objects.iter_mut().find(|o| o.id == object_id) {
                obj.rle.extend_from_slice(rle_data);
            }
        }

        if is_last {
            log::trace!(
                "[pgs] ODS complete: id={} size={}",
                object_id,
                self.objects
                    .iter()
                    .find(|o| o.id == object_id)
                    .map(|o| o.rle.len())
                    .unwrap_or(0)
            );
        }
    }

    fn finish_display_set(&mut self) -> Option<PgsDisplaySet> {
        if !self.seen_pcs {
            // Some streams split palette/object resource updates into standalone
            // display sets.  They update decoder state for future compositions
            // but must not clear the currently visible subtitle.
            self.reset_display_set_state();
            return None;
        }

        if self.pcs_object_count == 0 {
            let is_normal_clear = self.pcs_composition_state == 0x00;
            self.reset_display_set_state();
            return is_normal_clear.then_some(PgsDisplaySet::Clear);
        }

        if self.compositions.is_empty() {
            self.reset_display_set_state();
            return None;
        }

        let mut frames = Vec::new();

        for comp in &self.compositions {
            let Some(obj) = self.objects.iter().find(|o| o.id == comp.object_id) else {
                continue;
            };

            if obj.width == 0 || obj.height == 0 {
                continue;
            }

            match self.decode_rle(obj) {
                Ok(argb) => {
                    frames.push(PgsFrame {
                        argb,
                        width: obj.width as u32,
                        height: obj.height as u32,
                        x: comp.x,
                        y: comp.y,
                    });
                }
                Err(e) => {
                    log::warn!("[pgs] RLE decode failed: {e}");
                }
            }
        }

        // Don't clear objects — they may be referenced by future compositions
        // (PGS allows reusing objects across display sets)
        self.reset_display_set_state();

        if frames.is_empty() {
            None
        } else {
            Some(PgsDisplaySet::Show(frames))
        }
    }

    fn reset_display_set_state(&mut self) {
        self.seen_pcs = false;
        self.pcs_composition_state = 0;
        self.pcs_object_count = 0;
        self.display_set_pts = None;
        self.compositions.clear();
    }

    fn decode_rle(&self, obj: &OdsData) -> Result<Vec<u8>, String> {
        let pixel_count = obj.width as usize * obj.height as usize;
        let mut argb = vec![0u8; pixel_count * 4];
        let mut pos = 0usize; // position in output pixels
        let mut i = 0usize; // position in RLE data

        let rle = &obj.rle;

        while i < rle.len() && pos < pixel_count {
            let byte = rle[i];
            i += 1;

            if byte != 0 {
                // Single pixel with palette index
                self.write_pixel(&mut argb, pos, byte);
                pos += 1;
            } else {
                // Run-length encoded sequence
                if i >= rle.len() {
                    break;
                }
                let flag = rle[i];
                i += 1;

                if flag == 0 {
                    // End of line — advance to next row boundary
                    let row_w = obj.width as usize;
                    if row_w > 0 {
                        let remainder = pos % row_w;
                        if remainder != 0 {
                            pos += row_w - remainder;
                        }
                    }
                } else if flag & 0xC0 == 0x00 {
                    // Short run of color 0: length = flag (6 bits)
                    let run = (flag & 0x3F) as usize;
                    for _ in 0..run.min(pixel_count - pos) {
                        self.write_pixel(&mut argb, pos, 0);
                        pos += 1;
                    }
                } else if flag & 0xC0 == 0x40 {
                    // Long run of color 0: length = (flag & 0x3F) << 8 | next
                    if i >= rle.len() {
                        break;
                    }
                    let run = (((flag & 0x3F) as usize) << 8) | rle[i] as usize;
                    i += 1;
                    for _ in 0..run.min(pixel_count - pos) {
                        self.write_pixel(&mut argb, pos, 0);
                        pos += 1;
                    }
                } else if flag & 0xC0 == 0x80 {
                    // Short run of color N: length = flag & 0x3F, color = next
                    if i >= rle.len() {
                        break;
                    }
                    let run = (flag & 0x3F) as usize;
                    let color = rle[i];
                    i += 1;
                    for _ in 0..run.min(pixel_count - pos) {
                        self.write_pixel(&mut argb, pos, color);
                        pos += 1;
                    }
                } else {
                    // 0xC0: Long run of color N
                    if i + 1 >= rle.len() {
                        break;
                    }
                    let run = (((flag & 0x3F) as usize) << 8) | rle[i] as usize;
                    let color = rle[i + 1];
                    i += 2;
                    for _ in 0..run.min(pixel_count - pos) {
                        self.write_pixel(&mut argb, pos, color);
                        pos += 1;
                    }
                }
            }
        }

        Ok(argb)
    }

    #[inline]
    fn write_pixel(&self, buf: &mut [u8], pos: usize, palette_idx: u8) {
        let offset = pos * 4;
        if offset + 4 <= buf.len() {
            let [a, r, g, b] = self.palette[palette_idx as usize];
            // WL_SHM_FORMAT_ARGB8888: on little-endian, bytes are [B, G, R, A]
            buf[offset] = b;
            buf[offset + 1] = g;
            buf[offset + 2] = r;
            buf[offset + 3] = a;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PgsDecoder, PgsDisplaySet, TimedPgsDisplaySet};
    use std::time::Duration;

    fn segment(segment_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(3 + payload.len());
        data.push(segment_type);
        data.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        data.extend_from_slice(payload);
        data
    }

    fn end_segment() -> Vec<u8> {
        segment(0x80, &[])
    }

    fn pcs_payload(num_objects: u8) -> Vec<u8> {
        pcs_payload_with_state(0x00, num_objects)
    }

    fn pcs_payload_with_state(composition_state: u8, num_objects: u8) -> Vec<u8> {
        let mut payload = vec![
            0x07,
            0x80, // width 1920
            0x04,
            0x38, // height 1080
            0x10, // frame rate marker
            0x00,
            0x01, // composition number
            composition_state,
            0x00, // palette update flag
            0x00, // palette id
            num_objects,
        ];

        if num_objects > 0 {
            payload.extend_from_slice(&[
                0x00, 0x01, // object id
                0x00, // window id
                0x00, // cropped flag
                0x00, 0x0a, // x
                0x00, 0x14, // y
            ]);
        }

        payload
    }

    fn ods_payload() -> Vec<u8> {
        vec![
            0x00, 0x01, // object id
            0x00, // object version
            0xc0, // first and last ODS fragment
            0x00, 0x00, 0x05, // object data length
            0x00, 0x01, // width
            0x00, 0x01, // height
            0x01, // one pixel using palette index 1
        ]
    }

    fn pds_payload() -> Vec<u8> {
        vec![
            0x00, // palette id
            0x00, // palette version
            0x01, // entry id
            0xeb, // Y
            0x80, // Cr
            0x80, // Cb
            0xff, // alpha
        ]
    }

    #[test]
    fn object_only_display_sets_are_resource_updates_not_clears() {
        let mut decoder = PgsDecoder::new();
        let mut data = segment(0x15, &ods_payload());
        data.extend_from_slice(&end_segment());

        assert!(decoder.feed(&data).is_empty());
    }

    #[test]
    fn zero_object_normal_pcs_display_sets_emit_clear() {
        let mut decoder = PgsDecoder::new();
        let mut data = segment(0x16, &pcs_payload(0));
        data.extend_from_slice(&end_segment());

        assert_eq!(
            decoder.feed(&data),
            vec![TimedPgsDisplaySet {
                action: PgsDisplaySet::Clear,
                pts: None,
            }]
        );
    }

    #[test]
    fn zero_object_epoch_and_acquisition_pcs_display_sets_do_not_clear() {
        for composition_state in [0x40, 0x80] {
            let mut decoder = PgsDecoder::new();
            let mut data = segment(0x16, &pcs_payload_with_state(composition_state, 0));
            data.extend_from_slice(&end_segment());

            assert!(decoder.feed(&data).is_empty());
        }
    }

    #[test]
    fn composition_display_sets_emit_show() {
        let mut decoder = PgsDecoder::new();
        let mut data = segment(0x14, &pds_payload());
        data.extend_from_slice(&segment(0x15, &ods_payload()));
        data.extend_from_slice(&segment(0x16, &pcs_payload(1)));
        data.extend_from_slice(&end_segment());

        let display_sets = decoder.feed(&data);
        assert_eq!(display_sets.len(), 1);
        match &display_sets[0].action {
            PgsDisplaySet::Show(frames) => {
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].width, 1);
                assert_eq!(frames[0].height, 1);
                assert_eq!(frames[0].x, 10);
                assert_eq!(frames[0].y, 20);
            }
            PgsDisplaySet::Clear => panic!("composition display set unexpectedly cleared"),
        }
    }

    #[test]
    fn pgs_segment_headers_provide_display_set_pts() {
        let pts_90khz = 180_000u32;
        let payload = pcs_payload(0);
        let mut data = Vec::new();
        data.extend_from_slice(b"PG");
        data.extend_from_slice(&pts_90khz.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        data.push(0x16);
        data.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        data.extend_from_slice(&payload);
        data.extend_from_slice(&end_segment());

        assert_eq!(
            PgsDecoder::new().feed(&data),
            vec![TimedPgsDisplaySet {
                action: PgsDisplaySet::Clear,
                pts: Some(Duration::from_secs(2)),
            }]
        );
    }
}
