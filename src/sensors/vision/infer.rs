//! Models and the CPU-heavy work, off the engine thread: the model registry, its download,
//! embedding a region with the backbone, training the per-sensor head, and finding objects with a
//! detector. The networks themselves run through the `Network` the binary supplies.
//!
//! The head is multinomial logistic regression on backbone embeddings (origin `classifier.py`):
//! scikit-learn's `LogisticRegression(C, class_weight="balanced")`, L2 on the weights, not the
//! intercept; one weight vector for two states, a softmax for more. The problem is convex, so any
//! solver that reaches its optimum gives the same head. `fit`, `Head::predict`, `embedding` and
//! `postprocess` are pure.
//!
//! One worker thread runs `Job`s in order; one fetch thread downloads models.

use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use egui::ColorImage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Input;
use super::logic::{self, CONTEXT_MARGIN, Detection, ObjectFilter};
use super::pixels::{self, Rgb};
use super::store::{TaughtSample, TrainRow, hex};
use super::teach::{self, TEACH, TaughtIndex, TaughtRow};
use crate::sensors::types::{
    ClassSet, Detector, Found, LabelId, MinSize, ModelState, ObjectClass, Region, SampleId, SensorId, StateKey,
    StateSet, Unit,
};
use crate::sensors::{LoadNetwork, Network, Output};

/// A model (origin `backbones.json`, `detectors.json`). Downloaded on first need into
/// `<data>/models/<sha256>`, verified, and renamed into place, so a file there is always complete.
pub struct ModelSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
    /// Side of the square input.
    pub input: usize,
    /// Per channel, after scaling to `0.0..=1.0`.
    pub mean: [f32; 3],
    pub std: [f32; 3],
}

/// The backbone registry's default entry, `dinov2-small-q8`, copied from `backbones.json`.
pub const BACKBONE: ModelSpec = ModelSpec {
    id: "dinov2-small-q8",
    name: "DINOv2 small · 8-bit",
    url: "https://huggingface.co/onnx-community/dinov2-small/resolve/8b1f705a3a7f6f062f6bdd21986c1583d3ef105d/onnx/model_uint8.onnx",
    sha256: "c179f8f7f592449c4c1bca4cd124a7538021428c5ffb89afde9503935b197efb",
    size: 24_446_700,
    input: 224,
    mean: [0.485, 0.456, 0.406],
    std: [0.229, 0.224, 0.225],
};

/// A `detectors.json` entry: a 640 square scaled to `0.0..=1.0`, no mean or deviation.
const fn detector(which: Detector, url: &'static str, sha256: &'static str, size: u64) -> ModelSpec {
    ModelSpec { id: which.id(), name: which.name(), url, sha256, size, input: 640, mean: [0.0; 3], std: [1.0; 3] }
}

const DFINE_N: ModelSpec = detector(
    Detector::DfineN,
    "https://huggingface.co/onnx-community/dfine-nano-coco-ONNX/resolve/89df95b0e8575ff9879f13351bafe69bbe8e6823/onnx/model.onnx",
    "f7febe9695cd6f71120abbeb0642d3d6b6c6bcc36f0ba1d79b66d302b5860814",
    15_434_994,
);
const DFINE_S: ModelSpec = detector(
    Detector::DfineS,
    "https://huggingface.co/onnx-community/dfine_s_coco-ONNX/resolve/a3cf03147a9b86c78475139115c8ac142577352d/onnx/model.onnx",
    "cd8a49a945feda6d28c6304ae8ae85c2759ba1d78a5a83a22c5ce8db82ef7238",
    41_535_197,
);
const DFINE_M: ModelSpec = detector(
    Detector::DfineM,
    "https://huggingface.co/onnx-community/dfine_m_coco-ONNX/resolve/489756db2825cb068a588fe930af239a656d1fe1/onnx/model.onnx",
    "70aaa837978a06ba44ad17398c7079ae5a1a7b1a9032b5d7053981e1ada02d6b",
    78_624_257,
);
const DFINE_L: ModelSpec = detector(
    Detector::DfineL,
    "https://huggingface.co/onnx-community/dfine_l_coco-ONNX/resolve/e09b218185400510b68d067bbd0c9379d905ffcd/onnx/model.onnx",
    "d678f3baebfb909d3a20f21d1d807544d0172ed47fa1ab88e7fcdec7e365b236",
    125_348_332,
);

/// Every model the vision process may need. New models are new variants and constants here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Model {
    Backbone,
    Detector(Detector),
}

