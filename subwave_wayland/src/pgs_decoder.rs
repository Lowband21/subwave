//! Streaming PGS (Presentation Graphic Stream) subtitle decoder.
//!
//! Decodes `subpicture/x-pgs` buffers received from GStreamer into ARGB
//! bitmaps suitable for the Wayland subtitle subsurface.
//!
//! PGS segments arrive individually as GStreamer buffers.  Each buffer
//! contains one segment (no 2-byte PG sync header — GStreamer strips it).
//! A complete subtitle frame ("display set") is assembled from multiple
//! segments: PCS → WDS → PDS → ODS → END.

/// A fully decoded PGS subtitle image ready for display.
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

/// Accumulates PGS segments and produces decoded frames.
pub struct PgsDecoder {
    palette: Vec<[u8; 4]>, // 256 ARGB entries
    objects: Vec<OdsData>,
    compositions: Vec<CompositionObject>,
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

impl PgsDecoder {
    pub fn new() -> Self {
        Self {
            palette: vec![[0, 0, 0, 0]; 256],
            objects: Vec::new(),
            compositions: Vec::new(),
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
    /// decoded frames when an END segment is encountered.
    /// An empty `Vec` means "clear the subtitle" (composition with no objects).
    pub fn feed(&mut self, data: &[u8]) -> Option<Vec<PgsFrame>> {
        let mut result: Option<Vec<PgsFrame>> = None;
        let mut offset = 0usize;

        while offset < data.len() {
            // Try to parse a segment at the current offset.
            // PGS segments: [type:1][size:2][payload:size]
            // Some muxers include the 2-byte "PG" sync (0x50 0x47) before
            // each segment — skip it if present.
            if offset + 2 <= data.len()
                && data[offset] == 0x50
                && data[offset + 1] == 0x47
            {
                // Skip "PG" sync marker
                offset += 2;
                // After sync: [PTS:4][DTS:4][type:1][size:2][payload...]
                if offset + 11 > data.len() {
                    break;
                }
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
                    seg_type, seg_size, data.len() - payload_start
                );
                break;
            }

            let payload = &data[payload_start..payload_end];

            match seg_type {
                SEG_PCS => {
                    log::debug!("[pgs] PCS segment ({} bytes)", payload.len());
                    self.parse_pcs(payload);
                }
                SEG_WDS => {
                    log::debug!("[pgs] WDS segment ({} bytes)", payload.len());
                }
                SEG_PDS => {
                    log::debug!("[pgs] PDS segment ({} bytes, {} entries)", payload.len(), (payload.len().saturating_sub(2)) / 5);
                    self.parse_pds(payload);
                }
                SEG_ODS => {
                    log::debug!("[pgs] ODS segment ({} bytes)", payload.len());
                    self.parse_ods(payload);
                }
                SEG_END => {
                    log::info!(
                        "[pgs] END — display set complete ({} objects, {} compositions)",
                        self.objects.len(),
                        self.compositions.len()
                    );
                    result = Some(self.finish_display_set());
                }
                _ => {
                    log::debug!("[pgs] Unknown segment type: 0x{:02x} at offset {}", seg_type, offset);
                    // Can't determine size — bail out of this buffer
                    break;
                }
            }

            offset = payload_end;
        }

        result
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
            self.compositions.push(CompositionObject { object_id, x, y });
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
            let g = (y_val - 0.344136 * (cb - 128.0) - 0.714136 * (cr - 128.0))
                .clamp(0.0, 255.0) as u8;
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
            let rle_data = &data[4..]; // skip object_id(2) + version(1) + seq_flag(1)
            // Actually, continuation starts at offset 7 (after 3-byte data_length)
            let rle_data = if data.len() > 7 { &data[7..] } else { &data[4..] };
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

    fn finish_display_set(&mut self) -> Vec<PgsFrame> {
        if self.compositions.is_empty() {
            // No composition objects = clear subtitle
            self.objects.clear();
            return Vec::new();
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
        self.compositions.clear();
        frames
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
