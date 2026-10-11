//! Live viewer: thermal image with a colour palette, temperature readouts,
//! and the visible camera image side by side.
//!
//! Built for board repair: the hot spot is tracked automatically, points
//! can be pinned with a click, and a reference image taken before power is
//! injected can be subtracted so only what heats up stands out.

mod avi;
mod camera_thread;
mod holders;
mod render;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Instant;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke};
use flir_proto::{Calibration, Frame, ObjectParams, Spotmeter, TemperatureLut, LEPTON_RAW_SCALE};
use serde::{Deserialize, Serialize};

use camera_thread::Msg;

const SETTINGS_KEY: &str = "settings";
const MAX_MARKERS: usize = 9;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let (rx, control) = camera_thread::spawn();
    eframe::run_native(
        "FLIR One PC",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size(window_size())
                .with_min_inner_size([640.0, 400.0])
                .with_icon(app_icon()),
            ..Default::default()
        },
        Box::new(|cc| {
            let settings = cc.storage.and_then(|s| eframe::get_value(s, SETTINGS_KEY)).unwrap_or_default();
            Ok(Box::new(Viewer::new(rx, control, settings)))
        }),
    )
}

/// Window icon: a rainbow thermal image with a hot-spot crosshair.
fn app_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png")).expect("embedded icon").to_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData { rgba: img.into_raw(), width, height }
}

/// Initial window size; FLIR_WINDOW=LxH overrides it (handy for testing).
fn window_size() -> [f32; 2] {
    std::env::var("FLIR_WINDOW")
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some([w.parse().ok()?, h.parse().ok()?])
        })
        .unwrap_or([1280.0, 720.0])
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Palette {
    Iron,
    Turbo,
    Gray,
}

impl Palette {
    fn color(self, t: f64) -> Color32 {
        let t = t.clamp(0.0, 1.0);
        let c = match self {
            Palette::Iron => colorous::INFERNO.eval_continuous(t),
            Palette::Turbo => colorous::TURBO.eval_continuous(t),
            Palette::Gray => colorous::GREYS.eval_continuous(1.0 - t),
        };
        Color32::from_rgb(c.r, c.g, c.b)
    }
}

/// User settings, kept between sessions.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    palette: Palette,
    emissivity: f64,
    reflected_temp_c: f64,
    /// Added to every temperature, to match a reference thermometer.
    offset_c: f32,
    /// Anchor temperatures on the Lepton's own reading (Gen 3 telemetry).
    lepton_reference: bool,
    /// Weight of the previous image in the running average (0 = none).
    smoothing: f32,
    smooth_display: bool,
    show_hotspot: bool,
    fixed_range: bool,
    range_min: f32,
    range_max: f32,
    show_photo: bool,
    /// Where the thermal image sits in the photo: its width as a fraction
    /// of the photo's, and the offset of its centre (fractions of the photo).
    align_scale: f32,
    align_dx: f32,
    align_dy: f32,
    /// Opacity of the thermal image drawn over the photo (0 = off).
    overlay_alpha: f32,
    /// Name given to capture folders, e.g. "Dossier n°54 Xbox Series X".
    capture_name: String,
    /// Where capture folders go; empty means `captures` next to the program.
    output_dir: String,
}

impl Default for Settings {
    fn default() -> Self {
        let p = ObjectParams::default();
        Self {
            palette: Palette::Iron,
            emissivity: p.emissivity,
            reflected_temp_c: p.reflected_temp_c,
            offset_c: 0.0,
            lepton_reference: true,
            smoothing: 0.5,
            smooth_display: false,
            show_hotspot: true,
            fixed_range: false,
            range_min: 20.0,
            range_max: 40.0,
            show_photo: true,
            align_scale: 0.8,
            align_dx: 0.0,
            align_dy: 0.0,
            overlay_alpha: 0.0,
            capture_name: String::new(),
            output_dir: String::new(),
        }
    }
}

impl Settings {
    fn object_params(&self) -> ObjectParams {
        ObjectParams { emissivity: self.emissivity, reflected_temp_c: self.reflected_temp_c }
    }

    fn capture_root(&self) -> PathBuf {
        if self.output_dir.trim().is_empty() {
            default_capture_root()
        } else {
            PathBuf::from(self.output_dir.trim())
        }
    }
}

/// Alignment by pointing the same feature in both images: a click on the
/// thermal image, then on the photo. One pair sets the position, a second
/// pair also sets the size.
#[derive(Default)]
struct Align {
    /// Clicked points, as fractions of each image (0..1).
    thermal: Vec<egui::Vec2>,
    photo: Vec<egui::Vec2>,
}

impl Align {
    /// True when the next click goes on the thermal image.
    fn wants_thermal(&self) -> bool {
        self.thermal.len() == self.photo.len()
    }

    fn instruction(&self) -> &'static str {
        match (self.thermal.len(), self.photo.len()) {
            (0, _) => "1. Cliquez sur un détail net dans l'image thermique (bout du doigt, pointe du fer…).",
            (1, 0) => "2. Cliquez sur le même détail dans la photo.",
            (1, 1) => "Position réglée. Pour la taille, cliquez un 2e détail éloigné du premier dans l'image thermique, ou Terminer.",
            (2, 1) => "4. Cliquez sur ce 2e détail dans la photo.",
            _ => "Position et taille réglées. Terminer, ou Recommencer.",
        }
    }

    /// Position and size so that each thermal point lands on its photo
    /// point: photo = 0.5 + d + (thermal - 0.5) · scale.
    fn solve(&self, scale: f32) -> (f32, egui::Vec2) {
        let pairs: Vec<_> = self.thermal.iter().zip(&self.photo).map(|(t, p)| (*t - egui::vec2(0.5, 0.5), *p)).collect();
        let mut s = scale;
        if let [(u1, p1), (u2, p2)] = pairs[..] {
            let du = (u2 - u1).length();
            if du > 0.05 {
                s = ((p2 - p1).length() / du).clamp(0.2, 1.5);
            }
        }
        let n = pairs.len().max(1) as f32;
        let d = pairs.iter().fold(egui::Vec2::ZERO, |acc, (u, p)| acc + (*p - egui::vec2(0.5, 0.5) - *u * s)) / n;
        (s, d)
    }
}

struct Viewer {
    rx: Receiver<Msg>,
    status: String,
    frame: Option<Frame>,
    settings: Settings,
    cal: Calibration,
    cal_from_camera: bool,
    lut: TemperatureLut,
    lut_params: ObjectParams,
    /// Raw offset that makes the conversion agree with the Lepton's own
    /// reading (smoothed), and the value the current table was built with.
    anchor: Option<f64>,
    lut_anchor: Option<f64>,
    spot: Option<Spotmeter>,
    /// Running average of temperatures (°C, offset not applied).
    avg: Vec<f32>,
    /// Image taken before injection; when set, ΔT is displayed.
    reference: Option<Vec<f32>>,
    /// Displayed values: °C, or ΔT against the reference.
    shown: Vec<f32>,
    size: (usize, usize),
    markers: Vec<(usize, usize)>,
    frozen: bool,
    thermal_tex: Option<egui::TextureHandle>,
    visible_tex: Option<egui::TextureHandle>,
    color_range: (f32, f32),
    /// Point-and-click alignment in progress.
    align: Option<Align>,
    /// In ΔT mode, warming below this shows dark; None = automatic (0).
    delta_floor: Option<f32>,
    control: camera_thread::Control,
    recording: Option<Recording>,
    /// Frames are arriving; otherwise the connection help screen shows.
    connected: bool,
    waiting_since: Instant,
    last_error: Option<String>,
    holders: holders::Scanner,
    /// Feedback after trying to close a program.
    kill_note: Option<String>,
    /// Result of the last save (image or video), shown under the status.
    notice: Option<String>,
    battery: Option<flir_usb::Battery>,
}

