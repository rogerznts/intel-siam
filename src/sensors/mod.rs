//! Sensors: regions of a source observed continuously, each producing a value of one kind.
//!
//! This module owns both ends of the app ↔ vision process boundary. The wire protocol and the
//! vision process are private submodules: code outside `sensors` cannot name a protocol message.

mod client;
#[cfg(test)]
mod client_tests;
mod types;
mod vision;
mod wire;

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

pub use types::*;
pub use client::Sensors;

pub fn vision_child(program: &Path, data: &Path, ffmpeg: &Path, logs: &Path) -> crate::procs::ChildSpec {
    crate::procs::ChildSpec {
        program: program.into(),
        args: vec![
            "--data".into(), data.into(),
            "--logs".into(), logs.join("vision").into(),
            "--ffmpeg".into(), ffmpeg.into(),
        ],
        io: crate::procs::ChildIo::Duplex,
        stall_after: wire::HEARTBEAT * 6,
    }
}

/// A neural network run on the CPU. The `intel-siam-vision` binary supplies the ONNX Runtime one,
/// so the library never names ONNX Runtime and the UI binary links none of it.
pub trait Network {
    /// One NCHW `f32` input; the first output's last dimension and its values.
    fn run(&mut self, input: Vec<f32>, shape: [usize; 4]) -> Result<(usize, Vec<f32>), String>;
}

/// Opens the verified model file at the path.
pub type LoadNetwork = fn(&Path) -> Result<Box<dyn Network>, String>;

/// The `intel-siam-vision` binary: `--data <dir> --logs <dir> --ffmpeg <path>`. Returns when stdin
/// closes. Nothing but protocol frames ever reaches its stdout.
pub fn run_vision(args: impl Iterator<Item = OsString>, load: LoadNetwork) -> ExitCode {
    vision::run(args, load)
}
