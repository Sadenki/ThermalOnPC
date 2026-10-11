#[test]
fn extracts_calibration_from_camera_zip() {
    let cal = flir_usb::calibration_from_zip(include_bytes!("data/gen3-CameraFiles.zip")).unwrap();
    assert_eq!(cal.b, 1384.7);
}
