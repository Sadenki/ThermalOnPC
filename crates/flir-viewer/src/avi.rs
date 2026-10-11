//! Minimal Motion-JPEG AVI writer: every frame is a JPEG, so the camera's
//! own visible images go in as-is and no video codec is needed. The files
//! play in VLC and the Windows player.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

pub struct AviWriter {
    out: BufWriter<File>,
    /// (offset from the `movi` fourcc, size) of each frame, for the index.
    index: Vec<(u32, u32)>,
    movi_start: u64,
    max_frame: u32,
}

// Byte offsets of the header fields patched in `finish`.
const AVIH_USEC_PER_FRAME: u64 = 32;
const AVIH_TOTAL_FRAMES: u64 = 48;
const STRH_RATE: u64 = 132;
const STRH_LENGTH: u64 = 140;
const HEADER_LEN: u64 = 224;

impl AviWriter {
    pub fn create(path: &Path, width: u32, height: u32) -> std::io::Result<Self> {
        let mut h = Vec::with_capacity(HEADER_LEN as usize);
        h.extend_from_slice(b"RIFF\0\0\0\0AVI LIST");
        h.extend_from_slice(&192u32.to_le_bytes()); // hdrl list size
        h.extend_from_slice(b"hdrlavih");
        h.extend_from_slice(&56u32.to_le_bytes());
        for v in [0u32, 0, 0, 0x10 /* has index */, 0, 0, 1, 0, width, height, 0, 0, 0, 0] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        h.extend_from_slice(b"LIST");
        h.extend_from_slice(&116u32.to_le_bytes()); // strl list size
        h.extend_from_slice(b"strlstrh");
        h.extend_from_slice(&56u32.to_le_bytes());
        h.extend_from_slice(b"vidsMJPG");
        for v in [0u32, 0, 0, 1000 /* scale */, 0 /* rate */, 0, 0 /* length */, 0, u32::MAX, 0] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        for v in [0u16, 0, width as u16, height as u16] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        h.extend_from_slice(b"strf");
        h.extend_from_slice(&40u32.to_le_bytes());
        h.extend_from_slice(&40u32.to_le_bytes());
        h.extend_from_slice(&(width as i32).to_le_bytes());
        h.extend_from_slice(&(height as i32).to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&24u16.to_le_bytes());
        h.extend_from_slice(b"MJPG");
        for v in [width * height * 3, 0, 0, 0, 0] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        h.extend_from_slice(b"LIST\0\0\0\0movi");
        debug_assert_eq!(h.len() as u64, HEADER_LEN);

        let mut out = BufWriter::new(File::create(path)?);
        out.write_all(&h)?;
        Ok(Self { out, index: Vec::new(), movi_start: HEADER_LEN - 4, max_frame: 0 })
    }

    pub fn add_jpeg(&mut self, jpeg: &[u8]) -> std::io::Result<()> {
        let pos = self.out.stream_position()?;
        self.index.push(((pos - self.movi_start) as u32, jpeg.len() as u32));
        self.max_frame = self.max_frame.max(jpeg.len() as u32);
        self.out.write_all(b"00dc")?;
        self.out.write_all(&(jpeg.len() as u32).to_le_bytes())?;
        self.out.write_all(jpeg)?;
        if jpeg.len() % 2 == 1 {
            self.out.write_all(&[0])?;
        }
        Ok(())
    }

    pub fn frames(&self) -> usize {
        self.index.len()
    }

    /// Writes the index and the final sizes. The frame rate is the one
    /// achieved: frames over the recording's `duration`.
    pub fn finish(mut self, duration: Duration) -> std::io::Result<()> {
        let movi_end = self.out.stream_position()?;
        self.out.write_all(b"idx1")?;
        self.out.write_all(&(self.index.len() as u32 * 16).to_le_bytes())?;
        for &(offset, size) in &self.index {
            self.out.write_all(b"00dc")?;
            self.out.write_all(&0x10u32.to_le_bytes())?; // key frame
            self.out.write_all(&offset.to_le_bytes())?;
            self.out.write_all(&size.to_le_bytes())?;
        }
        let end = self.out.stream_position()?;

        let n = self.index.len() as u32;
        let secs = duration.as_secs_f64().max(0.001);
        let fps = if n > 1 { n as f64 / secs } else { 1.0 };
        let patches: [(u64, u32); 6] = [
            (4, (end - 8) as u32),
            (AVIH_USEC_PER_FRAME, (1e6 / fps) as u32),
            (AVIH_TOTAL_FRAMES, n),
            (STRH_RATE, (fps * 1000.0).round().max(1.0) as u32),
            (STRH_LENGTH, n),
            (self.movi_start - 4, (movi_end - self.movi_start) as u32),
        ];
        for (at, v) in patches {
            self.out.seek(SeekFrom::Start(at))?;
            self.out.write_all(&v.to_le_bytes())?;
        }
        // Suggested buffer sizes (avih and strh).
        for at in [60, 144] {
            self.out.seek(SeekFrom::Start(at))?;
            self.out.write_all(&(self.max_frame + 8).to_le_bytes())?;
        }
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_readable_avi() {
        let dir = std::env::var_os("FLIR_TEST_OUT").map_or_else(std::env::temp_dir, Into::into);
        let path = dir.join("test.avi");
        let img = image::RgbImage::from_fn(64, 48, |x, y| image::Rgb([x as u8 * 4, y as u8 * 5, 128]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg).encode_image(&img).unwrap();
        let mut avi = AviWriter::create(&path, 64, 48).unwrap();
        for _ in 0..5 {
            avi.add_jpeg(&jpeg).unwrap();
        }
        avi.finish(Duration::from_secs(1)).unwrap();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize, data.len() - 8);
        assert_eq!(u32::from_le_bytes(data[48..52].try_into().unwrap()), 5);
    }
}