/// A video being recorded: annotated thermal video, the camera's photos as
/// a second video, and the temperatures of each frame.
struct Recording {
    dir: PathBuf,
    thermal: avi::AviWriter,
    photo: Option<avi::AviWriter>,
    csv: std::io::BufWriter<std::fs::File>,
    started: Instant,
}

impl Viewer {
    fn new(rx: Receiver<Msg>, control: camera_thread::Control, settings: Settings) -> Self {
        let cal = Calibration::default();
        let params = settings.object_params();
        Self {
            rx,
            status: "Recherche de la caméra…".into(),
            frame: None,
            lut: TemperatureLut::new(&cal, &params),
            lut_params: params,
            anchor: None,
            lut_anchor: None,
            spot: None,
            settings,
            cal,
            cal_from_camera: false,
            avg: Vec::new(),
            reference: None,
            shown: Vec::new(),
            size: (80, 60),
            markers: Vec::new(),
            frozen: false,
            thermal_tex: None,
            visible_tex: None,
            color_range: (0.0, 1.0),
            align: None,
            delta_floor: None,
            control,
            recording: None,
            connected: false,
            waiting_since: Instant::now(),
            last_error: None,
            holders: holders::Scanner::new(),
            kill_note: None,
            notice: None,
            battery: None,
        }
    }

    fn delta_mode(&self) -> bool {
        self.reference.is_some()
    }

