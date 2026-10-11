//! Same as `dump`, but through libusb and a byte-for-byte copy of the
//! reference driver's start sequence. On Windows, WinUSB filters the raw
//! SET_INTERFACE request the camera needs; the libusb-win32 (libusb0.sys)
//! driver installed with Zadig passes it through.
//!
//!     cargo run -p flir-usb --example dump_libusb -- captures 10 [ep]
//!
//! `ep` (optional, 2 or 4) sends the reference driver's file-request
//! handshake (open/read `CameraFiles.zip`) on that OUT endpoint before
//! starting the frame stream.

use std::io::Write;
use std::{env, fs, path::PathBuf, time::Duration, time::Instant};

use flir_proto::FrameAssembler;
use rusb::{Context, DeviceHandle, UsbContext};

const T: Duration = Duration::from_millis(100);

/// bmRequestType for the start/stop requests. The camera wants a standard
/// SET_INTERFACE (0x01), but Windows drivers turn that into a real
/// alternate-setting switch, and alt 1 has no endpoints. Other request
/// types are passed through untouched; whether the camera honours them is
/// what FLIR_REQTYPE lets us test (e.g. 00, 21, 41).
fn request_type() -> u8 {
    env::var("FLIR_REQTYPE").ok().and_then(|v| u8::from_str_radix(v.trim_start_matches("0x"), 16).ok()).unwrap_or(0x01)
}

