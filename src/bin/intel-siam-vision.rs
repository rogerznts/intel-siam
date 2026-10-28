//! The vision process. ONNX Runtime lives here, outside the library, so the UI binary never links
//! it: the library reaches the network only through `sensors::Network`.

use std::path::Path;
use std::process::ExitCode;

use intel_siam::sensors::{self, Network};
use ort::logging::LogLevel;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor;

fn main() -> ExitCode {
    sensors::run_vision(std::env::args_os().skip(1), Onnx::load)
}

/// One CPU session, as the origin's `cpu_session`: no thread spinning while idle, only errors
/// logged.
struct Onnx {
    session: Session,
    output: String,
}

impl Onnx {
    fn load(model: &Path) -> Result<Box<dyn Network>, String> {
        let fail = |e: &dyn std::fmt::Display| e.to_string();
        let env = ort::init().with_name("intel-siam-vision").build().map_err(|e| fail(&e))?;
        let session = Session::builder(&env)
            .map_err(|e| fail(&e))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| fail(&e))?
            .with_intra_op_spinning(false)
            .map_err(|e| fail(&e))?
            .with_inter_op_spinning(false)
            .map_err(|e| fail(&e))?
            .with_log_level(LogLevel::Error)
            .map_err(|e| fail(&e))?
            .commit_from_file(model)
            .map_err(|e| fail(&e))?;
        let output = session.outputs().first().ok_or("the model has no output")?.name().to_owned();
        Ok(Box::new(Self { session, output }))
    }
}

impl Network for Onnx {
    fn run(&mut self, input: Vec<f32>, shape: [usize; 4]) -> Result<(usize, Vec<f32>), String> {
        let tensor = Tensor::from_array((shape, input)).map_err(|e| e.to_string())?;
        let outputs = self.session.run(ort::inputs![tensor]).map_err(|e| e.to_string())?;
        let (shape, values) = outputs[self.output.as_str()].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        let dim = shape.last().copied().and_then(|d| usize::try_from(d).ok()).ok_or("the output has no dimension")?;
        Ok((dim, values.to_vec()))
    }
}