impl Model {
    pub fn spec(self) -> &'static ModelSpec {
        match self {
            Self::Backbone => &BACKBONE,
            Self::Detector(Detector::DfineN) => &DFINE_N,
            Self::Detector(Detector::DfineS) => &DFINE_S,
            Self::Detector(Detector::DfineM) => &DFINE_M,
            Self::Detector(Detector::DfineL) => &DFINE_L,
        }
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
const NMS_IOU: f32 = 0.7; // DETECTION["nms_iou"]
const MAX_DETECTIONS: usize = 100; // DETECTION["max_detections"]

pub enum Job {
    /// One checked region → one vector.
    Embed { sensor: SensorId, crop: Rgb },
    /// Embeds the samples without a cached vector (cropped with `region`), then fits.
    Train { sensor: SensorId, region: Region, rows: Vec<TrainRow> },
    /// Embeds the taught boxes without a cached vector, then builds the index to compare with.
    Index { sensor: SensorId, rows: Vec<TaughtSample>, parents: Vec<(LabelId, ObjectClass)> },
    /// One check of an object sensor: detect in the region, then compare with the taught boxes.
    Detect { sensor: SensorId, frame: Arc<ColorImage>, request: DetectRequest },
}

/// What a check of an object sensor needs from its settings.
pub struct DetectRequest {
    pub detector: Detector,
    pub region: Region,
    pub classes: ClassSet,
    pub threshold: Unit,
    pub min_size: MinSize,
    pub taught: Option<Compare>,
}

/// The taught boxes to compare with, and the last check's boxes (an object that stayed keeps its
/// answer).
pub struct Compare {
    pub index: Arc<TaughtIndex>,
    pub previous: Vec<Found>,
}

pub enum Done {
    Embedded { sensor: SensorId, vector: Result<Vec<f32>, String> },
    /// `region` is the hash the job was cropped with.
    Trained { sensor: SensorId, region: u64, outcome: Result<Trained, String> },
    Indexed { sensor: SensorId, outcome: Result<Indexed, String> },
    Detected { sensor: SensorId, found: Result<Vec<Found>, String> },
}

pub struct Trained {
    /// `None`: fewer than two states have samples.
    pub head: Option<Head>,
    /// Embeddings computed for this job, to cache.
    pub fresh: Vec<(SampleId, Vec<f32>)>,
}

pub struct Indexed {
    /// `None`: no taught box means anything now.
    pub index: Option<TaughtIndex>,
    pub fresh: Vec<(SampleId, Vec<f32>)>,
}

/// A model's download or verification moved on.
pub struct ModelEvent {
    pub model: Model,
    pub state: ModelState,
}

pub struct Infer {
    jobs: Sender<Job>,
    fetch: Sender<Model>,
    worker: Option<JoinHandle<()>>,
}

impl Infer {
    /// The engine submits only once the job's models are ready.
    pub fn submit(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    /// Idempotent: a model on disk is verified once per process, never downloaded again. The
    /// answer arrives as `Input::Model`.
    pub fn ensure(&self, model: Model) {
        let _ = self.fetch.send(model);
    }
}

#[cfg(test)]
pub(super) fn test_channels() -> (Infer, Receiver<Job>, Receiver<Model>) {
    let (jobs, job_rx) = mpsc::channel();
    let (fetch, fetch_rx) = mpsc::channel();
    (Infer { jobs, fetch, worker: None }, job_rx, fetch_rx)
}

/// The worker finishes its job before the process exits: ONNX Runtime's static destructors run at
/// exit and fail an inference still in flight.
impl Drop for Infer {
    fn drop(&mut self) {
        drop(std::mem::replace(&mut self.jobs, mpsc::channel().0));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn spawn(dir: PathBuf, engine: Sender<Input>, load: LoadNetwork) -> Infer {
    let (jobs, job_rx) = mpsc::channel();
    let (fetch, fetch_rx) = mpsc::channel();
    let worker = {
        let (dir, engine) = (dir.clone(), engine.clone());
        thread::spawn(move || work(Networks { dir, load, backbone: None, detector: None }, job_rx, &engine))
    };
    thread::spawn(move || fetch_models(&dir, fetch_rx, &engine));
    Infer { jobs, fetch, worker: Some(worker) }
}

/// The worker's networks, each loaded on first use.
struct Networks {
    dir: PathBuf,
    load: LoadNetwork,
    backbone: Option<Box<dyn Network>>,
    detector: Option<(Detector, Box<dyn Network>)>,
}

impl Networks {
    fn backbone(&mut self) -> Result<&mut dyn Network, String> {
        if self.backbone.is_none() {
            self.backbone = Some(self.open(Model::Backbone)?);
        }
        Ok(self.backbone.as_deref_mut().expect("just loaded"))
    }

    /// Another detector than the loaded one replaces it.
    fn detector(&mut self, which: Detector) -> Result<&mut dyn Network, String> {
        if self.detector.as_ref().is_none_or(|(d, _)| *d != which) {
            self.detector = None;
            self.detector = Some((which, self.open(Model::Detector(which))?));
        }
        Ok(self.detector.as_mut().map(|(_, n)| n.as_mut()).expect("just loaded"))
    }

    fn open(&self, model: Model) -> Result<Box<dyn Network>, String> {
        (self.load)(&self.dir.join(model.spec().sha256)).map_err(|e| format!("modelo: {e}"))
    }
}

fn work(mut networks: Networks, jobs: Receiver<Job>, engine: &Sender<Input>) {
    for job in jobs {
        let done = match job {
            Job::Embed { sensor, crop } => {
                Done::Embedded { sensor, vector: networks.backbone().and_then(|n| embed(n, &crop)) }
            }
            Job::Train { sensor, region, rows } => Done::Trained {
                sensor,
                region: region.hash(),
                outcome: networks.backbone().and_then(|n| train(n, &region, rows)),
            },
            Job::Index { sensor, rows, parents } => {
                Done::Indexed { sensor, outcome: networks.backbone().and_then(|n| index(n, rows, parents)) }
            }
            Job::Detect { sensor, frame, request } => {
                Done::Detected { sensor, found: detect(&mut networks, &frame, &request) }
            }
        };
        if engine.send(Input::Infer(done)).is_err() {
            return;
        }
    }
}

fn train(network: &mut dyn Network, region: &Region, rows: Vec<TrainRow>) -> Result<Trained, String> {
    let (mut vectors, mut labels, mut fresh) = (Vec::new(), Vec::new(), Vec::new());
    for row in rows {
        let vector = match row.cached {
            Some(vector) => vector,
            None => {
                let frame = match read_image(&row.image) {
                    Ok(frame) => frame,
                    Err(e) => {
                        eprintln!("sample {} skipped: {e}", row.sample.0);
                        continue;
                    }
                };
                let vector = embed(network, &pixels::crop(&frame, region))?;
                fresh.push((row.sample, vector.clone()));
                vector
            }
        };
        vectors.push(vector);
        labels.push(row.label);
    }
    Ok(Trained { head: fit(&vectors, &labels), fresh })
}

/// The stored crops are embedded alone, one at a time, as the boxes of a check are: the 8-bit
/// backbone quantises a batch together, so the same crop would compare differently in a batch
/// (origin `teach.embed`). A box whose image is unreadable is left out (origin issue #45).
fn index(network: &mut dyn Network, rows: Vec<TaughtSample>, parents: Vec<(LabelId, ObjectClass)>) -> Result<Indexed, String> {
    let (mut taught, mut fresh) = (Vec::new(), Vec::new());
    for row in rows {
        let vector = match row.cached {
            Some(vector) => vector,
            None => {
                let crop = match read_image(&row.image) {
                    Ok(crop) => crop,
                    Err(e) => {
                        eprintln!("taught box {} skipped: {e}", row.sample.0);
                        continue;
                    }
                };
                let vector = embed(network, &Rgb::from(&crop))?;
                fresh.push((row.sample, vector.clone()));
                vector
            }
        };
        taught.push(TaughtRow { sample: row.sample, label: row.label, detected: row.detected, vector });
    }
    Ok(Indexed { index: teach::build(taught, parents.into_iter().collect()), fresh })
}

/// One check of an object sensor (origin `_run_objects` up to the tracks): the detector sees the
/// region with a margin around it, the boxes that count are kept, then compared with the taught
/// ones. Once a box the detector missed was taught, its unsure boxes are looked at too.
fn detect(networks: &mut Networks, frame: &ColorImage, request: &DetectRequest) -> Result<Vec<Found>, String> {
    let analysed = logic::region_box(&request.region, CONTEXT_MARGIN);
    let rescue = request.taught.as_ref().is_some_and(|t| t.index.rescue);
    let threshold = f32::from(request.threshold);
    let floor = if rescue { threshold.min(TEACH.rescue_floor) } else { threshold };
    let spec = Model::Detector(request.detector).spec();
    let input = tensor(&pixels::stretch(&pixels::crop_box(frame, analysed), spec.input), spec);
    let outputs = networks.detector(request.detector)?.run(input, [1, 3, spec.input, spec.input]).map_err(|e| format!("detector: {e}"))?;
    let raw = postprocess(&outputs, floor)?;
    let filter = ObjectFilter { region: &request.region, classes: &request.classes, min_size: request.min_size };
    let found = logic::filter_detections(&raw, analysed, &filter, rescue);
    let Some(compare) = &request.taught else { return Ok(found) };
    let (found, candidates): (Vec<Found>, Vec<Found>) = if rescue {
        let (sure, unsure): (Vec<Found>, Vec<Found>) = found.into_iter().partition(|f| f.score >= threshold);
        (sure.into_iter().filter(|f| request.classes.contains(f.class)).collect(), unsure)
    } else {
        (found, Vec::new())
    };
    let (checked, rescued) = teach::plan(&found, &candidates, &compare.index);
    let backbone = networks.backbone()?;
    let mut vector = |area: [f32; 4]| embed(backbone, &pixels::crop_box(frame, teach::crop_area(area)));
    let checked = checked.into_iter().map(|i| Ok((i, vector(found[i].area)?))).collect::<Result<Vec<_>, String>>()?;
    let rescued = rescued.into_iter().map(|c| Ok((c, vector(c.area)?))).collect::<Result<Vec<_>, String>>()?;
    Ok(teach::apply(&found, &checked, &rescued, &compare.index, &request.classes, &compare.previous))
}

/// Normalised with the model's mean and deviation, channels first.
fn tensor(square: &Rgb, spec: &ModelSpec) -> Vec<f32> {
    let plane = square.size[0] * square.size[1];
    let mut input = vec![0.0f32; 3 * plane];
    for (i, rgb) in square.pixels.as_chunks::<3>().0.iter().enumerate() {
        for c in 0..3 {
            input[c * plane + i] = (f32::from(rgb[c]) / 255.0 - spec.mean[c]) / spec.std[c];
        }
    }
    input
}

/// DETR-style outputs (`logits`: queries × classes, `pred_boxes`: queries × centre/size, both
/// normalised to the image shown) as detections: every query and class above `min_score`, then
/// boxes of one class overlapping more than `NMS_IOU` merged into the most certain, most certain
/// first (origin `postprocess`).
pub fn postprocess(outputs: &[Output], min_score: f32) -> Result<Vec<Detection>, String> {
    let [logits, boxes, ..] = outputs else { return Err("o detector não devolveu caixas".into()) };
    let classes = crate::sensors::COCO.len();
    let queries = boxes.values.len() / 4;
    if logits.shape.last() != Some(&classes) || boxes.shape.last() != Some(&4) || logits.values.len() != queries * classes {
        return Err(format!("saída do detector com forma inesperada: {:?} e {:?}", logits.shape, boxes.shape));
    }
    let mut candidates: Vec<Detection> = Vec::new();
    for (q, scores) in logits.values.chunks_exact(classes).enumerate() {
        let [cx, cy, w, h] = boxes.values[q * 4..q * 4 + 4] else { unreachable!("four values per box") };
        let area = [cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0].map(|v| v.clamp(0.0, 1.0));
        for (c, logit) in scores.iter().enumerate() {
            let score = 1.0 / (1.0 + (-logit).exp());
            if score > min_score {
                let class = ObjectClass::from_index(c).expect("one score per class of the set");
                candidates.push(Detection { class, score, area });
            }
        }
    }
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Detection> = Vec::new();
    for cand in candidates {
        if !kept.iter().any(|k| k.class == cand.class && teach::box_iou(k.area, cand.area) > NMS_IOU) {
            kept.push(cand);
        }
    }
    kept.truncate(MAX_DETECTIONS);
    Ok(kept)
}

fn read_image(path: &Path) -> Result<ColorImage, String> {
    fs::read(path).map_err(|e| e.to_string()).and_then(|b| pixels::decode(&b).map_err(|e| e.to_string()))
}

/// Letterboxed to the input square, normalised, channels first (origin `Embedder.preprocess`), then
/// pooled into the vector a head sees.
fn embed(network: &mut dyn Network, crop: &Rgb) -> Result<Vec<f32>, String> {
    let side = BACKBONE.input;
    let input = tensor(&pixels::letterbox(crop, side), &BACKBONE);
    let outputs = network.run(input, [1, 3, side, side]).map_err(|e| format!("inferência: {e}"))?;
    let hidden = outputs.first().ok_or("o modelo não devolveu saída")?;
    let dim = hidden.shape.last().copied().unwrap_or(0);
    if dim == 0 || hidden.values.len() % dim != 0 {
        return Err("saída do modelo com forma inesperada".into());
    }
    Ok(embedding(&hidden.values, dim))
}

/// Requests for one model while it downloads are answered by the same download.
fn fetch_models(dir: &Path, requests: Receiver<Model>, engine: &Sender<Input>) {
    let mut ready: BTreeSet<Model> = BTreeSet::new();
    while let Ok(first) = requests.recv() {
        let pending: BTreeSet<Model> = std::iter::once(first).chain(requests.try_iter()).collect();
        for model in pending {
            let progress = |done, total| {
                let _ = engine.send(Input::Model(ModelEvent { model, state: ModelState::Downloading { done, total } }));
            };
            let state = if ready.contains(&model) {
                ModelState::Ready
            } else {
                match ensure(model.spec(), &dir.join(model.spec().sha256), &progress) {
                    Ok(()) => {
                        ready.insert(model);
                        ModelState::Ready
                    }
                    Err(e) => ModelState::Failed(e),
                }
            };
            if engine.send(Input::Model(ModelEvent { model, state })).is_err() {
                return;
            }
        }
    }
}

fn ensure(spec: &ModelSpec, path: &Path, progress: &dyn Fn(u64, u64)) -> Result<(), String> {
    if path.is_file() {
        if sha256_file(path).map_err(|e| e.to_string())? == spec.sha256 {
            return Ok(());
        }
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    download(spec, path, progress)
}

/// Into `<path>.part`, hashing as it arrives; renamed only when the SHA-256 matches.
fn download(spec: &ModelSpec, path: &Path, progress: &dyn Fn(u64, u64)) -> Result<(), String> {
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .build()
        .new_agent();
    let response = agent.get(spec.url).call().map_err(|e| format!("download: {e}"))?;
    let mut body = response.into_body().into_reader();
    let part = path.with_extension("part");
    let fail = |e: io::Error| format!("download: {e}");
    fs::create_dir_all(path.parent().expect("models have a folder")).map_err(fail)?;
    let mut file = File::create(&part).map_err(fail)?;
    let mut hash = Sha256::new();
    let (mut done, mut percent) = (0u64, 0u64);
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = body.read(&mut buf).map_err(fail)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(fail)?;
        hash.update(&buf[..n]);
        done += n as u64;
        if done * 100 / spec.size.max(1) > percent {
            percent = done * 100 / spec.size.max(1);
            progress(done, spec.size);
        }
    }
    file.sync_all().map_err(fail)?;
    drop(file);
    let digest = hex(&hash.finalize());
    if digest != spec.sha256 {
        let _ = fs::remove_file(&part);
        return Err(format!("o SHA-256 do modelo {} não confere: {digest}", spec.id));
    }
    fs::rename(&part, path).map_err(fail)
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut hash = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hash)?;
    Ok(hex(&hash.finalize()))
}

pub const CLASSIFIER_C: f64 = 30.0; // CLASSIFIER["C"]
pub const CLASSIFIER_MAX_ITER: usize = 3000; // CLASSIFIER["max_iter"]
pub const CLASSIFIER_TOL: f64 = 1e-6; // CLASSIFIER["tol"]
const LBFGS_MEMORY: usize = 10; // classifier._lbfgs(memory=10)

/// The backbone's output for one image as the vector a head sees: the class token concatenated
/// with the mean of the patch tokens, L2-normalised (pooling "cls_mean"). `hidden` holds the
/// tokens of one image, `dim` values each; a model that pools by itself gives one token.
pub fn embedding(hidden: &[f32], dim: usize) -> Vec<f32> {
    let tokens: Vec<&[f32]> = hidden.chunks_exact(dim).collect();
    let mut features = tokens[0].to_vec();
    if let [_, patches @ ..] = tokens.as_slice()
        && !patches.is_empty()
    {
        features.extend((0..dim).map(|d| patches.iter().map(|t| t[d]).sum::<f32>() / patches.len() as f32));
    }
    let norm = features.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-8);
    features.iter().map(|v| v / norm).collect()
}

/// A trained head. Valid only for the backbone and region its samples were embedded with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Head {
    /// Sorted; the class order of `coef` and `intercept`.
    keys: Vec<StateKey>,
    dim: usize,
    /// Row-major, `rows × dim`. One row for two keys, the second key being the positive class.
    coef: Vec<f64>,
    intercept: Vec<f64>,
}

impl Head {
    /// One probability per state of `states`; a state the head was not trained on gets 0.
    pub fn predict(&self, vector: &[f32], states: &StateSet) -> Vec<(StateKey, f32)> {
        let x: Vec<f64> = vector.iter().map(|v| f64::from(*v)).collect();
        let probabilities = probabilities(&self.scores(&x));
        states
            .iter()
            .map(|s| (s.key, self.keys.iter().position(|k| *k == s.key).map_or(0.0, |i| probabilities[i] as f32)))
            .collect()
    }