    fn poll_camera(&mut self, ctx: &egui::Context) {
        // Keep only the newest frame so the display never lags behind.
        let mut newest = None;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Status(s) => self.status = s,
                Msg::Battery(b) => self.battery = Some(b),
                Msg::Released => {
                    self.connected = false;
                    self.frozen = false;
                    self.battery = None;
                    self.status = "Caméra libérée".into();
                }
                Msg::Waiting(e) => {
                    if self.connected {
                        self.connected = false;
                        self.waiting_since = Instant::now();
                        self.frozen = false;
                        self.stop_recording();
                        self.status = "Caméra déconnectée".into();
                        self.battery = None;
                    }
                    self.last_error = Some(e);
                }
                Msg::Frame(f) => newest = Some(f),
                Msg::Calibration(cal) => {
                    self.cal = cal;
                    self.cal_from_camera = true;
                    // The anchor was computed with the old constants.
                    self.anchor = None;
                    self.lut_anchor = None;
                    self.rebuild_lut();
                }
            }
        }
        let Some(f) = newest else { return };
        if self.control.is_paused() {
            return;
        }
        self.connected = true;
        self.kill_note = None;
        self.last_error = None;
        if self.frozen {
            return;
        }
        self.status = "Flux en direct".into();
        if let Ok(img) = image::load_from_memory_with_format(&f.jpeg, image::ImageFormat::Jpeg) {
            let rgb = img.to_rgb8();
            let size = [rgb.width() as usize, rgb.height() as usize];
            let ci = egui::ColorImage::from_rgb(size, rgb.as_raw());
            self.visible_tex = Some(ctx.load_texture("visible", ci, Default::default()));
        }
        self.update_anchor(&f);
        // During a shutter calibration the sensor sees the shutter: keep the
        // last scene instead of averaging the shutter in.
        if !f.is_ffc() {
            self.accumulate(&f);
        }
        self.frame = Some(f);
        self.refresh(ctx);
        if let Err(e) = self.record_frame() {
            self.notice = Some(format!("Vidéo interrompue : {e}"));
            self.stop_recording();
        }
    }

    /// Constants actually used: the camera's, shifted onto the Lepton's
    /// reading when that is available and enabled.
    fn effective_cal(&self) -> Calibration {
        match self.lut_anchor {
            Some(raw_offset) => Calibration { raw_scale: LEPTON_RAW_SCALE, raw_offset, ..self.cal },
            None => self.cal,
        }
    }

    /// Follows the Lepton's own reading of the centre pixels. Raw counts
    /// drift slowly with the sensor temperature and jump at each shutter
    /// calibration: drifts are smoothed, jumps applied at once. The table
    /// is only rebuilt when the shift is worth ~0.02 °C.
    fn update_anchor(&mut self, f: &Frame) {
        self.spot = f.spotmeter();
        let target = self.spot.filter(|_| self.settings.lepton_reference).map(|s| self.cal.anchored(s.raw_mean, s.celsius).raw_offset);
        self.anchor = match (target, self.anchor) {
            (Some(t), Some(a)) if (t - a).abs() < 150.0 => Some(a + 0.2 * (t - a)),
            (t, _) => t,
        };
        match (self.anchor, self.lut_anchor) {
            (Some(a), Some(b)) if (a - b).abs() > 5.0 => {
                self.lut_anchor = self.anchor;
                self.lut = TemperatureLut::new(&self.effective_cal(), &self.lut_params);
            }
            (a, b) if a.is_some() != b.is_some() => {
                self.lut_anchor = self.anchor;
                self.rebuild_lut();
            }
            _ => {}
        }
    }

    fn rebuild_lut(&mut self) {
        self.lut_params = self.settings.object_params();
        self.lut = TemperatureLut::new(&self.effective_cal(), &self.lut_params);
        // Old averages were computed with other constants.
        self.avg.clear();
        if let Some(f) = self.frame.take() {
            self.accumulate(&f);
            self.frame = Some(f);
        }
    }

    fn accumulate(&mut self, f: &Frame) {
        let size = (f.width, f.height);
        if size != self.size || self.avg.len() != f.thermal.len() {
            if size != self.size {
                self.reference = None;
                self.markers.clear();
            }
            self.size = size;
            self.avg = f.thermal.iter().map(|&r| self.lut.celsius(r)).collect();
            return;
        }
        let a = self.settings.smoothing.clamp(0.0, 0.95);
        for (avg, &raw) in self.avg.iter_mut().zip(&f.thermal) {
            let t = self.lut.celsius(raw);
            *avg = if avg.is_finite() { a * *avg + (1.0 - a) * t } else { t };
        }
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        if self.avg.is_empty() {
            return;
        }
        let offset = self.settings.offset_c;
        self.shown = match &self.reference {
            Some(r) => self.avg.iter().zip(r).map(|(t, r)| t - r).collect(),
            None => self.avg.iter().map(|t| t + offset).collect(),
        };
        let (lo, hi) = min_max(&self.shown);
        self.color_range = if self.delta_mode() {
            // Only heating matters: threshold → dark, hottest → bright.
            let floor = self.delta_floor.unwrap_or(0.0);
            (floor, hi.max(floor + 0.5))
        } else if self.settings.fixed_range {
            (self.settings.range_min, self.settings.range_max.max(self.settings.range_min + 0.1))
        } else {
            (lo, hi.max(lo + 0.1))
        };
        let (lo, hi) = self.color_range;
        let pixels = self.shown.iter().map(|&t| self.settings.palette.color(((t - lo) / (hi - lo)) as f64)).collect();
        let (w, h) = self.size;
        let ci = egui::ColorImage { size: [w, h], pixels, ..Default::default() };
        let opts = if self.settings.smooth_display { egui::TextureOptions::LINEAR } else { egui::TextureOptions::NEAREST };
        self.thermal_tex = Some(ctx.load_texture("thermal", ci, opts));
    }

    fn hotspot(&self) -> Option<(usize, usize, f32)> {
        let (w, _) = self.size;
        self.shown
            .iter()
            .enumerate()
            .filter(|(_, t)| t.is_finite())
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, &t)| (i % w, i / w, t))
    }

    fn value_text(&self, t: f32) -> String {
        if self.delta_mode() {
            format!("{t:+.2} °C")
        } else {
            format!("{t:.1} °C")
        }
    }

    fn palette_colors(&self) -> Vec<[u8; 3]> {
        let (lo, hi) = self.color_range;
        self.shown
            .iter()
            .map(|&t| {
                let c = self.settings.palette.color(((t - lo) / (hi - lo)) as f64);
                [c.r(), c.g(), c.b()]
            })
            .collect()
    }

    /// The thermal image as seen on screen (8× enlarged) with the hot spot
    /// and points drawn in.
    fn annotated_thermal(&self) -> image::RgbImage {
        let (w, h) = self.size;
        let hotspot = if self.settings.show_hotspot {
            self.hotspot().map(|(x, y, t)| (x, y, self.value_text(t)))
        } else {
            None
        };
        let markers: Vec<_> = self
            .markers
            .iter()
            .map(|&(x, y)| (x, y, self.value_text(self.shown.get(y * w + x).copied().unwrap_or(f32::NAN))))
            .collect();
        render::thermal_image(&self.palette_colors(), w, h, &render::Overlay { hotspot, markers: &markers })
    }

    /// Saves the image thermique (PNG), the photo and the values (CSV, for
    /// Excel) in a new folder named after the date and the capture name.
    fn snapshot(&self) -> std::io::Result<PathBuf> {
        let Some(f) = &self.frame else { return Err(std::io::Error::other("aucune image")) };
        if self.shown.is_empty() {
            return Err(std::io::Error::other("aucune image"));
        }
        let dir = new_capture_dir(&self.settings.capture_root(), &self.settings.capture_name, None)?;
        self.annotated_thermal().save(dir.join("image thermique.png")).map_err(std::io::Error::other)?;
        std::fs::write(dir.join("photo.jpg"), &f.jpeg)?;
        let mut csv = csv_writer(&dir.join("valeurs brutes.csv"))?;
        let what = if self.delta_mode() { "Écart par rapport à la référence (°C)" } else { "Température (°C)" };
        writeln!(csv, "{what};une valeur par pixel, ligne par ligne")?;
        for row in self.shown.chunks(self.size.0) {
            let line: Vec<String> = row.iter().map(|&t| excel_number(t, 2)).collect();
            writeln!(csv, "{}", line.join(";"))?;
        }
        csv.flush()?;
        Ok(dir)
    }

    fn start_recording(&mut self) -> std::io::Result<()> {
        let dir = new_capture_dir(&self.settings.capture_root(), &self.settings.capture_name, Some("vidéo"))?;
        let (w, h) = self.size;
        let thermal =
            avi::AviWriter::create(&dir.join("vidéo thermique.avi"), w as u32 * render::SCALE, h as u32 * render::SCALE)?;
        let mut csv = csv_writer(&dir.join("températures.csv"))?;
        let what = if self.delta_mode() { "écart (°C)" } else { "(°C)" };
        let mut header = format!("Temps (s);Point chaud {what}");
        for n in 1..=self.markers.len() {
            header.push_str(&format!(";Point {n} {what}"));
        }
        writeln!(csv, "{header}")?;
        self.recording = Some(Recording { dir, thermal, photo: None, csv, started: Instant::now() });
        Ok(())
    }

    fn record_frame(&mut self) -> std::io::Result<()> {
        if self.recording.is_none() || self.shown.is_empty() {
            return Ok(());
        }
        let img = self.annotated_thermal();
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode_image(&img)
            .map_err(std::io::Error::other)?;
        let hot = self.hotspot().map_or(f32::NAN, |h| h.2);
        let w = self.size.0;
        let points: Vec<f32> =
            self.markers.iter().map(|&(x, y)| self.shown.get(y * w + x).copied().unwrap_or(f32::NAN)).collect();
        let photo = self.frame.as_ref().map(|f| f.jpeg.clone()).unwrap_or_default();
        let photo_size = self.visible_tex.as_ref().map(|t| t.size());

        let rec = self.recording.as_mut().expect("checked above");
        rec.thermal.add_jpeg(&jpeg)?;
        if let (false, Some([pw, ph])) = (photo.is_empty(), photo_size) {
            if rec.photo.is_none() {
                rec.photo = Some(avi::AviWriter::create(&rec.dir.join("vidéo photo.avi"), pw as u32, ph as u32)?);
            }
            if let Some(p) = &mut rec.photo {
                p.add_jpeg(&photo)?;
            }
        }
        let mut line = format!("{};{}", excel_number(rec.started.elapsed().as_secs_f32(), 2), excel_number(hot, 2));
        for t in points {
            line.push(';');
            line.push_str(&excel_number(t, 2));
        }
        writeln!(rec.csv, "{line}")
    }

    fn stop_recording(&mut self) {
        let Some(mut rec) = self.recording.take() else { return };
        let frames = rec.thermal.frames();
        let duration = rec.started.elapsed();
        let result = rec
            .thermal
            .finish(duration)
            .and_then(|()| rec.photo.map_or(Ok(()), |p| p.finish(duration)))
            .and_then(|()| rec.csv.flush());
        self.notice = Some(match result {
            Ok(()) => format!("Vidéo enregistrée ({frames} images) : {}", rec.dir.display()),
            Err(e) => format!("Échec de l'enregistrement vidéo : {e}"),
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let before = self.settings.clone();
        ui.vertical_centered(|ui| ui.heading("FLIR One PC"));
        self.status_badge(ui);
        self.connection_button(ui);
        ui.label("Nom des captures");
        ui.add(egui::TextEdit::singleline(&mut self.settings.capture_name).hint_text("Dossier n°54 Xbox Series X"));
        self.output_folder(ui);
        ui.horizontal(|ui| {
            let label = if self.frozen { "▶ Reprendre" } else { "⏸ Figer" };
            if ui.button(label).clicked() {
                self.frozen = !self.frozen;
            }
            if ui.button("📷 Image").on_hover_text("Enregistrer l'image").clicked() {
                self.notice = Some(match self.snapshot() {
                    Ok(p) => format!("Enregistré dans {}", p.display()),
                    Err(e) => format!("Échec de l'enregistrement : {e}"),
                });
            }
            match &self.recording {
                None => {
                    if ui.button("⏺ Vidéo").clicked() {
                        if let Err(e) = self.start_recording() {
                            self.notice = Some(format!("Échec de la vidéo : {e}"));
                        }
                    }
                }
                Some(rec) => {
                    let s = rec.started.elapsed().as_secs();
                    let label = egui::RichText::new(format!("⏹ {:02}:{:02}", s / 60, s % 60)).color(Color32::from_rgb(255, 90, 90));
                    if ui.button(label).on_hover_text("Arrêter la vidéo").clicked() {
                        self.stop_recording();
                    }
                }
            }
        });
        if let Some(n) = &self.notice {
            ui.label(egui::RichText::new(n).small().weak());
        }
        if self.connected {
            if let Some(b) = &self.battery {
                battery_widget(ui, b);
            }
        }
        ui.separator();

        if let (true, Some((_, _, t))) = (self.connected, self.hotspot()) {
            let (lo, _) = min_max(&self.shown);
            ui.label(egui::RichText::new(format!("Point chaud : {}", self.value_text(t))).strong());
            if !self.delta_mode() {
                ui.label(format!("Min : {lo:.1} °C"));
            }
        }
        for (n, &(x, y)) in self.markers.iter().enumerate() {
            if let Some(&t) = self.shown.get(y * self.size.0 + x) {
                ui.label(format!("Point {} : {}", n + 1, self.value_text(t)));
            }
        }
        if !self.markers.is_empty() && ui.button("Effacer les points").clicked() {
            self.markers.clear();
        }
        ui.small("Clic : poser un point · clic droit : retirer le plus proche");
        ui.separator();

        ui.collapsing("Diagnostic par injection", |ui| {
            ui.small(
                "Prendre la référence carte hors tension, puis injecter : \
                 seul ce qui chauffe apparaît (écart en °C).",
            );
            ui.horizontal(|ui| {
                if ui.button("📌 Prendre la référence").clicked() && !self.avg.is_empty() {
                    self.reference = Some(self.avg.clone());
                }
                if self.reference.is_some() && ui.button("Effacer").clicked() {
                    self.reference = None;
                    self.delta_floor = None;
                }
            });
            if self.delta_mode() {
                ui.label("Mode écart actif");
                self.threshold_controls(ui, ctx);
            }
        });

        ui.collapsing("Affichage", |ui| {
            for (p, name) in [(Palette::Iron, "Fer"), (Palette::Turbo, "Arc-en-ciel"), (Palette::Gray, "Gris")] {
                ui.radio_value(&mut self.settings.palette, p, name);
            }
            ui.checkbox(&mut self.settings.show_hotspot, "Repérer le point chaud");
            ui.checkbox(&mut self.settings.smooth_display, "Image lissée");
            ui.add(egui::Slider::new(&mut self.settings.smoothing, 0.0..=0.95).text("Moyenne (anti-bruit)"));
            ui.checkbox(&mut self.settings.fixed_range, "Échelle fixe");
            if self.settings.fixed_range {
                ui.add(egui::DragValue::new(&mut self.settings.range_min).speed(0.1).prefix("min ").suffix(" °C"));
                ui.add(egui::DragValue::new(&mut self.settings.range_max).speed(0.1).prefix("max ").suffix(" °C"));
            }
        });

        ui.collapsing("Photo et alignement", |ui| {
            ui.checkbox(&mut self.settings.show_photo, "Afficher la photo");
            ui.add(egui::Slider::new(&mut self.settings.overlay_alpha, 0.0..=1.0).text("Superposition"));
            match &self.align {
                None => {
                    ui.small(
                        "Aligner en pointant un même détail (bout du doigt, pointe du fer) \
                         dans l'image thermique puis dans la photo.",
                    );
                    if ui.button("🎯 Aligner en pointant").clicked() {
                        self.align = Some(Align::default());
                        self.settings.overlay_alpha = self.settings.overlay_alpha.max(0.5);
                    }
                }
                Some(a) => {
                    ui.label(egui::RichText::new(a.instruction()).color(Color32::YELLOW));
                    let done = !a.photo.is_empty();
                    ui.horizontal(|ui| {
                        if ui.button(if done { "✔ Terminer" } else { "Annuler" }).clicked() {
                            self.align = None;
                        }
                        if done && ui.button("Recommencer").clicked() {
                            self.align = Some(Align::default());
                        }
                    });
                }
            }
            ui.add(egui::Slider::new(&mut self.settings.align_scale, 0.2..=1.5).fixed_decimals(2).text("Taille"));
            ui.add(egui::Slider::new(&mut self.settings.align_dx, -0.5..=0.5).fixed_decimals(2).text("Décalage X"));
            ui.add(egui::Slider::new(&mut self.settings.align_dy, -0.5..=0.5).fixed_decimals(2).text("Décalage Y"));
            if ui.button("Réinitialiser l'alignement").clicked() {
                let d = Settings::default();
                self.settings.align_scale = d.align_scale;
                self.settings.align_dx = d.align_dx;
                self.settings.align_dy = d.align_dy;
            }
        });

        ui.collapsing("Réglages de mesure", |ui| {
            ui.add(egui::Slider::new(&mut self.settings.emissivity, 0.1..=1.0).text("Émissivité"));
            ui.add(egui::Slider::new(&mut self.settings.reflected_temp_c, -20.0..=60.0).text("T° réfléchie"));
            ui.add(egui::Slider::new(&mut self.settings.offset_c, -10.0..=10.0).step_by(0.1).text("Offset °C"));
            ui.small("L'offset corrige l'écart avec un thermomètre de référence.");
            ui.checkbox(&mut self.settings.lepton_reference, "Référence du capteur (recommandé)");
            match (self.settings.lepton_reference, self.spot) {
                (true, Some(s)) => ui.small(format!("Le capteur mesure {:.1} °C au centre ; les températures sont calées dessus.", s.celsius)),
                (true, None) => ui.small("Référence du capteur indisponible : constantes seules."),
                (false, _) => ui.small("Constantes seules : les valeurs peuvent être trop hautes de plusieurs degrés."),
            };
            if ui.button("Valeurs par défaut").clicked() {
                let (smoothing, palette) = (self.settings.smoothing, self.settings.palette);
                self.settings = Settings { smoothing, palette, ..Settings::default() };
            }
            ui.label(if self.cal_from_camera { "Constantes de la caméra" } else { "Constantes par défaut" });
        });

        if self.settings != before {
            if self.settings.object_params() != self.lut_params {
                self.rebuild_lut();
            }
            self.refresh(ctx);
        }

        if self.connected {
            ui.separator();
            self.color_bar(ui);
        }
    }

    fn color_bar(&self, ui: &mut egui::Ui) {
        let (lo, hi) = self.color_range;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width().min(200.0), 14.0), egui::Sense::hover());
        let steps = 64;
        for i in 0..steps {
            let x0 = rect.left() + rect.width() * i as f32 / steps as f32;
            let x1 = rect.left() + rect.width() * (i + 1) as f32 / steps as f32;
            let c = self.settings.palette.color(i as f64 / (steps - 1) as f64);
            ui.painter().rect_filled(Rect::from_x_y_ranges(x0..=x1, rect.y_range()), 0.0, c);
        }
        ui.horizontal(|ui| {
            ui.label(self.value_text(lo));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(self.value_text(hi)));
        });
    }

    /// Thermal pixel (centre) → normalised position in the photo.
    fn to_photo(&self, x: f32, y: f32) -> egui::Vec2 {
        let (w, h) = self.size;
        let s = &self.settings;
        let u = (x + 0.5) / w as f32 - 0.5;
        let v = (y + 0.5) / h as f32 - 0.5;
        egui::vec2(0.5 + s.align_dx + u * s.align_scale, 0.5 + s.align_dy + v * s.align_scale)
    }

    fn thermal_view(&mut self, ui: &mut egui::Ui, size: egui::Vec2) {
        let Some(tex) = &self.thermal_tex else { return };
        let (w, h) = self.size;
        let resp = ui.add(egui::Image::new(tex).fit_to_exact_size(size).sense(egui::Sense::click()));
        let rect = resp.rect;
        let to_pixel = |pos: Pos2| {
            let rel = (pos - rect.min) / rect.size();
            (((rel.x * w as f32) as usize).min(w - 1), ((rel.y * h as f32) as usize).min(h - 1))
        };
        let to_screen = |x: usize, y: usize| {
            rect.min + egui::vec2((x as f32 + 0.5) / w as f32 * rect.width(), (y as f32 + 0.5) / h as f32 * rect.height())
        };

        let aligning = self.align.as_ref().is_some_and(|a| a.wants_thermal() && a.thermal.len() < 2);
        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                if aligning {
                    if let Some(a) = &mut self.align {
                        a.thermal.push((pos - rect.min) / rect.size());
                    }
                } else if self.align.is_none() && self.markers.len() < MAX_MARKERS {
                    self.markers.push(to_pixel(pos));
                }
            }
        }
        if resp.secondary_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let nearest = self
                    .markers
                    .iter()
                    .enumerate()
                    .min_by(|a, b| {
                        let da = (to_screen(a.1 .0, a.1 .1) - pos).length_sq();
                        let db = (to_screen(b.1 .0, b.1 .1) - pos).length_sq();
                        da.total_cmp(&db)
                    })
                    .map(|(i, _)| i);
                if let Some(i) = nearest {
                    self.markers.remove(i);
                }
            }
        }

        let painter = ui.painter_at(rect);
        let cell = rect.width() / w as f32;
        if self.settings.show_hotspot {
            if let Some((x, y, t)) = self.hotspot() {
                let p = to_screen(x, y);
                let r = (cell * 1.5).max(10.0);
                crosshair(&painter, p, r);
                label_near(&painter, p, r + 4.0, &self.value_text(t));
            }
        }
        for (n, &(x, y)) in self.markers.iter().enumerate() {
            let p = to_screen(x, y);
            marker_dot(&painter, p);
            let t = self.shown.get(y * w + x).copied().unwrap_or(f32::NAN);
            label_near(&painter, p, 7.0, &format!("{} : {}", n + 1, self.value_text(t)));
        }
        if let Some(a) = &self.align {
            for (n, t) in a.thermal.iter().enumerate() {
                align_point(&painter, rect.min + *t * rect.size(), n + 1);
            }
            if aligning {
                painter.rect_stroke(rect, 0.0, Stroke::new(3.0, Color32::YELLOW), egui::StrokeKind::Inside);
            }
        }

        if let Some(pos) = resp.hover_pos() {
            let (x, y) = to_pixel(pos);
            let raw = self.frame.as_ref().map_or(0, |f| f.thermal.get(y * w + x).copied().unwrap_or(0));
            let t = self.shown.get(y * w + x).copied().unwrap_or(f32::NAN);
            resp.on_hover_text(format!("{} (brut {raw})", self.value_text(t)));
        }
    }
}

