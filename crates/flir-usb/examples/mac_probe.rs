//! macOS diagnostic: does the kernel let the raw SET_INTERFACE through?
//! After each start/stop request, checks whether the 0x83/0x85 pipes still
//! exist (Timeout = pipe alive, NotFound = macOS switched the alt setting).
//!
//!     FLIR_PAD=1 FLIR_WINDEX_OR=0100 cargo run -p flir-usb --example mac_probe
//!
//! FLIR_PAD=1 sends two zero data bytes with every request; FLIR_WINDEX_OR
//! (hex) is OR-ed into wIndex.

use std::{env, time::Duration, time::Instant};

use rusb::{Context, DeviceHandle, UsbContext};

fn probe(h: &DeviceHandle<Context>) {
    let mut buf = vec![0u8; 16384];
    for ep in [0x83u8, 0x85] {
        let r = h.read_bulk(ep, &mut buf, Duration::from_millis(50));
        println!("    0x{ep:02x}: {:?}", r);
    }
}

fn main() -> anyhow::Result<()> {
    let pad = env::var("FLIR_PAD").is_ok_and(|v| v == "1");
    let mask = env::var("FLIR_WINDEX_OR").ok().and_then(|v| u16::from_str_radix(&v, 16).ok()).unwrap_or(0);
    println!("pad={pad} wIndex|=0x{mask:04x}");
    let ctx = Context::new()?;
    let h = ctx
        .open_device_with_vid_pid(flir_usb::VENDOR_ID, flir_usb::PRODUCT_ID)
        .ok_or_else(|| anyhow::anyhow!("camera not found"))?;
    for i in 0..3 {
        h.claim_interface(i)?;
    }
    println!("claimed; initial pipes:");
    probe(&h);
    for (value, iface, data) in [(0u16, 2u16, false), (0, 1, false), (1, 1, false), (1, 2, true)] {
        let d: &[u8] = if data || pad { &[0, 0] } else { &[] };
        let r = h.write_control(0x01, 0x0b, value, iface | mask, d, Duration::from_millis(500));
        println!("SET_INTERFACE value={value} iface={iface} len={}: {r:?}", d.len());
        probe(&h);
    }
    let mut buf = vec![0u8; 16384];
    let (mut total, mut errs) = (0usize, 0usize);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(5) {
        match h.read_bulk(0x85, &mut buf, Duration::from_millis(200)) {
            Ok(n) => total += n,
            Err(rusb::Error::Timeout) => {}
            Err(e) => {
                errs += 1;
                if errs == 1 {
                    println!("0x85 read: {e:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    println!("RESULT 0x85: {total} bytes in 5 s, {errs} errors");
    Ok(())
}
