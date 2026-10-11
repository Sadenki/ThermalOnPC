//! Frame format of the FLIR One frame endpoint.
//!
//! Each frame starts with a 28-byte little-endian header:
//!
//! | offset | field                          |
//! |--------|--------------------------------|
//! | 0..4   | magic `EF BE 00 00`            |
//! | 4..8   | unknown                        |
//! | 8..12  | payload size (after header)    |
//! | 12..16 | thermal block size             |
//! | 16..20 | visible JPEG size              |
//! | 20..24 | status (JSON text) size        |
//! | 24..28 | unknown                        |
//!
//! The payload is the thermal block, then the JPEG, then the status text.
//! The thermal block holds one row of little-endian `u16` words per image
//! line after a 4-byte preamble; each row is two half-width runs of pixels
//! with a 2-word gap between them. The Gen 2 sends 160x120 (rows of 164
//! words); the Gen 3 sends 80x60, as its `sledInformation` message says.

use thiserror::Error;

pub const MAGIC: [u8; 4] = [0xEF, 0xBE, 0x00, 0x00];
pub const HEADER_LEN: usize = 28;
/// Gen 2 (and FLIR One Pro) sensor size.
pub const THERMAL_WIDTH: usize = 160;
pub const THERMAL_HEIGHT: usize = 120;
/// Gen 3 sensor size.
pub const GEN3_WIDTH: usize = 80;
pub const GEN3_HEIGHT: usize = 60;

const THERMAL_PREAMBLE: usize = 4;
/// Gap between the two half rows, in words.
const HALF_GAP_WORDS: usize = 2;
/// Smallest thermal block that holds a full 160x120 image (Gen 2 layout).
pub const MIN_THERMAL_LEN: usize = THERMAL_PREAMBLE + 164 * 2 * THERMAL_HEIGHT;
/// Guard against garbage sizes when resynchronising on a corrupted stream.
const MAX_PAYLOAD_LEN: usize = 4 * 1024 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("thermal block of {len} bytes too short for {width}x{height}")]
    ThermalTooShort { len: usize, width: usize, height: usize },
    #[error("header sizes inconsistent: payload {payload}, parts {parts}")]
    InconsistentSizes { payload: usize, parts: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub payload_len: usize,
    pub thermal_len: usize,
    pub jpeg_len: usize,
    pub status_len: usize,
}

fn le_u32(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
}

impl Header {
    /// Parses a header from the first 28 bytes of `b`, or returns `None` if
    /// the magic is missing or `b` is too short.
    pub fn parse(b: &[u8]) -> Option<Header> {
        if b.len() < HEADER_LEN || b[..4] != MAGIC {
            return None;
        }
        Some(Header {
            payload_len: le_u32(b, 8),
            thermal_len: le_u32(b, 12),
            jpeg_len: le_u32(b, 16),
            status_len: le_u32(b, 20),
        })
    }
}

/// One decoded frame.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// Raw sensor counts, row-major, `width * height`.
    pub thermal: Vec<u16>,
    /// The undecoded thermal block, kept for analysing new models.
    pub thermal_block: Vec<u8>,
    /// Visible-light camera image, JPEG encoded.
    pub jpeg: Vec<u8>,
    /// Status text sent by the camera (JSON, may be NUL padded).
    pub status: Vec<u8>,
}

impl Frame {
    pub fn status_text(&self) -> &str {
        let end = self.status.iter().position(|&c| c == 0).unwrap_or(self.status.len());
        std::str::from_utf8(&self.status[..end]).unwrap_or("")
    }

    /// True while the shutter is closed for a flat-field correction; the
    /// thermal image then shows the shutter, not the scene.
    pub fn is_ffc(&self) -> bool {
        let s = self.status_text();
        s.contains("\"shutterState\":\"FFC\"") || s.contains("FFC_PROGRESS")
    }

    /// The Lepton's own temperature reading (its "spotmeter") and the mean
    /// raw count of the pixels it covers, from the telemetry the Gen 3
    /// sends after the image rows. `None` when the frame has no telemetry
    /// or it does not look valid.
    pub fn spotmeter(&self) -> Option<Spotmeter> {
        if self.width != GEN3_WIDTH {
            return None;
        }
        // Third telemetry packet (row C): words 50..58 hold the spot mean,
        // max and min in kelvin × 100, then its region (rows, columns).
        let packet = self
            .thermal_block
            .chunks_exact(VOSPI_PACKET)
            .find(|p| (u16::from_le_bytes([p[0], p[1]]) & 0x0fff) as usize == self.height + 2)?;
        let word = |i: usize| u16::from_le_bytes([packet[4 + 2 * i], packet[5 + 2 * i]]) as usize;
        let (kelvin100, max, min) = (word(50), word(51), word(52));
        let (r0, c0, r1, c1) = (word(54), word(55), word(56), word(57));
        let valid = (20_000..42_000).contains(&kelvin100)
            && min <= kelvin100
            && kelvin100 <= max
            && r0 <= r1
            && r1 < self.height
            && c0 <= c1
            && c1 < self.width;
        if !valid {
            return None;
        }
        let mut sum = 0.0;
        for y in r0..=r1 {
            for x in c0..=c1 {
                sum += self.thermal[y * self.width + x] as f64;
            }
        }
        let raw_mean = sum / ((r1 - r0 + 1) * (c1 - c0 + 1)) as f64;
        Some(Spotmeter { celsius: kelvin100 as f64 / 100.0 - 273.15, raw_mean })
    }
}