fn ctrl(h: &DeviceHandle<Context>, value: u16, index: u16, data: &[u8]) -> rusb::Result<usize> {
    let rt = request_type();
    let r = h.write_control(rt, 0x0b, value, index, data, Duration::from_millis(200));
    println!("SET_INTERFACE (type 0x{rt:02x}) value={value} interface={index}: {r:?}");
    r
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut args = env::args().skip(1);
    let dir = PathBuf::from(args.next().unwrap_or_else(|| "captures".into()));
    let count: usize = args.next().map(|s| s.parse()).transpose()?.unwrap_or(10);
    let handshake_ep: Option<u8> = args.next().map(|s| s.parse()).transpose()?;
    fs::create_dir_all(&dir)?;

    println!("libusb {:?}", rusb::version());
    // FLIR_USBDK=1 uses the UsbDk backend (Windows, UsbDk installed), which
    // sends control requests to the camera as-is instead of letting the
    // driver turn SET_INTERFACE into an alternate-setting switch.
    let ctx = if env::var("FLIR_USBDK").is_ok_and(|v| v == "1") {
        println!("using UsbDk backend");
        usbdk_context()?
    } else {
        Context::new()?
    };
    let h = ctx
        .open_device_with_vid_pid(flir_usb::VENDOR_ID, flir_usb::PRODUCT_ID)
        .ok_or_else(|| anyhow::anyhow!("camera not found, or no libusb-compatible driver bound"))?;
    let _ = h.set_auto_detach_kernel_driver(true);
    for i in 0..3 {
        h.claim_interface(i)?;
    }
    println!("claimed interfaces 0, 1, 2");

    ctrl(&h, 0, 2, &[])?;
    ctrl(&h, 0, 1, &[])?;
    ctrl(&h, 1, 1, &[])?;
    if let Some(ep) = handshake_ep {
        handshake(&h, ep);
    }
    ctrl(&h, 1, 2, &[0, 0])?;

    // On Windows (libusb-win32) the SET_INTERFACE requests above become real
    // alternate-setting switches: the camera starts, but alt 1 has no
    // endpoints so Windows drops the 0x83/0x85 pipes. Switching back to
    // alt 0 recreates them; the open question is whether the camera keeps
    // streaming. FLIR_REARM lists the interfaces to switch back ("" = none).
    let usbdk = env::var("FLIR_USBDK").is_ok_and(|v| v == "1");
    let rearm =
        env::var("FLIR_REARM").unwrap_or_else(|_| if cfg!(windows) && !usbdk { "2".into() } else { String::new() });
    for i in rearm.split(',').filter_map(|v| v.trim().parse::<u8>().ok()) {
        let r = h.set_alternate_setting(i, 0);
        println!("re-arm interface {i} (alt 0): {r:?}");
    }

    // On the Gen 2 frames come on 0x85; a Gen 3 has been seen streaming on
    // 0x83 instead, so both feed a frame assembler.
    let mut asms = [(0x85u8, FrameAssembler::new(), 0usize, 0usize), (0x83u8, FrameAssembler::new(), 0usize, 0usize)];
    // Small reads: usbip (WSL) returned zero-filled 1 MiB buffers.
    let chunk: usize = env::var("FLIR_CHUNK").ok().and_then(|v| v.parse().ok()).unwrap_or(16384);
    println!("read size {chunk} B");
    let mut buf = vec![0u8; chunk];
    let mut saved = 0;
    let mut status_log = fs::File::create(dir.join("status.log"))?;
    let started = Instant::now();
    let mut last_report = Instant::now();
    while saved < count {
        if last_report.elapsed() > Duration::from_secs(3) {
            let stats: Vec<String> = asms
                .iter()
                .map(|(ep, a, total, zeros)| format!("0x{ep:02x}: {total} B, {} B skipped, {zeros} all-zero reads", a.skipped_bytes))
                .collect();
            println!("... {}s, {}", started.elapsed().as_secs(), stats.join(", "));
            last_report = Instant::now();
        }
        if started.elapsed() > Duration::from_secs(30) && saved == 0 {
            anyhow::bail!("no frame after 30 s");
        }
        for (ep, asm, total, zero_reads) in asms.iter_mut() {
            let ep = *ep;
            let n = match h.read_bulk(ep, &mut buf, T) {
                Ok(n) => n,
                Err(rusb::Error::Timeout) => continue,
                Err(rusb::Error::Pipe) => {
                    let r = h.clear_halt(ep);
                    println!("0x{ep:02x} stalled, clear_halt: {r:?}");
                    continue;
                }
                Err(e) => {
                    println!("0x{ep:02x}: {e}");
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
            };
            if buf[..n].iter().all(|&b| b == 0) {
                *zero_reads += 1;
            } else if *zero_reads > 0 && *total - *zero_reads * chunk == 0 {
                let hex: Vec<String> = buf[..n.min(48)].iter().map(|b| format!("{b:02x}")).collect();
                println!("0x{ep:02x} first non-zero chunk ({n} B) after {zero_reads} zero reads: {}", hex.join(" "));
                fs::write(dir.join(format!("nonzero-0x{ep:02x}.bin")), &buf[..n])?;
            }
            if *total == 0 {
                let hex: Vec<String> = buf[..n.min(48)].iter().map(|b| format!("{b:02x}")).collect();
                println!("0x{ep:02x} first chunk {n} B: {}", hex.join(" "));
            }
            *total += n;
            for f in asm.push(&buf[..n]) {
                let f = match f {
                    Ok(f) => f,
                    Err(e) => {
                        println!("0x{ep:02x} bad frame: {e}");
                        continue;
                    }
                };
                let (min, max) = f.thermal.iter().fold((u16::MAX, 0), |(a, b), &v| (a.min(v), b.max(v)));
                println!(
                    "frame {saved} (0x{ep:02x}): raw {min}..{max}, jpeg {} B, status {:?}",
                    f.jpeg.len(),
                    f.status_text()
                );
                let raw: Vec<u8> = f.thermal.iter().flat_map(|v| v.to_le_bytes()).collect();
                fs::write(dir.join(format!("{saved:03}-thermal.raw")), raw)?;
                fs::write(dir.join(format!("{saved:03}-visible.jpg")), &f.jpeg)?;
                fs::write(dir.join(format!("{saved:03}-thermal-block.bin")), &f.thermal_block)?;
                fs::write(dir.join(format!("{saved:03}-status.txt")), f.status_text())?;
                saved += 1;
            }
            // Keep one raw chunk per endpoint for offline analysis.
            if *total == n {
                fs::write(dir.join(format!("chunk-0x{ep:02x}.bin")), &buf[..n])?;
            }
        }
        match h.read_bulk(0x81, &mut buf, Duration::from_millis(10)) {
            Ok(n) if n > 0 => {
                // Status messages: 16-byte binary header, then JSON. Log
                // them in full, both on screen and in status.log.
                let text: String =
                    buf[..n].iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
                println!("0x81 <- {n} B: {text}");
                status_log.write_all(&buf[..n])?;
                status_log.write_all(b"\n")?;
            }
            Ok(_) | Err(rusb::Error::Timeout) => {}
            Err(rusb::Error::Pipe) => {
                let r = h.clear_halt(0x81);
                println!("0x81 stalled, clear_halt: {r:?}");
            }
            Err(e) => println!("0x81: {e}"),
        }
    }
    Ok(())
}

/// The file-request messages the reference driver sends: a 16-byte header
/// (magic `cc 01`, length of the JSON including its NUL) then the JSON.
fn handshake(h: &DeviceHandle<Context>, ep: u8) {
    let msgs: [(&[u8; 16], &str); 2] = [
        (
            &[0xcc, 0x01, 0, 0, 0x01, 0, 0, 0, 0x41, 0, 0, 0, 0xf8, 0xb3, 0xf7, 0x00],
            r#"{"type":"openFile","data":{"mode":"r","path":"CameraFiles.zip"}}"#,
        ),
        (
            &[0xcc, 0x01, 0, 0, 0x01, 0, 0, 0, 0x33, 0, 0, 0, 0xef, 0xdb, 0xc1, 0xc1],
            r#"{"type":"readFile","data":{"streamIdentifier":10}}"#,
        ),
    ];
    let mut buf = vec![0u8; 1 << 16];
    for (header, json) in msgs {
        let mut body = json.as_bytes().to_vec();
        body.push(0);
        let r1 = h.write_bulk(ep, header, Duration::from_millis(500));
        let r2 = h.write_bulk(ep, &body, Duration::from_millis(500));
        println!("0x{ep:02x} -> {json}: header {r1:?}, body {r2:?}");
        for _ in 0..5 {
            for rep in [0x81, 0x83] {
                match h.read_bulk(rep, &mut buf, Duration::from_millis(100)) {
                    Ok(n) => {
                        let text: String = buf[..n.min(200)]
                            .iter()
                            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                            .collect();
                        println!("0x{rep:02x} <- {n} B: {text}");
                    }
                    Err(rusb::Error::Timeout) => {}
                    Err(rusb::Error::Pipe) => {
                        let r = h.clear_halt(rep);
                        println!("0x{rep:02x} stalled, clear_halt: {r:?}");
                    }
                    Err(e) => println!("0x{rep:02x}: {e}"),
                }
            }
        }
    }
}

#[cfg(windows)]
fn usbdk_context() -> rusb::Result<Context> {
    Context::with_options(&[rusb::UsbOption::use_usbdk()])
}

#[cfg(not(windows))]
fn usbdk_context() -> rusb::Result<Context> {
    Context::new()
}
