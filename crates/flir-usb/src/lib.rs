//! USB transport for the FLIR One Gen 3 (USB-C / Android model).
//!
//! The camera exposes three vendor interfaces:
//!
//! | interface | endpoints            | role                          |
//! |-----------|----------------------|-------------------------------|
//! | 0         | 0x81 in, 0x02 out    | config / status (JSON)        |
//! | 1         | 0x83 in, 0x04 out    | file I/O (calibration files)  |
//! | 2         | 0x85 in, 0x06 out    | frame stream                  |
//!
//! Streams are started with standard SET_INTERFACE requests (value 1 = on,
//! 0 = off). The camera only serves 0x85 while interface 2 is "on", yet its
//! descriptors place 0x85 in alternate setting 0 and give setting 1 no
//! endpoints. A host must therefore send SET_INTERFACE as a raw request
//! without switching its own view of the interface:
//!
//! * Linux (usbfs) passes the raw request through.
//! * macOS does not: IOUSBHost performs a real alternate-setting switch, so
//!   the 0x83/0x85 pipes vanish ("pipe ref not recognized"), and switching
//!   back to alt 0 stops the camera's stream. Streaming on macOS is
//!   therefore not possible through libusb (see `examples/mac_probe.rs`).
//! * On Windows, WinUSB swallows it and libusb-win32 turns it into a real
//!   switch that drops the 0x85 pipe. Only the UsbDk driver passes it
//!   through, so on Windows UsbDk must be installed.
//!
//! libusb is vendored with a fix for this camera's out-of-order alternate
//! settings (see `vendor/libusb1-sys`).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flir_proto::{Calibration, Frame, FrameAssembler, FrameError};
use log::{debug, info, warn};
use rusb::{Context, DeviceHandle, UsbContext};
use thiserror::Error;

pub const VENDOR_ID: u16 = 0x09cb;
pub const PRODUCT_ID: u16 = 0x1996;

const EP_STATUS: u8 = 0x81;
const EP_CONFIG_OUT: u8 = 0x02;
const EP_FILE: u8 = 0x83;
const EP_FRAME: u8 = 0x85;
const CONFIG_MAGIC: u32 = 0x1cc;
const FILE_MAGIC: u32 = 0x5510;
const FILE_HEADER_LEN: usize = 24;
const READ_CHUNK: usize = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_millis(500);
const GIVE_UP_AFTER: Duration = Duration::from_secs(20);

