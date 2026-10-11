//! Captures a few frames and writes them to disk, for debugging the
//! protocol on new hardware:
//!
//!     cargo run -p flir-usb --example dump -- captures 10
//!
//! Also saves the camera's `CameraFiles.zip` (calibration). Writes per frame: `NNN-thermal.raw` (decoded little-endian u16),
//! `NNN-thermal-block.bin` (undecoded), `NNN-visible.jpg`, `NNN-status.txt`.

use std::{env, fs, path::PathBuf};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut args = env::args().skip(1);
    let dir = PathBuf::from(args.next().unwrap_or_else(|| "captures".into()));
    let count: usize = args.next().map(|s| s.parse()).transpose()?.unwrap_or(10);
    fs::create_dir_all(&dir)?;

    let camera = flir_usb::Camera::open()?;
    // Per-camera calibration; not fatal if it fails, frames still matter more.
    match camera.read_file("CameraFiles.zip") {
        Ok(zip) => {
            fs::write(dir.join("CameraFiles.zip"), &zip)?;
            println!("CameraFiles.zip: {} B", zip.len());
        }
        Err(e) => eprintln!("CameraFiles.zip not read: {e}"),
    }
    let mut stream = camera.start()?;
    for i in 0..count {
        let f = stream.next_frame()?;
        let (min, max) = f.thermal.iter().fold((u16::MAX, 0), |(a, b), &v| (a.min(v), b.max(v)));
        println!("frame {i}: {}x{}, raw {min}..{max}, jpeg {} B, status {:?}", f.width, f.height, f.jpeg.len(), f.status_text());
        let raw: Vec<u8> = f.thermal.iter().flat_map(|v| v.to_le_bytes()).collect();
        fs::write(dir.join(format!("{i:03}-thermal.raw")), raw)?;
        fs::write(dir.join(format!("{i:03}-thermal-block.bin")), &f.thermal_block)?;
        fs::write(dir.join(format!("{i:03}-visible.jpg")), &f.jpeg)?;
        fs::write(dir.join(format!("{i:03}-status.txt")), f.status_text())?;
    }
    Ok(())
}