    fn scores(&self, x: &[f64]) -> Vec<f64> {
        self.coef.chunks_exact(self.dim).zip(&self.intercept).map(|(w, b)| dot(w, x) + b).collect()
    }
}

fn probabilities(scores: &[f64]) -> Vec<f64> {
    if let [z] = scores {
        let p = 0.5 * (1.0 + (0.5 * z).tanh()); // the logistic function, without overflow
        return vec![1.0 - p, p];
    }
    let top = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let e: Vec<f64> = scores.iter().map(|z| (z - top).exp()).collect();
    let total: f64 = e.iter().sum();
    e.iter().map(|v| v / total).collect()
}

/// Trains a head on embeddings with their labels (origin `classifier.fit`). `None` with fewer than
/// two distinct labels.
pub fn fit(vectors: &[Vec<f32>], labels: &[StateKey]) -> Option<Head> {
    let mut keys = labels.to_vec();
    keys.sort();
    keys.dedup();
    if keys.len() < 2 {
        return None;
    }
    let n = labels.len();
    let dim = vectors[0].len();
    let target: Vec<usize> = labels.iter().map(|l| keys.partition_point(|k| k < l)).collect();
    let mut count = vec![0usize; keys.len()];
    target.iter().for_each(|t| count[*t] += 1);
    let problem = Problem {
        x: vectors.iter().flatten().map(|v| f64::from(*v)).collect(),
        // class_weight="balanced": every class weighs as much in total; then the mean over samples.
        weight: target.iter().map(|t| 1.0 / (keys.len() * count[*t]) as f64).collect(),
        target,
        dim,
        rows: if keys.len() == 2 { 1 } else { keys.len() },
        alpha: 1.0 / (CLASSIFIER_C * n as f64),
    };
    let mut coef = lbfgs(|theta| problem.objective(theta), vec![0.0; problem.rows * (dim + 1)]);
    let intercept = coef.split_off(problem.rows * dim);
    Some(Head { keys, dim, coef, intercept })
}

struct Problem {
    /// Row-major, one sample per row.
    x: Vec<f64>,
    target: Vec<usize>,
    weight: Vec<f64>,
    dim: usize,
    rows: usize,
    alpha: f64,
}

impl Problem {
    /// Weighted mean log loss plus `alpha / 2 · |coef|²`, and its gradient (origin `_objective`).
    /// `theta` is the coefficients, row-major, then the intercepts.
    fn objective(&self, theta: &[f64]) -> (f64, Vec<f64>) {
        let (coef, intercept) = theta.split_at(self.rows * self.dim);
        let mut value = 0.5 * self.alpha * dot(coef, coef);
        let mut grad = vec![0.0; theta.len()];
        let (grad_coef, grad_intercept) = grad.split_at_mut(self.rows * self.dim);
        let mut diff = vec![0.0; self.rows];
        for ((x, t), w) in self.x.chunks_exact(self.dim).zip(&self.target).zip(&self.weight) {
            let z: Vec<f64> = coef.chunks_exact(self.dim).zip(intercept).map(|(c, b)| dot(c, x) + b).collect();
            let loss = if let [z] = z[..] {
                let t = *t as f64;
                diff[0] = 0.5 * (1.0 + (0.5 * z).tanh()) - t;
                z.max(0.0) + (-z.abs()).exp().ln_1p() - t * z
            } else {
                let top = z.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let lse = top + z.iter().map(|v| (v - top).exp()).sum::<f64>().ln();
                for (d, v) in diff.iter_mut().zip(&z) {
                    *d = (v - lse).exp();
                }
                diff[*t] -= 1.0;
                lse - z[*t]
            };
            value += w * loss;
            for ((g, gb), d) in grad_coef.chunks_exact_mut(self.dim).zip(grad_intercept.iter_mut()).zip(&diff) {
                let scaled = d * w;
                g.iter_mut().zip(x).for_each(|(g, x)| *g += scaled * x);
                *gb += scaled;
            }
        }
        grad_coef.iter_mut().zip(coef).for_each(|(g, c)| *g += self.alpha * c);
        (value, grad)
    }
}

/// Minimises a smooth convex function: L-BFGS with a backtracking (Armijo) line search, step for
/// step as the origin's `_lbfgs`.
fn lbfgs(f: impl Fn(&[f64]) -> (f64, Vec<f64>), mut theta: Vec<f64>) -> Vec<f64> {
    let (mut value, mut grad) = f(&theta);
    let mut steps: VecDeque<(Vec<f64>, Vec<f64>, f64)> = VecDeque::new();
    for _ in 0..CLASSIFIER_MAX_ITER {
        if grad.iter().all(|g| g.abs() <= CLASSIFIER_TOL) {
            break;
        }
        let mut q = grad.clone();
        let mut alphas = Vec::with_capacity(steps.len());
        for (s, y, rho) in steps.iter().rev() {
            let a = rho * dot(s, &q);
            alphas.push(a);
            axpy(-a, y, &mut q);
        }
        let scale = match steps.back() {
            Some((s, y, _)) => dot(s, y) / dot(y, y),
            None => 1.0 / dot(&grad, &grad).sqrt().max(1.0),
        };
        q.iter_mut().for_each(|v| *v *= scale);
        for ((s, y, rho), a) in steps.iter().zip(alphas.iter().rev()) {
            let b = rho * dot(y, &q);
            axpy(a - b, s, &mut q);
        }
        let mut direction: Vec<f64> = q.iter().map(|v| -v).collect();
        let mut slope = dot(&grad, &direction);
        if slope >= 0.0 {
            // Not a descent direction (rounding): start over from the gradient.
            steps.clear();
            direction = grad.iter().map(|g| -g).collect();
            slope = -dot(&grad, &grad);
        }
        let mut step = 1.0;
        let (s, new_value, new_grad) = loop {
            let s: Vec<f64> = direction.iter().map(|d| step * d).collect();
            let candidate: Vec<f64> = theta.iter().zip(&s).map(|(t, s)| t + s).collect();
            let (v, g) = f(&candidate);
            if v <= value + 1e-4 * step * slope {
                break (s, v, g);
            }
            step *= 0.5;
            if step < 1e-12 {
                return theta; // no lower value along it: as close to the optimum as rounding allows
            }
        };
        let y: Vec<f64> = new_grad.iter().zip(&grad).map(|(n, o)| n - o).collect();
        let sy = dot(&s, &y);
        theta.iter_mut().zip(&s).for_each(|(t, s)| *t += s);
        if sy > 1e-12 {
            steps.push_back((s, y, 1.0 / sy));
            if steps.len() > LBFGS_MEMORY {
                steps.pop_front();
            }
        }
        (value, grad) = (new_value, new_grad);
    }
    theta
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(a, b)| a * b).sum()
}