impl Viewer {
    fn photo_view(&mut self, ui: &mut egui::Ui, size: egui::Vec2) {
        let Some(tex) = &self.visible_tex else { return };
        let resp = ui.add(egui::Image::new(tex).fit_to_exact_size(size).sense(egui::Sense::click()));
        let rect = resp.rect;
        let at = |n: egui::Vec2| rect.min + n * rect.size();
        let painter = ui.painter_at(rect);

        if self.settings.overlay_alpha > 0.0 {
            if let Some(t) = &self.thermal_tex {
                let (w, h) = self.size;
                let r = Rect::from_min_max(
                    at(self.to_photo(-0.5, -0.5)),
                    at(self.to_photo(w as f32 - 0.5, h as f32 - 0.5)),
                );
                let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
                let a = (self.settings.overlay_alpha * 255.0) as u8;
                painter.image(t.id(), r, uv, Color32::from_white_alpha(a));
            }
        }

        if self.settings.show_hotspot {
            if let Some((x, y, t)) = self.hotspot() {
                let p = at(self.to_photo(x as f32, y as f32));
                crosshair(&painter, p, 14.0);
                label_near(&painter, p, 18.0, &self.value_text(t));
            }
        }
        for (n, &(x, y)) in self.markers.iter().enumerate() {
            let p = at(self.to_photo(x as f32, y as f32));
            marker_dot(&painter, p);
            label_near(&painter, p, 7.0, &format!("{}", n + 1));
        }

        if let Some(a) = &mut self.align {
            let waiting = !a.wants_thermal();
            if waiting {
                painter.rect_stroke(rect, 0.0, Stroke::new(3.0, Color32::YELLOW), egui::StrokeKind::Inside);
                if let (true, Some(pos)) = (resp.clicked(), resp.interact_pointer_pos()) {
                    a.photo.push((pos - rect.min) / rect.size());
                    let (scale, d) = a.solve(self.settings.align_scale);
                    self.settings.align_scale = scale;
                    self.settings.align_dx = d.x;
                    self.settings.align_dy = d.y;
                }
            }
            for (n, p) in a.photo.iter().enumerate() {
                align_point(&painter, at(*p), n + 1);
            }
        }
    }
}

