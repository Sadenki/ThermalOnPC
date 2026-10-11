//! Background thread that owns the USB camera, reconnecting as needed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use flir_proto::{Calibration, Frame};

pub enum Msg {
    Status(String),
    /// No camera stream: why (the last error), while retrying.
    Waiting(String),
    Frame(Frame),
    Calibration(Calibration),
    Battery(flir_usb::Battery),
    /// The camera was stopped and released on request: it can be unplugged.
    Released,
}

/// Lets the UI disconnect the camera cleanly and reconnect it later.
#[derive(Clone, Default)]
pub struct Control {
    paused: Arc<AtomicBool>,
    /// True while the thread holds no USB handle.
    idle: Arc<AtomicBool>,
}

impl Control {
    pub fn disconnect(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    pub fn reconnect(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Disconnects and waits (up to `timeout`) for the camera to be
    /// released, so it is not left reserved when the program quits.
    pub fn shutdown(&self, timeout: Duration) {
        self.disconnect();
        let start = Instant::now();
        while !self.idle.load(Ordering::Relaxed) && start.elapsed() < timeout {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

pub fn spawn() -> (Receiver<Msg>, Control) {
    let (tx, rx) = sync_channel(4);
    let control = Control::default();
    let ctl = control.clone();
    // FLIR_REPLAY=<dossier> rejoue des trames enregistrées par l'exemple
    // `dump`, pour travailler sans caméra.
    if let Some(dir) = std::env::var_os("FLIR_REPLAY") {
        ctl.idle.store(true, Ordering::Relaxed);
        thread::spawn(move || replay(std::path::PathBuf::from(dir), &tx, &ctl));
        return (rx, control);
    }
    thread::spawn(move || loop {
        if ctl.is_paused() {
            ctl.idle.store(true, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        ctl.idle.store(false, Ordering::Relaxed);
        let result = run(&tx, &ctl);
        ctl.idle.store(true, Ordering::Relaxed);
        if let Err(e) = result {
            log::warn!("{e}");
            let why = match &e {
                flir_usb::Error::NotFound => "aucune caméra FLIR One sur les ports USB".to_string(),
                flir_usb::Error::NoData => "la caméra ne répond plus".to_string(),
                flir_usb::Error::Usb(u) => format!("problème d'accès USB ({u})"),
                other => other.to_string(),
            };
            if tx.send(Msg::Waiting(why)).is_err() {
                return;
            }
        }
        if !ctl.is_paused() {
            thread::sleep(Duration::from_secs(2));
        }
    });
    (rx, control)
}

fn run(tx: &SyncSender<Msg>, ctl: &Control) -> flir_usb::Result<()> {
    let camera = flir_usb::Camera::open()?;
    match camera.read_calibration() {
        Ok(cal) => {
            let _ = tx.send(Msg::Calibration(cal));
        }
        Err(e) => log::warn!("using default calibration: {e}"),
    }
    let mut stream = camera.start()?;
    let _ = tx.send(Msg::Status("Caméra connectée".into()));
    let mut battery = flir_usb::Battery::default();
    loop {
        if ctl.is_paused() {
            stream.close();
            let _ = tx.send(Msg::Released);
            return Ok(());
        }
        let f = stream.next_frame()?;
        let b = stream.battery();
        if b != battery {
            battery = b.clone();
            let _ = tx.send(Msg::Battery(b));
        }
        match tx.try_send(Msg::Frame(f)) {
            // The UI is behind: drop this frame rather than queueing latency.
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => return Ok(()),
        }
    }
}

fn replay(dir: std::path::PathBuf, tx: &SyncSender<Msg>, ctl: &Control) {
    if let Ok(zip) = std::fs::read(dir.join("CameraFiles.zip")) {
        if let Ok(cal) = flir_usb::calibration_from_zip(&zip) {
            let _ = tx.send(Msg::Calibration(cal));
        }
    }
    let mut frames = Vec::new();
    for i in 0.. {
        let read = |suffix: &str| std::fs::read(dir.join(format!("{i:03}-{suffix}")));
        let Ok(block) = read("thermal-block.bin") else { break };
        let (w, h) = (flir_proto::GEN3_WIDTH, flir_proto::GEN3_HEIGHT);
        let Ok(thermal) = flir_proto::frame::decode_thermal(&block, w, h) else { continue };
        frames.push(Frame {
            width: w,
            height: h,
            thermal,
            thermal_block: block,
            jpeg: read("visible.jpg").unwrap_or_default(),
            status: read("status.txt").unwrap_or_default(),
        });
    }
    if frames.is_empty() {
        let _ = tx.send(Msg::Status(format!("Aucune trame dans {}", dir.display())));
        return;
    }
    let _ = tx.send(Msg::Status("Relecture".into()));
    // Sample battery state so the indicator can be checked without a camera.
    let _ = tx.send(Msg::Battery(flir_usb::Battery {
        percent: Some(76),
        voltage: Some(3.95),
        charging_state: Some("stateChargingSmartPhone".into()),
        charging_current: Some(1000),
    }));
    let mut released = false;
    for f in frames.iter().cycle() {
        if ctl.is_paused() {
            if !released && tx.send(Msg::Released).is_err() {
                return;
            }
            released = true;
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        if released {
            released = false;
            let _ = tx.send(Msg::Status("Relecture".into()));
        }
        if tx.send(Msg::Frame(f.clone())).is_err() {
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }
}
