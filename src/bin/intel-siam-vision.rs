//! The vision process. ONNX Runtime lives here, outside the library, so the UI binary never links
//! it: the library reaches the network only through `sensors::Network`.

use std::path::Path;
use std::process::ExitCode;
use std::sync::LazyLock;

use intel_siam::sensors::{self, Network, Output, Tensor as Input};
use ort::environment::Environment;
use ort::logging::LogLevel;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor;

/// ONNX Runtime allows one environment per process; every model's session shares it.
static ENVIRONMENT: LazyLock<Result<Environment, String>> =
    LazyLock::new(|| ort::init().with_name("intel-siam-vision").build().map_err(|e| e.to_string()));

fn main() -> ExitCode {
    sensors::run_vision(std::env::args_os().skip(1), Onnx::load)
}

/// One CPU session, as the origin's `cpu_session`: no thread spinning while idle, only errors
/// logged.
struct Onnx {
    session: Session,
    inputs: Vec<String>,
    outputs: Vec<String>,
}

impl Onnx {
    fn load(model: &Path) -> Result<Box<dyn Network>, String> {
        let fail = |e: &dyn std::fmt::Display| e.to_string();
        let env = ENVIRONMENT.as_ref().map_err(Clone::clone)?;
        let session = Session::builder(env)
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
        let inputs: Vec<String> = session.inputs().iter().map(|i| i.name().to_owned()).collect();
        let outputs: Vec<String> = session.outputs().iter().map(|o| o.name().to_owned()).collect();
        if outputs.is_empty() {
            return Err("the model has no output".into());
        }
        Ok(Box::new(Self { session, inputs, outputs }))
    }
}

impl Network for Onnx {
    fn run(&mut self, inputs: Vec<Input>) -> Result<Vec<Output>, String> {
        if inputs.len() != self.inputs.len() {
            return Err(format!("the model takes {} inputs, not {}", self.inputs.len(), inputs.len()));
        }
        let feed = self
            .inputs
            .iter()
            .zip(inputs)
            .map(|(name, input)| Ok((name.clone(), Tensor::from_array((input.shape, input.values)).map_err(|e| e.to_string())?)))
            .collect::<Result<Vec<_>, String>>()?;
        let results = self.session.run(feed).map_err(|e| e.to_string())?;
        self.outputs
            .iter()
            .map(|name| {
                let (shape, values) = results[name.as_str()].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
                let shape = shape.iter().map(|d| usize::try_from(*d).map_err(|_| "negative dimension".to_owned())).collect::<Result<_, _>>()?;
                Ok(Output { shape, values: values.to_vec() })
            })
            .collect()
    }
}