fn crosshair(painter: &egui::Painter, p: Pos2, r: f32) {
    for (width, color) in [(4.0, Color32::BLACK), (2.0, Color32::WHITE)] {
        let s = Stroke::new(width, color);
        painter.circle_stroke(p, r, s);
        for d in [egui::vec2(1.0, 0.0), egui::vec2(-1.0, 0.0), egui::vec2(0.0, 1.0), egui::vec2(0.0, -1.0)] {
            painter.line_segment([p + d * r * 0.6, p + d * r * 1.8], s);
        }
    }
}

/// Yellow numbered target for an alignment click.
fn align_point(painter: &egui::Painter, p: Pos2, n: usize) {
    let s = Stroke::new(2.0, Color32::YELLOW);
    painter.circle_stroke(p, 7.0, Stroke::new(4.0, Color32::BLACK));
    painter.circle_stroke(p, 7.0, s);
    painter.line_segment([p - egui::vec2(11.0, 0.0), p + egui::vec2(11.0, 0.0)], s);
    painter.line_segment([p - egui::vec2(0.0, 11.0), p + egui::vec2(0.0, 11.0)], s);
    label_near(painter, p, 9.0, &format!("A{n}"));
}

fn marker_dot(painter: &egui::Painter, p: Pos2) {
    painter.circle_filled(p, 5.0, Color32::BLACK);
    painter.circle_filled(p, 3.5, Color32::from_rgb(80, 220, 255));
}

impl Viewer {
    /// Shown after "Déconnecter la caméra": safe to unplug.
    fn released_screen(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.2);
            ui.label(egui::RichText::new("⏏").size(48.0).color(Color32::from_rgb(160, 170, 185)));
            ui.heading("Caméra déconnectée");
            ui.label("Le flux est arrêté et la caméra libérée : vous pouvez la débrancher sans risque.");
            ui.add_space(16.0);
            if ui.add(egui::Button::new(egui::RichText::new("🔌 Reconnecter la caméra").size(16.0)).min_size(egui::vec2(220.0, 36.0))).clicked() {
                self.reconnect();
            }
        });
    }

    /// Shown until the camera streams: an animation of the camera being
    /// plugged in, then troubleshooting tips.
    fn waiting_screen(&mut self, ui: &mut egui::Ui) {
        self.holders.refresh();
        let t = ui.input(|i| i.time) as f32;
        let waited = self.waiting_since.elapsed().as_secs();
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.03);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(460.0, 160.0), egui::Sense::hover());
            plug_animation(ui.painter(), rect, t);
            ui.add_space(12.0);
            ui.heading("Branchez la caméra allumée à l'ordinateur");
            ui.label("La visionneuse démarre toute seule dès que la caméra est détectée.");
            ui.add_space(4.0);
            let dots = ".".repeat(1 + (t * 2.0) as usize % 3);
            ui.label(egui::RichText::new(format!("Recherche de la caméra{dots:<3}")).weak());
            ui.add_space(16.0);
            self.holders_box(ui);

            let frame = egui::Frame::group(ui.style()).inner_margin(12.0);
            frame.show(ui, |ui| {
                ui.set_max_width(520.0);
                let title = if waited >= 10 { "La caméra n'est pas détectée ?" } else { "Avant de commencer" };
                ui.label(egui::RichText::new(title).strong());
                ui.add_space(4.0);
                let tips = [
                    "Allumez la caméra : appui sur le bouton, le voyant vert s'allume.",
                    "Branchez-la directement sur un port USB de l'ordinateur ou avec une rallonge USB-C.",
                    "Si elle n'est pas détectée : débranchez-la, éteignez-la, rallumez-la puis rebranchez-la.",
                    "Batterie faible : branchez le chargeur sur la caméra pendant l'utilisation.",
                    "Fermez les autres logiciels qui pourraient utiliser la caméra.",
                    "Essayez un autre port USB.",
                ];
                let shown = if waited >= 10 { tips.len() } else { 2 };
                for (n, tip) in tips.iter().take(shown).enumerate() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(format!("{}.", n + 1)).strong());
                        ui.label(*tip);
                    });
                }
                if cfg!(windows) && waited >= 10 {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("•").strong());
                        ui.label("Sous Windows, le pilote UsbDk doit être installé (voir le README).");
                    });
                }
                if let (true, Some(e)) = (waited >= 10, &self.last_error) {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(format!("Détail : {e}")).small().weak());
                }
            });
        });
    }
}

