//! Conversion from raw sensor counts to temperature, using FLIR's Planck
//! calibration model (the same constants exiftool shows as `Planck*`).

/// Per-camera calibration constants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibration {
    pub r1: f64,
    pub r2: f64,
    pub b: f64,
    pub f: f64,
    pub o: f64,
    /// Multiplier applied to raw counts before the Planck formula. The
    /// FLIR One streams 14-bit values while the constants expect 16-bit.
    pub raw_scale: f64,
    /// Added to the scaled raw counts; set by [`Calibration::anchored`].
    pub raw_offset: f64,
}

/// Counts of the FLIR One Planck model per Lepton raw count, measured on a
/// Gen 3 by fitting the Lepton's own spotmeter readings (26–35 °C) against
/// the raw counts under it. The Gen 2 value of 4 read ~7 °C too hot.
pub const LEPTON_RAW_SCALE: f64 = 4.91;

impl Default for Calibration {
    /// Constants read from a FLIR One Gen 2 JPEG. A Gen 3 unit has its own
    /// values in its calibration files; until those are read from the
    /// camera, absolute temperatures are approximate.
    fn default() -> Self {
        Self { r1: 16528.178, r2: 0.012258549, b: 1427.5, f: 1.0, o: -1307.0, raw_scale: 4.0, raw_offset: 0.0 }
    }
}

impl Calibration {
    /// Reads the constants from a FLIR `calib.rsc` file (found in the
    /// camera's `CameraFiles.zip`). The active calibration case is
    /// `.calib.ccase.0.ID`; its `.calib.extRBF.<ID>` entry holds the
    /// R, B, F, J0, J1 constants of the model `raw = J1·R/(e^(B/T) − F) + J0`,
    /// which in exiftool's `Planck*` terms is R1 = R, R2 = 1/J1, O = −J0.
    pub fn from_rsc(text: &str) -> Option<Self> {
        let get = |key: &str| -> Option<&str> {
            text.lines().find_map(|l| {
                let mut parts = l.trim().splitn(3, ' ');
                (parts.next()? == key).then(|| ())?;
                parts.next()?;
                Some(parts.next()?.trim().trim_matches('"'))
            })
        };
        let id = get(".calib.ccase.0.ID")?;
        let num = |name: &str| get(&format!(".calib.extRBF.{id}.{name}"))?.parse::<f64>().ok();
        let (r, b, f, j0, j1) = (num("R")?, num("B")?, num("F")?, num("J0")?, num("J1")?);
        Some(Self { r1: r, r2: 1.0 / j1, b, f, o: -j0, raw_scale: 4.0, raw_offset: 0.0 })
    }
}

/// Properties of the scene that affect the measured temperature.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectParams {
    pub emissivity: f64,
    pub reflected_temp_c: f64,
}

impl Default for ObjectParams {
    fn default() -> Self {
        Self { emissivity: 0.95, reflected_temp_c: 20.0 }
    }
}

impl Calibration {
    /// Radiance in counts (16-bit scale) emitted by a black body at `t_c`.
    pub fn radiance(&self, t_c: f64) -> f64 {
        self.r1 / (self.r2 * ((self.b / (t_c + 273.15)).exp() - self.f)) - self.o
    }

    /// The same constants, shifted so that `raw_mean` reads `t_c` (a black
    /// body temperature, e.g. the Lepton's spotmeter). Raw counts drift with
    /// the sensor temperature and jump at each shutter calibration; a
    /// reference measured in the same frame cancels both.
    pub fn anchored(&self, raw_mean: f64, t_c: f64) -> Self {
        let raw_scale = LEPTON_RAW_SCALE;
        Self { raw_scale, raw_offset: self.radiance(t_c) - raw_scale * raw_mean, ..*self }
    }

    /// Temperature in °C of an object seen with raw count `raw`.
    pub fn raw_to_celsius(&self, raw: u16, p: &ObjectParams) -> f64 {
        let s = raw as f64 * self.raw_scale + self.raw_offset;
        let refl = self.radiance(p.reflected_temp_c);
        let obj = (s - (1.0 - p.emissivity) * refl) / p.emissivity;
        self.b / (self.r1 / (self.r2 * (obj + self.o)) + self.f).ln() - 273.15
    }
}

/// Precomputed raw→°C table: building it costs ~65k evaluations, after which
/// converting a whole 160x120 frame is a table lookup per pixel.
pub struct TemperatureLut {
    table: Vec<f32>,
}

impl TemperatureLut {
    pub fn new(cal: &Calibration, p: &ObjectParams) -> Self {
        let table = (0..=u16::MAX).map(|raw| cal.raw_to_celsius(raw, p) as f32).collect();
        Self { table }
    }

    /// Returns NaN for counts outside the model's valid range.
    #[inline]
    pub fn celsius(&self, raw: u16) -> f32 {
        self.table[raw as usize]
    }
}