#[derive(Debug, Error)]
pub enum Error {
    #[error("no FLIR One camera found (USB {VENDOR_ID:04x}:{PRODUCT_ID:04x})")]
    NotFound,
    #[error("USB error: {0}")]
    Usb(#[from] rusb::Error),
    #[error("no data from the camera")]
    NoData,
    #[error("bad frame: {0}")]
    Frame(#[from] FrameError),
    #[error("calibration not found in CameraFiles.zip: {0}")]
    Calibration(String),
    #[error("camera refused to open {path}: {response}")]
    OpenFile { path: String, response: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Battery state, from the camera's status messages.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Battery {
    pub percent: Option<u32>,
    pub voltage: Option<f32>,
    /// e.g. `stateNoCharging`, `stateChargingSmartPhone`, `stateManagedCharging`.
    pub charging_state: Option<String>,
    /// mA; positive while a charger feeds the camera.
    pub charging_current: Option<u32>,
}

impl Battery {
    /// A charger is connected and feeding the camera.
    pub fn is_charging(&self) -> bool {
        let state = self.charging_state.as_deref().unwrap_or("");
        let by_state = state.contains("Charging") && !state.contains("NoCharging") && !state.contains("Fault");
        by_state || self.charging_current.is_some_and(|c| c > 0)
    }

    pub fn is_fault(&self) -> bool {
        self.charging_state.as_deref().is_some_and(|s| s.contains("Fault"))
    }

    /// Updates from one status JSON message; returns true if it was a battery one.
    pub fn update(&mut self, json: &str) -> bool {
        if json.contains("\"batteryVoltageUpdate\"") {
            self.percent = json_number(json, "percentage").or(self.percent);
            self.voltage = json_float(json, "voltage").or(self.voltage);
        } else if json.contains("\"batteryChargingStateUpdate\"") {
            self.charging_state = json_string(json, "chargingState").or(self.charging_state.take());
        } else if json.contains("\"batteryChargingCurrentUpdate\"") {
            self.charging_current = json_number(json, "chargingCurrent").or(self.charging_current);
        } else {
            return false;
        }
        true
    }
}

type Handle = Arc<DeviceHandle<Context>>;

/// An opened camera with its interfaces claimed, not yet streaming.
pub struct Camera {
    handle: Handle,
}

/// libusb context: UsbDk on Windows (the only driver that passes the raw
/// SET_INTERFACE through), the default backend elsewhere.
fn context() -> Result<Context> {
    #[cfg(windows)]
    {
        match Context::with_options(&[rusb::UsbOption::use_usbdk()]) {
            Ok(ctx) => return Ok(ctx),
            Err(e) => warn!("UsbDk unavailable ({e}); is it installed? Falling back to WinUSB, which cannot stream"),
        }
    }
    Ok(Context::new()?)
}

impl Camera {
    /// Opens the first FLIR One found on the bus.
    pub fn open() -> Result<Self> {
        let ctx = context()?;
        let devices = ctx.devices()?;
        debug!("{} USB devices on the bus", devices.len());
        let device = devices
            .iter()
            .find(|d| d.device_descriptor().is_ok_and(|dd| dd.vendor_id() == VENDOR_ID && dd.product_id() == PRODUCT_ID))
            .ok_or(Error::NotFound)?;
        // Open explicitly so a driver or access problem is reported as such
        // instead of looking like a missing camera.
        let handle = device.open()?;
        let _ = handle.set_auto_detach_kernel_driver(true);
        for i in 0..3 {
            handle.claim_interface(i)?;
        }
        info!("camera opened");
        Ok(Self { handle: Arc::new(handle) })
    }

    /// Reads a whole file from the camera, e.g. `CameraFiles.zip` (which
    /// holds the per-camera calibration). Must be called before `start`,
    /// which takes over the status endpoint.
    ///
    /// Commands go to the config endpoint as JSON (`openFile`, then
    /// `readFile` with the stream id from `openFileStatus`); file data comes
    /// back on 0x83 as messages with a 6-word header (magic 0x5510, 1, size,
    /// stream id, CRC-32 of the data, header CRC). `readFile` is repeated
    /// until the camera sends an empty chunk or stops answering.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let h = &self.handle;
        set_stream(h, 2, false, &[])?;
        set_stream(h, 1, false, &[])?;
        set_stream(h, 1, true, &[])?;
        settle();

        send_command(h, &format!(r#"{{"type":"openFile","data":{{"mode":"r","path":"{path}"}}}}"#))?;
        let response = wait_status(h, "openFileStatus")?;
        let id = json_number(&response, "streamIdentifier")
            .or_else(|| json_number(&response, "value"))
            .ok_or_else(|| Error::OpenFile { path: path.into(), response: response.clone() })?;
        info!("{path} opened as stream {id}");

        let mut data = Vec::new();
        let mut pending = Vec::new();
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            send_command(h, &format!(r#"{{"type":"readFile","data":{{"streamIdentifier":{id}}}}}"#))?;
            let chunk = read_file_message(h, &mut pending, &mut buf)?;
            match chunk {
                Some(c) if !c.is_empty() => {
                    debug!("{path}: +{} bytes", c.len());
                    data.extend_from_slice(&c);
                }
                _ => break,
            }
        }
        info!("{path}: {} bytes", data.len());
        let _ = send_command(h, &format!(r#"{{"type":"closeFile","data":{{"streamIdentifier":{id}}}}}"#));
        Ok(data)
    }

    /// Reads this camera's own radiometric constants from the `calib.rsc`
    /// inside its `CameraFiles.zip`. Must be called before `start`.
    pub fn read_calibration(&self) -> Result<Calibration> {
        let zip = self.read_file("CameraFiles.zip")?;
        calibration_from_zip(&zip)
    }

    /// Runs the start sequence and returns a stream of frames.
    pub fn start(self) -> Result<FrameStream> {
        let h = &self.handle;
        set_stream(h, 2, false, &[])?;
        set_stream(h, 1, false, &[])?;
        set_stream(h, 1, true, &[])?;
        settle();
        // The reference drivers send two (zero) data bytes with this one.
        set_stream(h, 2, true, &[0, 0])?;

        let size = Arc::new(AtomicU32::new(0));
        let battery = Arc::new(Mutex::new(Battery::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let reader = spawn_status_reader(self.handle.clone(), size.clone(), battery.clone(), stop.clone());
        Ok(FrameStream {
            handle: self.handle,
            asm: FrameAssembler::new(),
            size,
            battery,
            stop,
            reader: Some(reader),
            closed: false,
            buf: vec![0; READ_CHUNK],
            ready: Vec::new(),
        })
    }
}

/// After "stream on" for the file interface the camera wakes up (it then
/// sends `sledInformation` and battery messages). Commands sent during that
/// moment make it reset under Linux (seen on two PCs, 11/10/2026); a 2 s
/// pause per step was enough (`examples/linux_probe.rs`), 1 s is used.
fn settle() {
    std::thread::sleep(Duration::from_millis(1000));
}

/// SET_INTERFACE (bmRequestType 0x01, bRequest 0x0B), sent raw.
fn set_stream(h: &DeviceHandle<Context>, iface: u16, on: bool, data: &[u8]) -> Result<()> {
    debug!("interface {iface}: stream {}", if on { "on" } else { "off" });
    h.write_control(0x01, 0x0b, on as u16, iface, data, Duration::from_millis(500))?;
    Ok(())
}

/// Finds `calib.rsc` in a `CameraFiles.zip` and parses it.
pub fn calibration_from_zip(zip: &[u8]) -> Result<Calibration> {
    use std::io::Read;
    let bad = |e: &dyn std::fmt::Display| Error::Calibration(e.to_string());
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).map_err(|e| bad(&e))?;
    let name = archive
        .file_names()
        .find(|n| n.ends_with("calib.rsc"))
        .map(str::to_owned)
        .ok_or_else(|| bad(&"no calib.rsc"))?;
    let mut text = String::new();
    archive.by_name(&name).map_err(|e| bad(&e))?.read_to_string(&mut text).map_err(|e| bad(&e))?;
    let cal = Calibration::from_rsc(&text).ok_or_else(|| bad(&"no extRBF constants for the active case"))?;
    info!("calibration: {cal:?}");
    Ok(cal)
}

/// CRC-32 (IEEE, as zlib) used in every message header.
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

/// Sends a JSON command (NUL-terminated) with its 16-byte config header.
fn send_command(h: &DeviceHandle<Context>, json: &str) -> Result<()> {
    debug!("command: {json}");
    let mut body = json.as_bytes().to_vec();
    body.push(0);
    let mut header = Vec::with_capacity(16);
    for w in [CONFIG_MAGIC, 1, body.len() as u32] {
        header.extend_from_slice(&w.to_le_bytes());
    }
    header.extend_from_slice(&crc32(&header).to_le_bytes());
    let t = Duration::from_millis(1000);
    h.write_bulk(EP_CONFIG_OUT, &header, t)?;
    h.write_bulk(EP_CONFIG_OUT, &body, t)?;
    Ok(())
}

/// Reads status messages until one of type `kind` arrives (5 s max).
fn wait_status(h: &DeviceHandle<Context>, kind: &str) -> Result<String> {
    let mut buf = vec![0u8; 16 * 1024];
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match h.read_bulk(EP_STATUS, &mut buf, Duration::from_millis(500)) {
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                for json in text.split('\0').filter_map(|s| s.find('{').map(|i| &s[i..])) {
                    debug!("status: {json}");
                    if json.contains(&format!("\"{kind}\"")) {
                        return Ok(json.to_string());
                    }
                }
            }
            Err(rusb::Error::Timeout) => {}
            Err(rusb::Error::Pipe) => {
                let _ = h.clear_halt(EP_STATUS);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::NoData)
}

/// Reads one file message from 0x83. Returns `None` if the camera sends
/// nothing for 3 s.
fn read_file_message(h: &DeviceHandle<Context>, pending: &mut Vec<u8>, buf: &mut [u8]) -> Result<Option<Vec<u8>>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        // Resync on the magic, then wait for a whole message.
        let magic = FILE_MAGIC.to_le_bytes();
        if let Some(start) = pending.windows(4).position(|w| w == magic) {
            pending.drain(..start);
        } else if pending.len() > 3 {
            let keep = pending.len() - 3;
            pending.drain(..keep);
        }
        if pending.len() >= FILE_HEADER_LEN {
            let word = |i: usize| u32::from_le_bytes(pending[i * 4..i * 4 + 4].try_into().unwrap());
            let size = word(2) as usize;
            if crc32(&pending[..20]) != word(5) {
                warn!("bad file header CRC, resyncing");
                pending.drain(..4);
                continue;
            }
            if pending.len() >= FILE_HEADER_LEN + size {
                let data_crc = word(4);
                let data: Vec<u8> = pending[FILE_HEADER_LEN..FILE_HEADER_LEN + size].to_vec();
                pending.drain(..FILE_HEADER_LEN + size);
                if size > 0 && crc32(&data) != data_crc {
                    warn!("file chunk CRC mismatch ({size} bytes)");
                }
                return Ok(Some(data));
            }
        }
        if std::time::Instant::now() >= deadline {
            if !pending.is_empty() {
                warn!("{} bytes of incomplete file data dropped", pending.len());
            }
            return Ok(None);
        }
        match h.read_bulk(EP_FILE, buf, Duration::from_millis(500)) {
            Ok(n) => pending.extend_from_slice(&buf[..n]),
            Err(rusb::Error::Timeout) => {}
            Err(rusb::Error::Pipe) => {
                let _ = h.clear_halt(EP_FILE);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Reads status messages from 0x81 in the background. They are a 16-byte
/// header (magic 0x1cc, 1, length, CRC-32) then JSON; the `sledInformation`
/// message gives the thermal sensor size, which is stored in `size` as
/// `width << 16 | height`.
fn spawn_status_reader(
    h: Handle,
    size: Arc<AtomicU32>,
    battery: Arc<Mutex<Battery>>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 16 * 1024];
        while !stop.load(Ordering::Relaxed) {
            match h.read_bulk(EP_STATUS, &mut buf, Duration::from_secs(1)) {
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    for json in text.split('\0').filter_map(|s| s.find('{').map(|i| &s[i..])) {
                        debug!("status: {json}");
                        if let Ok(mut b) = battery.lock() {
                            b.update(json);
                        }
                        if json.contains("\"sledInformation\"") {
                            if let (Some(w), Some(hh)) = (json_number(json, "thermalWidth"), json_number(json, "thermalHeight")) {
                                info!("sensor {w}x{hh}");
                                size.store(w << 16 | hh, Ordering::Relaxed);
                            }
                        }
                    }
                }
                Err(rusb::Error::Timeout) => {}
                Err(rusb::Error::Pipe) => {
                    let _ = h.clear_halt(EP_STATUS);
                }
                Err(e) => {
                    debug!("status reader stopped: {e}");
                    return;
                }
            }
        }
    })
}

/// Extracts `"key":"123"` or `"key":123` from a flat JSON string.
fn json_number(json: &str, key: &str) -> Option<u32> {
    let rest = &json[json.find(&format!("\"{key}\""))? + key.len() + 2..];
    let rest = rest.trim_start_matches([':', ' ', '"', '\t', '\n', '\r']);
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Extracts `"key":1.25` from a flat JSON string.
fn json_float(json: &str, key: &str) -> Option<f32> {
    let rest = &json[json.find(&format!("\"{key}\""))? + key.len() + 2..];
    let rest = rest.trim_start_matches([':', ' ', '"', '\t', '\n', '\r']);
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Extracts `"key":"text"` from a flat JSON string.
fn json_string(json: &str, key: &str) -> Option<String> {
    let rest = &json[json.find(&format!("\"{key}\""))? + key.len() + 2..];
    let rest = rest.trim_start_matches([':', ' ', '\t', '\n', '\r']).strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_string())
}

/// Blocking access to decoded frames.
pub struct FrameStream {
    handle: Handle,
    asm: FrameAssembler,
    size: Arc<AtomicU32>,
    battery: Arc<Mutex<Battery>>,
    /// Tells the status reader to finish, so the USB handle can be released.
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    closed: bool,
    buf: Vec<u8>,
    ready: Vec<Frame>,
}

impl FrameStream {
    /// Stops the streams and releases the camera, so another program (or a
    /// later run) can open it and it can be unplugged safely. Also done on
    /// drop.
    pub fn close(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.stop.store(true, Ordering::Relaxed);
        let _ = set_stream(&self.handle, 2, false, &[]);
        let _ = set_stream(&self.handle, 1, false, &[]);
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
        for i in 0..3 {
            let _ = self.handle.release_interface(i);
        }
        info!("camera released");
    }

    /// Latest battery state reported by the camera.
    pub fn battery(&self) -> Battery {
        self.battery.lock().map(|b| b.clone()).unwrap_or_default()
    }

    /// Blocks until the next complete frame arrives. Corrupt frames are
    /// logged and skipped.
    pub fn next_frame(&mut self) -> Result<Frame> {
        let mut idle = Duration::ZERO;
        loop {
            if !self.ready.is_empty() {
                return Ok(self.ready.remove(0));
            }
            let s = self.size.load(Ordering::Relaxed);
            if s != 0 {
                self.asm.size = Some(((s >> 16) as usize, (s & 0xffff) as usize));
            }
            match self.handle.read_bulk(EP_FRAME, &mut self.buf, READ_TIMEOUT) {
                Ok(n) => {
                    idle = Duration::ZERO;
                    for f in self.asm.push(&self.buf[..n]) {
                        match f {
                            Ok(f) => self.ready.push(f),
                            Err(e) => warn!("dropping frame: {e}"),
                        }
                    }
                }
                Err(rusb::Error::Timeout) => {
                    idle += READ_TIMEOUT;
                    if idle >= GIVE_UP_AFTER {
                        return Err(Error::NoData);
                    }
                }
                Err(rusb::Error::Pipe) => {
                    warn!("frame endpoint stalled, clearing");
                    self.handle.clear_halt(EP_FRAME)?;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{crc32, json_number, Battery};

    #[test]
    fn tracks_battery_messages() {
        let mut b = Battery::default();
        assert!(b.update(r#"{"type":"batteryVoltageUpdate","data":{"voltage":4.11999988555908,"percentage":100}}"#));
        assert!(b.update(r#"{"type":"batteryChargingStateUpdate","data":{"chargingState":"stateNoCharging"}}"#));
        assert_eq!(b.percent, Some(100));
        assert!((b.voltage.unwrap() - 4.12).abs() < 0.01);
        assert!(!b.is_charging());
        b.update(r#"{"type":"batteryChargingCurrentUpdate","data":{"chargingCurrent":1000}}"#);
        b.update(r#"{"type":"batteryChargingStateUpdate","data":{"chargingState":"stateChargingSmartPhone"}}"#);
        assert!(b.is_charging());
        assert!(!b.update(r#"{"type":"sledInformation","data":{}}"#));
    }

    #[test]
    fn header_crc_matches_reference_driver() {
        let mut h = Vec::new();
        for w in [0x1ccu32, 1, 0x41] {
            h.extend_from_slice(&w.to_le_bytes());
        }
        assert_eq!(crc32(&h).to_le_bytes(), [0xf8, 0xb3, 0xf7, 0x00]);
    }

    #[test]
    fn reads_sensor_size() {
        let j = r#"{"type":"sledInformation","data":{"thermalHeight":"60","thermalWidth":"80"}}"#;
        assert_eq!(json_number(j, "thermalWidth"), Some(80));
        assert_eq!(json_number(j, "thermalHeight"), Some(60));
    }
}