impl Viewer {
    /// Coloured badge with the camera state, centred and always one line
    /// high so the controls below never move.
    fn status_badge(&self, ui: &mut egui::Ui) {
        let t = ui.input(|i| i.time) as f32;
        let ffc = self.frame.as_ref().is_some_and(|f| f.is_ffc());
        let amber = Color32::from_rgb(255, 176, 50);
        let grey = Color32::from_rgb(160, 170, 185);
        let paused = self.control.is_paused();
        let (color, text, pulse) = if paused && self.connected {
            (grey, "Déconnexion…", true)
        } else if paused {
            (grey, "Caméra libérée", false)
        } else if !self.connected {
            let text = if self.status == "Caméra déconnectée" { "Caméra déconnectée" } else { "Recherche de la caméra…" };
            (amber, text, true)
        } else if ffc {
            (amber, "Calibration de l'obturateur…", false)
        } else if self.frozen {
            (Color32::from_rgb(110, 170, 255), "Image figée", false)
        } else {
            (Color32::from_rgb(70, 210, 110), "Caméra connectée", false)
        };
        let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 38);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect(rect, 10.0, fill, Stroke::new(1.0, color), egui::StrokeKind::Inside);
        let galley = painter.layout_no_wrap(text.to_owned(), FontId::proportional(14.5), color);
        let width = 12.0 + 8.0 + galley.size().x;
        let left = rect.center().x - width / 2.0;
        let dot_at = Pos2::new(left + 6.0, rect.center().y);
        let a = if pulse { 0.45 + 0.55 * (t * 3.0).sin().abs() } else { 1.0 };
        if pulse {
            let halo = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), (60.0 * a) as u8);
            painter.circle_filled(dot_at, 6.0 + 2.0 * a, halo);
        }
        painter.circle_filled(dot_at, 4.5, Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), (255.0 * a) as u8));
        let text_pos = Pos2::new(left + 20.0, rect.center().y - galley.size().y / 2.0);
        painter.galley(text_pos, galley, color);
    }

    /// Disconnects the camera cleanly so it can be unplugged, or reconnects it.
    fn connection_button(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered_justified(|ui| {
            if self.control.is_paused() {
                if ui.button("🔌 Reconnecter la caméra").clicked() {
                    self.reconnect();
                }
            } else if self.connected
                && ui
                    .button("⏏ Déconnecter la caméra")
                    .on_hover_text("Arrête le flux et libère la caméra : elle peut ensuite être débranchée sans risque.")
                    .clicked()
            {
                self.disconnect();
            }
        });
    }

    fn disconnect(&mut self) {
        self.stop_recording();
        self.align = None;
        self.control.disconnect();
    }

    fn reconnect(&mut self) {
        self.waiting_since = Instant::now();
        self.last_error = None;
        self.status = "Recherche de la caméra…".into();
        self.control.reconnect();
    }

    /// Output folder, with buttons to pick another and to open it.
    fn output_folder(&mut self, ui: &mut egui::Ui) {
        ui.label("Dossier d'enregistrement");
        let root = self.settings.capture_root();
        let full = std::path::absolute(&root).unwrap_or(root.clone());
        ui.add(egui::Label::new(egui::RichText::new(full.display().to_string()).small().weak()).truncate())
            .on_hover_text(full.display().to_string());
        ui.horizontal(|ui| {
            if ui.button("📁 Choisir…").clicked() {
                let mut dialog = rfd::FileDialog::new().set_title("Dossier d'enregistrement des captures");
                if full.is_dir() {
                    dialog = dialog.set_directory(&full);
                }
                if let Some(dir) = dialog.pick_folder() {
                    self.settings.output_dir = dir.display().to_string();
                }
            }
            if ui.button("Ouvrir").on_hover_text("Ouvrir le dossier dans l'explorateur").clicked() {
                if let Err(e) = std::fs::create_dir_all(&full).and_then(|()| open_folder(&full)) {
                    self.notice = Some(format!("Impossible d'ouvrir le dossier : {e}"));
                }
            }
            if !self.settings.output_dir.is_empty() && ui.small_button("Par défaut").clicked() {
                self.settings.output_dir.clear();
            }
        });
    }

    /// In ΔT mode: the warming shown as the darkest colour, adjustable after
    /// the automatic setting (0 °C).
    fn threshold_controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let (_, hi) = min_max(&self.shown);
        let mut floor = self.delta_floor.unwrap_or(0.0);
        let before = floor;
        ui.label("Seuil (écart affiché en noir) :");
        ui.horizontal(|ui| {
            if ui.small_button("−").on_hover_text("Baisser le seuil de 0,1 °C").clicked() {
                floor -= 0.1;
            }
            ui.add(egui::DragValue::new(&mut floor).speed(0.02).fixed_decimals(2).suffix(" °C"));
            if ui.small_button("+").on_hover_text("Monter le seuil de 0,1 °C").clicked() {
                floor += 0.1;
            }
            let auto = self.delta_floor.is_none();
            if ui.selectable_label(auto, "Auto").on_hover_text("Seuil à 0 °C : tout ce qui chauffe apparaît").clicked() {
                self.delta_floor = None;
                self.refresh(ctx);
            }
        });
        let top = hi.max(floor).max(2.0);
        ui.spacing_mut().slider_width = ui.available_width() - 8.0;
        ui.add(egui::Slider::new(&mut floor, -2.0..=top).fixed_decimals(2).show_value(false));
        ui.small("Monter le seuil masque les petits échauffements ; le baisser fait ressortir les plus faibles.");
        if floor != before {
            self.delta_floor = Some(floor);
            self.refresh(ctx);
        }
    }

    /// Lists programs that may hold the camera, with a button to close each.
    fn holders_box(&mut self, ui: &mut egui::Ui) {
        if self.holders.found.is_empty() && self.kill_note.is_none() {
            return;
        }
        let warn = Color32::from_rgb(255, 180, 60);
        let mut to_kill = None;
        egui::Frame::group(ui.style()).inner_margin(12.0).stroke(Stroke::new(1.0, warn)).show(ui, |ui| {
            ui.set_max_width(520.0);
            if !self.holders.found.is_empty() {
                ui.label(egui::RichText::new("Ces programmes peuvent bloquer la caméra :").strong().color(warn));
                ui.add_space(4.0);
                for h in &self.holders.found {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} (n° {})", h.name, h.pid));
                        if ui.button("Fermer ce programme").clicked() {
                            to_kill = Some((h.pid, h.name.clone()));
                        }
                    });
                }
                ui.small("Une seule application peut utiliser la caméra à la fois.");
            }
            if let Some(note) = &self.kill_note {
                ui.add_space(4.0);
                ui.label(note);
            }
        });
        ui.add_space(12.0);
        if let Some((pid, name)) = to_kill {
            self.kill_note = Some(if self.holders.kill(pid) {
                format!(
                    "{name} a été fermé. Si la caméra n'est toujours pas détectée, \
                     débranchez-la et rebranchez-la."
                )
            } else {
                {
                let tool = if cfg!(target_os = "macos") { "le Moniteur d'activité" } else { "le Gestionnaire des tâches" };
                format!("Impossible de fermer {name} : fermez-le depuis {tool}.")
            }
            });
        }
    }
}

