//! Linux diagnostic: runs the start sequence one step at a time and checks
//! after each step (and during a pause) whether the camera is still on the
//! bus, to find the request after which it resets.
//!
//!     cargo run --release -p flir-usb --example linux_probe
//!
//! FLIR_PAUSE=<ms> sets the pause after each step (default 2000).
//! FLIR_NO_CLAIM=1 skips claiming interfaces 1 and 2 (only 0 is claimed).

use std::{env, time::Duration, time::Instant};

use rusb::{Context, DeviceHandle, UsbContext};

type H = DeviceHandle<Context>;

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// GET_STATUS on the device: answers as long as the camera is connected.
fn alive(h: &H) -> Result<(), rusb::Error> {
    let mut b = [0u8; 2];
    h.read_control(0x80, 0x00, 0, 0, &mut b, Duration::from_millis(300)).map(|_| ())
}

/// Waits `pause`, reading 0x81 (status messages) and checking the camera.
fn watch(h: &H, step: &str, pause: Duration) -> bool {
    let t = Instant::now();
    let mut buf = vec![0u8; 16384];
    while t.elapsed() < pause {
        match h.read_bulk(0x81, &mut buf, Duration::from_millis(200)) {
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                let kinds: Vec<&str> = text.split("\"type\":\"").skip(1).filter_map(|s| s.split('"').next()).collect();
                println!("    {:>5} ms  0x81 <- {n} B {kinds:?}", t.elapsed().as_millis());
            }
            Err(rusb::Error::Timeout) => {}
            Err(e) => println!("    {:>5} ms  0x81: {e:?}", t.elapsed().as_millis()),
        }
        if let Err(e) = alive(h) {
            println!("!!! camera gone {} ms after « {step} » ({e:?})", t.elapsed().as_millis());
            return false;
        }
    }
    println!("    ok, still connected {} ms after « {step} »", pause.as_millis());
    true
}

fn command(h: &H, json: &str) -> Result<(), rusb::Error> {
    let mut body = json.as_bytes().to_vec();
    body.push(0);
    let mut header = Vec::new();
    for w in [0x1ccu32, 1, body.len() as u32] {
        header.extend_from_slice(&w.to_le_bytes());
    }
    header.extend_from_slice(&crc32(&header).to_le_bytes());
    h.write_bulk(0x02, &header, Duration::from_secs(1))?;
    h.write_bulk(0x02, &body, Duration::from_secs(1))?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let pause = Duration::from_millis(env::var("FLIR_PAUSE").ok().and_then(|v| v.parse().ok()).unwrap_or(2000));
    let no_claim = env::var("FLIR_NO_CLAIM").is_ok_and(|v| v == "1");
    println!("libusb {:?}", rusb::version());
    let ctx = Context::new()?;
    let h = ctx
        .open_device_with_vid_pid(flir_usb::VENDOR_ID, flir_usb::PRODUCT_ID)
        .ok_or_else(|| anyhow::anyhow!("camera not found"))?;
    let dev = h.device();
    println!("opened: bus {} address {} speed {:?}", dev.bus_number(), dev.address(), dev.speed());
    println!("active configuration: {:?}", h.active_configuration());
    if !watch(&h, "ouverture", pause) {
        return Ok(());
    }
    for i in 0..3u8 {
        if no_claim && i > 0 {
            continue;
        }
        println!("claim interface {i}: {:?}", h.claim_interface(i));
        if !watch(&h, &format!("claim {i}"), pause) {
            return Ok(());
        }
    }
    let steps: [(&str, u16, u16, &[u8]); 3] = [("iface 2 alt 0", 0, 2, &[]), ("iface 1 alt 0", 0, 1, &[]), ("iface 1 alt 1", 1, 1, &[])];
    for (name, value, iface, data) in steps {
        let r = h.write_control(0x01, 0x0b, value, iface, data, Duration::from_millis(500));
        println!("SET_INTERFACE {name}: {r:?}");
        if !watch(&h, name, pause) {
            return Ok(());
        }
    }
    let r = command(&h, r#"{"type":"openFile","data":{"mode":"r","path":"CameraFiles.zip"}}"#);
    println!("openFile: {r:?}");
    if !watch(&h, "openFile", pause) {
        return Ok(());
    }
    let r = h.write_control(0x01, 0x0b, 1, 2, &[0, 0], Duration::from_millis(500));
    println!("SET_INTERFACE iface 2 alt 1 (+2 octets): {r:?}");
    if !watch(&h, "iface 2 alt 1", pause) {
        return Ok(());
    }
    let mut buf = vec![0u8; 16384];
    let (mut total, t) = (0usize, Instant::now());
    while t.elapsed() < Duration::from_secs(5) {
        match h.read_bulk(0x85, &mut buf, Duration::from_millis(200)) {
            Ok(n) => total += n,
            Err(rusb::Error::Timeout) => {}
            Err(e) => {
                println!("0x85 after {} ms: {e:?}", t.elapsed().as_millis());
                break;
            }
        }
    }
    println!("RESULT 0x85: {total} bytes; camera {}", if alive(&h).is_ok() { "still connected" } else { "gone" });
    Ok(())
}