/// Temperature measured by the Lepton itself over a few pixels, with the
/// mean raw count of those pixels. Used to anchor the raw → °C conversion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spotmeter {
    pub celsius: f64,
    pub raw_mean: f64,
}

/// Picks the sensor size from the thermal block length when the camera
/// did not tell us.
pub fn guess_size(thermal_len: usize) -> (usize, usize) {
    if thermal_len >= MIN_THERMAL_LEN {
        (THERMAL_WIDTH, THERMAL_HEIGHT)
    } else {
        (GEN3_WIDTH, GEN3_HEIGHT)
    }
}

/// Lepton VoSPI packet: 2-byte ID (row number in the low 12 bits), 2-byte
/// CRC, then 80 pixels.
const VOSPI_PACKET: usize = 4 + 2 * GEN3_WIDTH;

/// Decodes a thermal block into `width * height` raw counts.
///
/// The Gen 3 forwards the Lepton's VoSPI packets unchanged: one packet per
/// row, followed by 3 telemetry packets. Older models use a padded layout
/// handled by [`decode_padded`].
pub fn decode_thermal(block: &[u8], width: usize, height: usize) -> Result<Vec<u16>, FrameError> {
    if width == GEN3_WIDTH && block.len() % VOSPI_PACKET == 0 && block.len() / VOSPI_PACKET >= height {
        return Ok(decode_vospi(block, height));
    }
    decode_padded(block, width, height)
}

fn decode_vospi(block: &[u8], height: usize) -> Vec<u16> {
    let mut out = vec![0u16; GEN3_WIDTH * height];
    for packet in block.chunks_exact(VOSPI_PACKET) {
        let row = (u16::from_le_bytes([packet[0], packet[1]]) & 0x0fff) as usize;
        if row >= height {
            continue; // telemetry
        }
        for (x, px) in packet[4..].chunks_exact(2).enumerate() {
            out[row * GEN3_WIDTH + x] = u16::from_le_bytes([px[0], px[1]]);
        }
    }
    out
}

/// Decodes a padded thermal block (Gen 2) into `width * height` raw counts. The row
/// stride is derived from the block length, so padding differences between
/// models do not matter.
fn decode_padded(block: &[u8], width: usize, height: usize) -> Result<Vec<u16>, FrameError> {
    let too_short = FrameError::ThermalTooShort { len: block.len(), width, height };
    let stride = block.len().saturating_sub(THERMAL_PREAMBLE) / (2 * height);
    if stride < width + HALF_GAP_WORDS {
        return Err(too_short);
    }
    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let gap = if x >= width / 2 { HALF_GAP_WORDS } else { 0 };
            let at = THERMAL_PREAMBLE + 2 * (y * stride + x + gap);
            out.push(u16::from_le_bytes([block[at], block[at + 1]]));
        }
    }
    Ok(out)
}

/// Reassembles frames from arbitrarily split chunks of the 0x85 stream.
#[derive(Default)]
pub struct FrameAssembler {
    buf: Vec<u8>,
    /// Sensor size, when known (from `sledInformation`); guessed otherwise.
    pub size: Option<(usize, usize)>,
    /// Bytes dropped while looking for a frame start; useful to spot desync.
    pub skipped_bytes: u64,
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk and returns every frame it completed, in order.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Result<Frame, FrameError>> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            match find_magic(&self.buf) {
                Some(0) => {}
                Some(i) => {
                    self.skipped_bytes += i as u64;
                    self.buf.drain(..i);
                }
                None => {
                    // Keep a possible partial magic at the end.
                    let keep = self.buf.len().min(MAGIC.len() - 1);
                    let drop = self.buf.len() - keep;
                    self.skipped_bytes += drop as u64;
                    self.buf.drain(..drop);
                    break;
                }
            }
            let Some(h) = Header::parse(&self.buf) else { break };
            if h.payload_len > MAX_PAYLOAD_LEN {
                // Not a real header: skip this magic and resync.
                self.skipped_bytes += 1;
                self.buf.drain(..1);
                continue;
            }
            let total = HEADER_LEN + h.payload_len;
            if self.buf.len() < total {
                break;
            }
            frames.push(split_frame(&h, &self.buf[HEADER_LEN..total], self.size));
            self.buf.drain(..total);
        }
        frames
    }
}

fn find_magic(b: &[u8]) -> Option<usize> {
    b.windows(MAGIC.len()).position(|w| w == MAGIC)
}

fn split_frame(h: &Header, payload: &[u8], size: Option<(usize, usize)>) -> Result<Frame, FrameError> {
    let parts = h.thermal_len + h.jpeg_len + h.status_len;
    if parts > payload.len() {
        return Err(FrameError::InconsistentSizes { payload: payload.len(), parts });
    }
    let (thermal, rest) = payload.split_at(h.thermal_len);
    let (jpeg, rest) = rest.split_at(h.jpeg_len);
    let status = &rest[..h.status_len];
    let (width, height) = size.unwrap_or_else(|| guess_size(thermal.len()));
    Ok(Frame {
        width,
        height,
        thermal: decode_thermal(thermal, width, height)?,
        thermal_block: thermal.to_vec(),
        jpeg: jpeg.to_vec(),
        status: status.to_vec(),
    })
}