/// Battery icon with level and charging state. While charging, a light
/// sweeps across the fill and a small bolt glows.
fn battery_widget(ui: &mut egui::Ui, b: &flir_usb::Battery) {
    let Some(pct) = b.percent else { return };
    let t = ui.input(|i| i.time) as f32;
    let level = (pct.min(100) as f32) / 100.0;
    let color = if b.is_fault() {
        Color32::from_rgb(240, 80, 80)
    } else if pct > 50 {
        Color32::from_rgb(70, 210, 110)
    } else if pct > 20 {
        Color32::from_rgb(255, 176, 50)
    } else {
        Color32::from_rgb(240, 80, 80)
    };
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(egui::vec2(38.0, 18.0), egui::Sense::hover());
        let p = ui.painter();
        let body = Rect::from_min_size(r.min, egui::vec2(34.0, 18.0));
        let tip = Rect::from_min_size(Pos2::new(body.right(), body.center().y - 4.0), egui::vec2(3.0, 8.0));
        let outline = Color32::from_gray(190);
        p.rect_stroke(body, 3.0, Stroke::new(1.5, outline), egui::StrokeKind::Inside);
        p.rect_filled(tip, 1.0, outline);
        let inner = body.shrink(3.0);
        let fill = Rect::from_min_size(inner.min, egui::vec2((inner.width() * level).max(2.0), inner.height()));
        p.rect_filled(fill, 1.5, color);
        if b.is_charging() {
            // Light sweeping left to right across the fill.
            let x = inner.left() + (t * 0.6).fract() * inner.width();
            let band = Rect::from_x_y_ranges((x - 4.0).max(inner.left())..=(x + 4.0).min(fill.right().max(inner.left() + 1.0)), inner.y_range());
            if band.width() > 0.0 {
                p.rect_filled(band, 1.0, Color32::from_white_alpha(70));
            }
            // Small bolt, gently breathing.
            let a = 0.6 + 0.4 * (t * 2.5).sin().abs();
            let c = body.center();
            let bolt = Color32::from_rgba_unmultiplied(255, 240, 120, (255.0 * a) as u8);
            let edge = Stroke::new(1.0, Color32::from_black_alpha(160));
            for tri in [
                [egui::vec2(1.5, -7.0), egui::vec2(-4.0, 1.5), egui::vec2(1.0, 1.5)],
                [egui::vec2(-1.0, -1.5), egui::vec2(4.0, -1.5), egui::vec2(-1.5, 7.0)],
            ] {
                p.add(egui::Shape::convex_polygon(tri.iter().map(|v| c + *v).collect(), bolt, edge));
            }
        }
        let state = if b.is_fault() {
            "défaut de charge"
        } else if b.is_charging() {
            "en charge"
        } else {
            "sur batterie"
        };
        ui.label(egui::RichText::new(format!("{pct} %")).strong());
        ui.label(egui::RichText::new(state).color(if b.is_charging() { color } else { ui.visuals().weak_text_color() }));
    })
    .response
    .on_hover_text(b.voltage.map_or(String::new(), |v| format!("Batterie de la caméra : {v:.2} V")));
}

/// A camera sliding its USB-C plug into a laptop, then its LED lighting up.
fn plug_animation(painter: &egui::Painter, rect: Rect, t: f32) {
    let line = Color32::from_gray(170);
    let stroke = Stroke::new(2.0, line);
    let cycle = 3.0;
    let phase = (t % cycle) / cycle; // 0..1
    // 0–0.5: plug moves in; 0.5–1: connected, LED and signal pulse.
    let ease = |x: f32| 1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3);
    let travel = ease(phase / 0.5);
    let connected = phase >= 0.5;

    // Laptop (left): screen and base, with a USB port on its right edge.
    let c = rect.center();
    let screen = Rect::from_center_size(Pos2::new(c.x - 130.0, c.y - 20.0), egui::vec2(150.0, 100.0));
    painter.rect_stroke(screen, 6.0, stroke, egui::StrokeKind::Middle);
    let inner = screen.shrink(8.0);
    let glow = if connected { Color32::from_rgb(60, 25, 90) } else { Color32::from_gray(30) };
    painter.rect_filled(inner, 3.0, glow);
    if connected {
        // A tiny thermal image appears on the screen.
        let k = ((phase - 0.5) / 0.5).clamp(0.0, 1.0);
        painter.circle_filled(inner.center(), 6.0 + 18.0 * k, Color32::from_rgba_unmultiplied(255, 170, 40, (200.0 * k) as u8));
        painter.circle_filled(inner.center(), 4.0 + 6.0 * k, Color32::from_rgb(255, 240, 160));
    }
    let base = Rect::from_min_max(Pos2::new(screen.left() - 14.0, screen.bottom() + 4.0), Pos2::new(screen.right() + 14.0, screen.bottom() + 14.0));
    painter.rect_stroke(base, 3.0, stroke, egui::StrokeKind::Middle);
    let port = Pos2::new(base.right(), base.center().y);
    painter.rect_filled(Rect::from_center_size(port, egui::vec2(4.0, 6.0)), 1.0, line);

    // FLIR One Gen 3 (right): a small dark dongle, landscape, its USB-C
    // plug on the top edge; thermal and visible lenses side by side on the
    // left of the front, power button and LED on the right.
    let cam = Rect::from_center_size(Pos2::new(c.x + 150.0, c.y + 22.0), egui::vec2(118.0, 58.0));
    let body = Color32::from_gray(48);
    painter.rect_filled(cam, 12.0, body);
    painter.rect_stroke(cam, 12.0, Stroke::new(1.5, Color32::from_gray(120)), egui::StrokeKind::Middle);
    // Lighter front plate around the lenses.
    let plate = Rect::from_min_size(cam.min + egui::vec2(8.0, 8.0), egui::vec2(62.0, cam.height() - 16.0));
    painter.rect_filled(plate, 8.0, Color32::from_gray(30));
    let ir = Pos2::new(plate.left() + 19.0, plate.center().y);
    painter.circle_filled(ir, 13.0, Color32::from_gray(70));
    painter.circle_filled(ir, 9.0, Color32::from_rgb(25, 20, 35));
    painter.circle_filled(ir + egui::vec2(-3.0, -3.0), 2.5, Color32::from_white_alpha(60));
    let vis = Pos2::new(plate.left() + 46.0, plate.center().y);
    painter.circle_filled(vis, 6.0, Color32::from_gray(70));
    painter.circle_filled(vis, 3.5, Color32::from_rgb(20, 30, 45));
    // Power button on the top edge, LED beside it.
    let button = Rect::from_center_size(Pos2::new(cam.right() - 24.0, cam.top() - 1.5), egui::vec2(16.0, 4.0));
    painter.rect_filled(button, 2.0, Color32::from_gray(110));
    let led_on = connected && ((t * 6.0) as i32 % 2 == 0);
    let led = if led_on { Color32::from_rgb(60, 230, 90) } else { Color32::from_gray(80) };
    let led_pos = Pos2::new(cam.right() - 24.0, cam.top() + 12.0);
    if led_on {
        painter.circle_filled(led_pos, 6.0, Color32::from_rgba_unmultiplied(60, 230, 90, 50));
    }
    painter.circle_filled(led_pos, 3.0, led);
    // USB-C plug on the top edge, where the cable attaches.
    let conn = Rect::from_center_size(Pos2::new(cam.center().x - 8.0, cam.top() - 7.0), egui::vec2(12.0, 14.0));
    painter.rect_filled(conn, 3.0, Color32::from_gray(150));

    // Cable from the camera's plug to the laptop.
    let start = 120.0;
    let plug_x = port.x + 6.0 + start * (1.0 - travel);
    let cable_end = Pos2::new(plug_x + 14.0, port.y);
    let cam_out = Pos2::new(conn.center().x, conn.top());
    // The cable rises from the camera's plug and arcs over to the port.
    let curve = egui::epaint::CubicBezierShape::from_points_stroke(
        [cable_end, Pos2::new(cable_end.x + 60.0, cable_end.y), Pos2::new(cam_out.x, cam_out.y - 55.0), cam_out],
        false,
        Color32::TRANSPARENT,
        Stroke::new(3.0, line),
    );
    painter.add(curve);
    painter.rect_filled(Rect::from_min_size(Pos2::new(plug_x, port.y - 5.0), egui::vec2(16.0, 10.0)), 3.0, line);
    if !connected {
        // Arrow hinting where to plug.
        let a = Pos2::new(plug_x - 10.0, port.y - 18.0);
        painter.arrow(a, egui::vec2(-24.0, 0.0), Stroke::new(2.0, Color32::from_rgb(255, 170, 40)));
    }
}

