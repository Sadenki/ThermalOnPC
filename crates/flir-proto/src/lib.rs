//! Pure protocol logic for the FLIR One (Gen 2 / Gen 3 USB) camera.
//!
//! This crate does no I/O: it turns the byte stream read from the frame
//! endpoint (0x85) into frames, and raw sensor counts into temperatures.
//! Keeping it I/O-free means it can be unit tested without hardware and
//! reused by any transport (USB, recorded files, ...).

pub mod frame;
pub mod radiometry;

pub use frame::{Frame, FrameAssembler, FrameError, Header, Spotmeter, GEN3_HEIGHT, GEN3_WIDTH, THERMAL_HEIGHT, THERMAL_WIDTH};
pub use radiometry::{Calibration, ObjectParams, TemperatureLut, LEPTON_RAW_SCALE};
