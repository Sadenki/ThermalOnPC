use flir_proto::frame::{FrameAssembler, HEADER_LEN, MAGIC};
use flir_proto::{Calibration, ObjectParams};

/// Builds a synthetic frame whose pixel (x, y) has value `y * 1000 + x`,
/// with rows of `stride` words split in two halves separated by 2 words.
fn synthetic_frame(width: usize, height: usize, stride: usize) -> Vec<u8> {
    let mut thermal = vec![0u8; 4 + stride * 2 * height];
    for y in 0..height {
        for x in 0..width {
            let gap = if x >= width / 2 { 2 } else { 0 };
            let at = 4 + 2 * (y * stride + x + gap);
            let v = (y * 1000 + x) as u16;
            thermal[at..at + 2].copy_from_slice(&v.to_le_bytes());
        }
    }
    let jpeg = b"\xFF\xD8fakejpeg\xFF\xD9".to_vec();
    let status = b"{\"shutterState\":\"ON\"}\0\0".to_vec();
    let payload = thermal.len() + jpeg.len() + status.len();

    let mut f = Vec::new();
    f.extend_from_slice(&MAGIC);
    f.extend_from_slice(&[0; 4]);
    for n in [payload, thermal.len(), jpeg.len(), status.len()] {
        f.extend_from_slice(&(n as u32).to_le_bytes());
    }
    f.extend_from_slice(&[0; 4]);
    assert_eq!(f.len(), HEADER_LEN);
    f.extend(thermal);
    f.extend(jpeg);
    f.extend(status);
    f
}

#[test]
fn decodes_gen2_frame_split_across_chunks() {
    let one = synthetic_frame(160, 120, 164);
    let mut stream = b"garbage".to_vec();
    stream.extend(&one);
    stream.extend(&one);

    let mut asm = FrameAssembler::new();
    let mut frames = Vec::new();
    for chunk in stream.chunks(16384) {
        frames.extend(asm.push(chunk));
    }
    assert_eq!(frames.len(), 2);
    assert_eq!(asm.skipped_bytes, 7);
    let f = frames.remove(0).unwrap();
    assert_eq!((f.width, f.height), (160, 120));
    assert_eq!(f.thermal[79], 79);
    assert_eq!(f.thermal[80], 80);
    assert_eq!(f.thermal[119 * 160 + 159], 119_159u32 as u16);
    assert!(f.jpeg.starts_with(b"\xFF\xD8"));
    assert_eq!(f.status_text(), "{\"shutterState\":\"ON\"}");
}

#[test]
fn decodes_gen3_frame_with_either_padding() {
    for stride in [82, 84] {
        let mut asm = FrameAssembler::new();
        let f = asm.push(&synthetic_frame(80, 60, stride)).remove(0).unwrap();
        assert_eq!((f.width, f.height), (80, 60));
        assert_eq!(f.thermal[39], 39);
        assert_eq!(f.thermal[40], 40);
        assert_eq!(f.thermal[59 * 80 + 79], 59_079);
    }
}

#[test]
fn radiometry_round_trips() {
    let cal = Calibration::default();
    let p = ObjectParams { emissivity: 1.0, reflected_temp_c: 20.0 };
    for t in [-10.0, 0.0, 25.0, 37.0, 100.0] {
        let raw = (cal.radiance(t) / cal.raw_scale).round() as u16;
        let back = cal.raw_to_celsius(raw, &p);
        assert!((back - t).abs() < 0.2, "{t} -> {raw} -> {back}");
    }
}

/// A real Gen 3 thermal block (camera pointed at a room, a hand in view).
#[test]
fn decodes_real_gen3_block() {
    let block = include_bytes!("data/gen3-thermal-block.bin");
    let px = flir_proto::frame::decode_thermal(block, 80, 60).unwrap();
    assert_eq!(px.len(), 80 * 60);
    // Every pixel is a plausible 14-bit sensor count: no packet headers or
    // telemetry leaked into the image.
    assert!(px.iter().all(|&v| (2000..8000).contains(&v)), "{:?}", px.iter().minmax());
}

trait MinMax {
    fn minmax(self) -> (u16, u16);
}
impl<'a, I: Iterator<Item = &'a u16>> MinMax for I {
    fn minmax(self) -> (u16, u16) {
        self.fold((u16::MAX, 0), |(a, b), &v| (a.min(v), b.max(v)))
    }
}

#[test]
fn reads_gen3_calibration_file() {
    let text = include_str!("data/gen3-calib.rsc");
    let cal = flir_proto::Calibration::from_rsc(text).expect("constants");
    // Active case ds120C_weRB_apNOA_fiNOF_leExFOL2.
    assert_eq!(cal.r1, 14331.136);
    assert_eq!(cal.b, 1384.7);
    assert_eq!(cal.o, 3776.0);
    assert!((cal.r2 - 1.0 / 107.27056).abs() < 1e-12);
}

/// The Gen 3 telemetry carries the Lepton's own reading of the centre
/// pixels; anchoring the conversion on it must give that reading back.
#[test]
fn lepton_spotmeter_anchors_conversion() {
    let block = include_bytes!("data/gen3-thermal-block.bin").to_vec();
    let thermal = flir_proto::frame::decode_thermal(&block, 80, 60).unwrap();
    let f = flir_proto::Frame { width: 80, height: 60, thermal, thermal_block: block, jpeg: vec![], status: vec![] };
    let spot = f.spotmeter().expect("telemetry with a spotmeter");
    assert!((15.0..40.0).contains(&spot.celsius), "{spot:?}");
    let cal = Calibration::default().anchored(spot.raw_mean, spot.celsius);
    let p = ObjectParams { emissivity: 1.0, reflected_temp_c: 20.0 };
    let raw = spot.raw_mean.round() as u16;
    let t = cal.raw_to_celsius(raw, &p);
    assert!((t - spot.celsius).abs() < 0.1, "{t} vs {spot:?}");
}