/// `captures` in the working folder; on macOS, where an app starts in `/`,
/// `Documents/FLIR One PC` instead.
fn default_capture_root() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if cfg!(target_os = "macos") => PathBuf::from(home).join("Documents").join("FLIR One PC"),
        _ => PathBuf::from("captures"),
    }
}

/// Opens a folder in the system file manager.
fn open_folder(dir: &Path) -> std::io::Result<()> {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program).arg(dir).spawn().map(|_| ())
}

/// Creates `<root>/<date heure> - <nom>[ - suffix]`, numbered if it
/// already exists.
fn new_capture_dir(root: &Path, name: &str, suffix: Option<&str>) -> std::io::Result<PathBuf> {
    let mut base = chrono::Local::now().format("%Y-%m-%d %Hh%Mm%Ss").to_string();
    let name = sanitize(name);
    if !name.is_empty() {
        base.push_str(" - ");
        base.push_str(&name);
    }
    if let Some(s) = suffix {
        base.push_str(" - ");
        base.push_str(s);
    }
    let mut dir = root.join(&base);
    let mut n = 2;
    while dir.exists() {
        dir = root.join(format!("{base} ({n})"));
        n += 1;
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Removes characters Windows forbids in file names.
fn sanitize(name: &str) -> String {
    let cleaned: String =
        name.chars().map(|c| if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '-' } else { c }).collect();
    cleaned.trim().trim_end_matches('.').to_string()
}

/// CSV for a French Excel: UTF-8 with BOM, `;` separators, decimal commas.
fn csv_writer(path: &Path) -> std::io::Result<std::io::BufWriter<std::fs::File>> {
    let mut w = std::io::BufWriter::new(std::fs::File::create(path)?);
    w.write_all("\u{feff}".as_bytes())?;
    Ok(w)
}

fn excel_number(v: f32, decimals: usize) -> String {
    if v.is_finite() {
        format!("{v:.decimals$}").replace('.', ",")
    } else {
        String::new()
    }
}

/// Largest size with the given aspect ratio (w/h) that fits in `avail`.
fn fit(avail: egui::Vec2, aspect: f32) -> egui::Vec2 {
    let w = avail.x.min(avail.y * aspect).max(1.0);
    egui::vec2(w, w / aspect)
}

/// Draws a label next to `p` (up-right, flipped to stay inside the view).
fn label_near(painter: &egui::Painter, p: Pos2, r: f32, text: &str) {
    let size = painter.layout_no_wrap(text.to_owned(), FontId::proportional(15.0), Color32::WHITE).size();
    let clip = painter.clip_rect();
    let x = if p.x + r + size.x > clip.right() { p.x - r - size.x } else { p.x + r };
    let y = if p.y - r - size.y < clip.top() { p.y + r + size.y } else { p.y - r };
    outlined_text(painter, Pos2::new(x, y), text);
}

fn outlined_text(painter: &egui::Painter, pos: Pos2, text: &str) {
    let font = FontId::proportional(15.0);
    for d in [egui::vec2(1.0, 1.0), egui::vec2(-1.0, -1.0), egui::vec2(1.0, -1.0), egui::vec2(-1.0, 1.0)] {
        painter.text(pos + d, Align2::LEFT_BOTTOM, text, font.clone(), Color32::BLACK);
    }
    painter.text(pos, Align2::LEFT_BOTTOM, text, font, Color32::WHITE);
}

fn min_max(v: &[f32]) -> (f32, f32) {
    let (lo, hi) = v.iter().filter(|t| t.is_finite()).fold((f32::MAX, f32::MIN), |(a, b), &t| (a.min(t), b.max(t)));
    if lo > hi {
        (0.0, 0.0)
    } else {
        (lo, hi)
    }
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_camera(&ctx);
        if !ctx.egui_wants_keyboard_input() && ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.frozen = !self.frozen;
        }

        egui::Panel::left("controls").default_size(250.0).min_size(230.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.controls(ui, &ctx));
        });
        egui::CentralPanel::default().show(ui, |ui| {
            if self.control.is_paused() && !self.connected {
                self.released_screen(ui);
                return;
            }
            if !self.connected {
                self.waiting_screen(ui);
                return;
            }
            if self.frozen {
                ui.label(egui::RichText::new("Image figée (espace pour reprendre)").strong());
            }
            // Thermal and photo side by side on a wide window, stacked on a
            // tall one, whichever gives the larger images.
            let avail = ui.available_size();
            let gap = ui.spacing().item_spacing.x;
            let (tw, th) = self.size;
            let t_aspect = tw as f32 / th as f32;
            let p_aspect = self.visible_tex.as_ref().map_or(4.0 / 3.0, |t| t.aspect_ratio());
            if !self.settings.show_photo || self.visible_tex.is_none() {
                self.thermal_view(ui, fit(avail, t_aspect));
                return;
            }
            let side = (fit(egui::vec2((avail.x - gap) / 2.0, avail.y), t_aspect), fit(egui::vec2((avail.x - gap) / 2.0, avail.y), p_aspect));
            let stacked = (fit(egui::vec2(avail.x, (avail.y - gap) / 2.0), t_aspect), fit(egui::vec2(avail.x, (avail.y - gap) / 2.0), p_aspect));
            if side.0.x * side.0.y >= stacked.0.x * stacked.0.y {
                ui.horizontal_top(|ui| {
                    self.thermal_view(ui, side.0);
                    self.photo_view(ui, side.1);
                });
            } else {
                ui.vertical(|ui| {
                    self.thermal_view(ui, stacked.0);
                    self.photo_view(ui, stacked.1);
                });
            }
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(30));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }
}

impl Drop for Viewer {
    /// Closing the window mid-recording still leaves playable videos, and
    /// the camera is released so it is not left reserved.
    fn drop(&mut self) {
        self.stop_recording();
        self.control.shutdown(std::time::Duration::from_millis(1500));
    }
}

#[cfg(test)]
mod tests {
    use super::{excel_number, sanitize, Align};
    use eframe::egui::vec2;

    #[test]
    fn folder_names_keep_accents_and_drop_forbidden_characters() {
        assert_eq!(sanitize(" Dossier n°54 Xbox: Series/X. "), "Dossier n°54 Xbox- Series-X");
    }

    #[test]
    fn alignment_from_two_points() {
        // Photo = 0.5 + d + (thermal - 0.5) * s with s = 0.6, d = (0.05, -0.1).
        let map = |t: eframe::egui::Vec2| vec2(0.5, 0.5) + vec2(0.05, -0.1) + (t - vec2(0.5, 0.5)) * 0.6;
        let t1 = vec2(0.2, 0.3);
        let t2 = vec2(0.8, 0.7);
        let mut a = Align { thermal: vec![t1], photo: vec![map(t1)] };
        let (s, d) = a.solve(0.6);
        assert!((s - 0.6).abs() < 1e-5 && (d - vec2(0.05, -0.1)).length() < 1e-5);
        a.thermal.push(t2);
        a.photo.push(map(t2));
        let (s, d) = a.solve(1.0);
        assert!((s - 0.6).abs() < 1e-4, "{s}");
        assert!((d - vec2(0.05, -0.1)).length() < 1e-4, "{d:?}");
    }

    #[test]
    fn numbers_use_decimal_commas() {
        assert_eq!(excel_number(36.254, 2), "36,25");
        assert_eq!(excel_number(f32::NAN, 2), "");
    }
}