fn axpy(a: f64, x: &[f64], y: &mut [f64]) {
    y.iter_mut().zip(x).for_each(|(y, x)| *y += a * x);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sensors::types::StateDef;

    fn key(k: u8) -> StateKey {
        k.try_into().unwrap()
    }

    fn states(keys: &[u8]) -> StateSet {
        keys.iter()
            .map(|k| StateDef { key: key(*k), name: format!("s{k}").try_into().unwrap() })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap()
    }

    /// Deterministic data, computed the same way in Python (float64, then float32). The expected
    /// heads below are what the origin's `classifier.fit` returns for it.
    fn dataset(counts: &[(u8, usize)], dim: usize) -> (Vec<Vec<f32>>, Vec<StateKey>) {
        let mut i = 0;
        let (mut vectors, mut labels) = (Vec::new(), Vec::new());
        for (k, (label, count)) in counts.iter().enumerate() {
            for _ in 0..*count {
                vectors.push(
                    (0..dim)
                        .map(|d| {
                            let centre = ((k * 5 + d * 3) % 7) as f64 / 7.0 - 0.4;
                            let noise = ((i * 7 + d * 13) % 17) as f64 / 17.0 - 0.5;
                            (centre + 0.8 * noise) as f32
                        })
                        .collect(),
                );
                labels.push(key(*label));
                i += 1;
            }
        }
        (vectors, labels)
    }

    fn assert_close(got: &[f64], want: &[f64], tol: f64) {
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() <= tol, "got {got:?}\nwant {want:?}");
        }
    }

    #[test]
    fn two_states_match_the_origin() {
        let (vectors, labels) = dataset(&[(1, 8), (3, 5)], 6);
        let head = fit(&vectors, &labels).unwrap();
        assert_eq!(head.keys, [key(1), key(3)]);
        let coef = [
            3.396321136799211,
            -2.5915409028262197,
            -1.0277916665805307,
            -1.0411811572595384,
            -2.638786523864301,
            3.375565142398481,
        ];
        assert_close(&head.coef, &coef, 1e-9);
        assert_close(&head.intercept, &[-0.09980094614458102], 1e-9);
        let probs = head.predict(&vectors[0], &states(&[1, 3]));
        assert_close(
            &probs.iter().map(|(_, p)| f64::from(*p)).collect::<Vec<_>>(),
            &[0.9716831360068732, 0.028316863993126884],
            1e-5,
        );
    }

    #[test]
    fn three_states_match_the_origin() {
        let (vectors, labels) = dataset(&[(2, 6), (5, 4), (7, 5)], 5);
        let head = fit(&vectors, &labels).unwrap();
        assert_eq!(head.keys, [key(2), key(5), key(7)]);
        let coef = [
            -3.270113039042574,
            0.45297712305297516,
            1.74243065876921,
            -0.14067718434119902,
            2.6888252325683095,
            2.399692454843908,
            -3.0940178553996347,
            -0.49916504152259233,
            -1.9341499723568925,
            -0.4774527583484354,
            0.870420584198668,
            2.6410407323466614,
            -1.2432656172466192,
            2.0748271566980914,
            -2.2113724742198775,
        ];
        assert_close(&head.coef, &coef, 1e-9);
        assert_close(&head.intercept, &[-0.3642676936717881, -0.11503782878854457, 0.4793055224603374], 1e-9);
        let probs = head.predict(&vectors[0], &states(&[2, 5, 7]));
        let want = [0.9747161013979281, 0.003626781075259136, 0.021657117526812753];
        assert_close(&probs.iter().map(|(_, p)| f64::from(*p)).collect::<Vec<_>>(), &want, 1e-5);
    }

    #[test]
    fn fit_reaches_the_optimum_of_the_objective() {
        for (counts, dim) in [(&[(1, 12), (2, 19)][..], 16), (&[(1, 12), (2, 19), (3, 26), (4, 33)][..], 16)] {
            let (vectors, labels) = dataset(counts, dim);
            let head = fit(&vectors, &labels).unwrap();
            let n = labels.len();
            let target: Vec<usize> = labels.iter().map(|l| head.keys.partition_point(|k| k < l)).collect();
            let problem = Problem {
                x: vectors.iter().flatten().map(|v| f64::from(*v)).collect(),
                weight: target.iter().map(|t| n as f64 / (counts.len() * counts[*t].1) as f64 / n as f64).collect(),
                target,
                dim,
                rows: head.intercept.len(),
                alpha: 1.0 / (CLASSIFIER_C * n as f64),
            };
            let theta: Vec<f64> = head.coef.iter().chain(&head.intercept).copied().collect();
            let (_, grad) = problem.objective(&theta);
            assert!(grad.iter().all(|g| g.abs() <= CLASSIFIER_TOL), "{} states", counts.len());
            let probs = head.predict(&vectors[0], &states(&counts.iter().map(|c| c.0).collect::<Vec<_>>()));
            assert!((probs.iter().map(|(_, p)| p).sum::<f32>() - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn separates_classes_and_gives_untrained_states_zero() {
        let (a, b): (Vec<Vec<f32>>, Vec<Vec<f32>>) = (0..10)
            .map(|i| {
                let noise = |d: usize| ((i * 7 + d * 13) % 17) as f32 / 170.0 - 0.05;
                ((0..8).map(|d| 1.0 + noise(d)).collect(), (0..8).map(|d| -1.0 + noise(d)).collect())
            })
            .unzip();
        let vectors: Vec<Vec<f32>> = a.iter().chain(&b).cloned().collect();
        let labels: Vec<StateKey> = [key(1); 10].into_iter().chain([key(2); 10]).collect();
        let head = fit(&vectors, &labels).unwrap();
        let all = states(&[1, 2, 3]);
        for (v, want) in vectors.iter().zip(&labels) {
            let probs = head.predict(v, &all);
            let (top, _) =
                probs.iter().copied().fold((key(3), 0.0), |best, (k, p)| if p > best.1 { (k, p) } else { best });
            assert_eq!(top, *want);
            assert_eq!(probs[2], (key(3), 0.0), "state without training data");
        }
    }

    #[test]
    fn needs_two_states() {
        assert!(fit(&vec![vec![1.0; 4]; 3], &[key(1); 3]).is_none());
    }

    #[test]
    fn embedding_concatenates_class_token_and_patch_mean() {
        let hidden = [1.0, 0.0, 0.0, 2.0, 0.0, 4.0];
        let v = embedding(&hidden, 2);
        let norm = 10f32.sqrt();
        assert_eq!(v, [1.0 / norm, 0.0, 0.0, 3.0 / norm]);
        assert_eq!(embedding(&[3.0, 4.0], 2), [0.6, 0.8], "already pooled");
    }

    /// Detector outputs with one query per `(class index, probability, [cx, cy, w, h])`, the rest
    /// background (origin `test_objects.outputs`).
    fn outputs(queries: &[(usize, f32, [f32; 4])]) -> Vec<Output> {
        let classes = crate::sensors::COCO.len();
        let mut logits = vec![-10.0f32; 10 * classes];
        let mut boxes = vec![0.0f32; 10 * 4];
        for (q, (class, p, area)) in queries.iter().enumerate() {
            logits[q * classes + class] = (p / (1.0 - p)).ln();
            boxes[q * 4..q * 4 + 4].copy_from_slice(area);
        }
        vec![Output { shape: vec![1, 10, classes], values: logits }, Output { shape: vec![1, 10, 4], values: boxes }]
    }

    const PERSON: usize = 0;
    const CAR: usize = 2;

    #[test]
    fn postprocess_converts_boxes_and_drops_weak() {
        let found = postprocess(&outputs(&[(PERSON, 0.9, [0.5, 0.5, 0.2, 0.4]), (CAR, 0.3, [0.2, 0.2, 0.1, 0.1])]), 0.5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].class.key(), "person");
        assert!((found[0].score - 0.9).abs() < 1e-4);
        for (got, want) in found[0].area.iter().zip([0.4, 0.3, 0.6, 0.7]) {
            assert!((got - want).abs() < 1e-4, "{:?}", found[0].area);
        }
    }

    #[test]
    fn postprocess_merges_duplicates_of_one_class_only() {
        let same = [0.5, 0.5, 0.2, 0.4];
        let found = postprocess(
            &outputs(&[
                (PERSON, 0.9, same),
                (PERSON, 0.8, [0.505, 0.5, 0.2, 0.4]),
                (PERSON, 0.7, [0.1, 0.5, 0.1, 0.2]),
                (CAR, 0.85, same),
            ]),
            0.5,
        )
        .unwrap();
        let got: Vec<(&str, i32)> = found.iter().map(|d| (d.class.key(), (d.score * 100.0).round() as i32)).collect();
        assert_eq!(got, [("person", 90), ("car", 85), ("person", 70)], "most certain first; the car is not merged");
    }

    #[test]
    fn postprocess_refuses_outputs_of_another_shape() {
        let wrong = vec![Output { shape: vec![1, 10, 91], values: vec![0.0; 910] }, Output { shape: vec![1, 10, 4], values: vec![0.0; 40] }];
        assert!(postprocess(&wrong, 0.5).is_err(), "a model with another label set");
        assert!(postprocess(&wrong[..1], 0.5).is_err());
    }
}
