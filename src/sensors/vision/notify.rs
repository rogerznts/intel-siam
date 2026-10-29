//! The webhook and the copy of the image: the only place the vision process leaves the machine.
//! A thread of its own, fed by `Input::Notify`, so a slow or dead endpoint never holds the engine.
//! The bytes posted are the ones the history row kept: one encode, two destinations.

use std::fs;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{Value, json};

use super::Input;
use crate::sensors::types::{ImageSettings, Name, NoticeKind, Payload, SensorId, SensorValue, Webhook};

/// One detection to send and copy. Built by the engine, already past the threshold.
pub struct Job {
    pub sensor: SensorId,
    pub name: Name,
    pub at: SystemTime,
    pub confidence: f32,
    pub value: SensorValue,
    pub changed: bool,
    pub jpeg: Vec<u8>,
    pub webhook: Option<Webhook>,
    pub image: ImageSettings,
}

/// Hands jobs to the thread. Sending never blocks on the network.
pub struct Poster {
    jobs: Sender<Job>,
}

pub fn spawn(engine: Sender<Input>) -> Poster {
    let (jobs, rx) = mpsc::channel();
    thread::spawn(move || work(rx, &engine));
    Poster { jobs }
}

impl Poster {
    pub fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    /// For tests: a poster whose jobs can be watched.
    #[cfg(test)]
    pub fn watched() -> (Self, std::sync::mpsc::Receiver<Job>) {
        let (jobs, rx) = std::sync::mpsc::channel();
        (Self { jobs }, rx)
    }

    /// For tests: the jobs go nowhere.
    #[cfg(test)]
    pub fn detached() -> Self {
        let (jobs, _rx) = mpsc::channel();
        Self { jobs }
    }
}

fn work(rx: Receiver<Job>, engine: &Sender<Input>) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .new_agent();
    for job in rx {
        if let Some(folder) = &job.image.export
            && let Err(error) = export(folder.as_str(), &job)
        {
            notice(engine, job.sensor, NoticeKind::Export(error));
        }
        if let Some(webhook) = &job.webhook
            && let Err(error) = post(&agent, webhook, &job)
        {
            notice(engine, job.sensor, NoticeKind::Webhook(error));
        }
    }
}

fn notice(engine: &Sender<Input>, sensor: SensorId, kind: NoticeKind) {
    let _ = engine.send(Input::Notice { sensor, kind });
}

/// Writes one `<id>-<name>-<time>.jpg`, creating the folder the user typed if it is gone.
fn export(folder: &str, job: &Job) -> Result<(), String> {
    fs::create_dir_all(folder).map_err(|e| format!("{folder}: {e}"))?;
    let path = std::path::Path::new(folder).join(file_name(job));
    fs::write(&path, &job.jpeg).map_err(|e| format!("{}: {e}", path.display()))
}

fn file_name(job: &Job) -> String {
    format!("{}-{}-{}.jpg", job.sensor.0, slug(job.name.as_str()), millis(job.at))
}

/// A name that survives every file system: anything but a letter or a digit becomes `-`,
/// and accents stay, because they are letters and every file system here takes UTF-8.
fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_owned()
}

fn millis(at: SystemTime) -> u128 {
    at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

fn post(agent: &ureq::Agent, webhook: &Webhook, job: &Job) -> Result<(), String> {
    let url = webhook.url.expose();
    match webhook.payload {
        Payload::None | Payload::Base64 => {
            let bytes = serde_json::to_vec(&body(job, webhook.payload)).map_err(|e| e.to_string())?;
            agent
                .post(url)
                .header("Content-Type", "application/json")
                .send(bytes)
                .map_err(|e| e.to_string())?;
        }
        Payload::Multipart => {
            let boundary = format!("intelsim{}", millis(job.at));
            let bytes = multipart(&boundary, &body(job, Payload::None), &job.jpeg);
            agent
                .post(url)
                .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
                .send(bytes)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Everything known about the detection at the moment it happened.
fn body(job: &Job, payload: Payload) -> Value {
    let mut body = json!({
        "sensor": { "id": job.sensor.0, "name": job.name.as_str() },
        "at": millis(job.at),
        "confidence": job.confidence,
        "value": job.value,
        "changed": job.changed,
    });
    if payload == Payload::Base64 {
        body["image"] = json!({
            "encoding": "base64",
            "media_type": "image/jpeg",
            "jpeg": base64::engine::general_purpose::STANDARD.encode(&job.jpeg),
        });
    }
    body
}

fn multipart(boundary: &str, body: &Value, jpeg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(jpeg.len() + 512);
    out.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"payload\"\r\nContent-Type: application/json\r\n\r\n").as_bytes(),
    );
    out.extend_from_slice(serde_json::to_string(body).unwrap_or_default().as_bytes());
    out.extend_from_slice(
        format!("\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"detection.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n").as_bytes(),
    );
    out.extend_from_slice(jpeg);
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(name: &str, at: u64, payload_webhook: Option<Payload>) -> Job {
        Job {
            sensor: SensorId(7),
            name: Name::try_from(name.to_owned()).unwrap(),
            at: UNIX_EPOCH + Duration::from_millis(at),
            confidence: 0.93,
            value: SensorValue::Unknown,
            changed: true,
            jpeg: b"jpegb".to_vec(),
            webhook: payload_webhook.map(|payload| Webhook { url: "https://example.test/hook".to_owned().try_into().unwrap(), payload }),
            image: ImageSettings::default(),
        }
    }

    /// A one-shot HTTP server: answers the first request with 200 and hands its bytes back.
    fn serve_once() -> (String, mpsc::Receiver<Vec<u8>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = socket.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&request).to_string();
                let length = text.lines().find_map(|l| l.strip_prefix("content-length: ")).map_or(0, |n| n.trim().parse().unwrap_or(0));
                if let Some(head) = text.find("\r\n\r\n")
                    && request.len() >= head + 4 + length
                {
                    break;
                }
                if read == 0 {
                    break;
                }
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap();
            let _ = tx.send(request);
        });
        (url, rx)
    }

    #[test]
    fn post_puts_the_detection_on_the_wire() {
        let (url, request) = serve_once();
        let mut job = job("Portão", 1_700_000_000_123, Some(Payload::None));
        job.webhook = Some(Webhook { url: url.try_into().unwrap(), payload: Payload::None });
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().new_agent();
        post(&agent, job.webhook.as_ref().unwrap(), &job).expect("the server answers 200");

        let request = String::from_utf8(request.recv().unwrap()).unwrap();
        assert!(request.starts_with("POST /hook HTTP/1.1"), "{request}");
        assert!(request.to_lowercase().contains("content-type: application/json"), "{request}");
        let body = request.split("\r\n\r\n").nth(1).expect("a body");
        assert_eq!(serde_json::from_str::<serde_json::Value>(body).unwrap()["sensor"]["id"], 7);
    }

    #[test]
    fn post_sends_the_multipart_with_its_boundary() {
        let (url, request) = serve_once();
        let mut job = job("Portão", 1_700_000_000_123, Some(Payload::Multipart));
        job.webhook = Some(Webhook { url: url.try_into().unwrap(), payload: Payload::Multipart });
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().new_agent();
        post(&agent, job.webhook.as_ref().unwrap(), &job).expect("the server answers 200");

        let request = String::from_utf8_lossy(&request.recv().unwrap()).to_string();
        assert!(request.to_lowercase().contains("content-type: multipart/form-data; boundary=intelsim1700000000123"), "{request}");
        assert!(request.contains("filename=\"detection.jpg\""), "{request}");
    }

    #[test]
    fn a_webhook_or_an_export_that_fails_comes_back_as_a_notice() {
        let dir = std::env::temp_dir().join(format!("notify-notice-{}-{}", std::process::id(), getrandom::u64().unwrap()));
        fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("blocked");
        fs::write(&blocker, b"a file where a folder should be").unwrap();

        let mut refused = job("Portão", 1_700_000_000_123, Some(Payload::None));
        refused.webhook = Some(Webhook { url: "http://127.0.0.1:1/hook".to_owned().try_into().unwrap(), payload: Payload::None });
        let mut unwritable = job("Portão", 1_700_000_000_124, None);
        unwritable.image = ImageSettings { export: Some(blocker.join("inside").to_string_lossy().to_string().try_into().unwrap()), ..ImageSettings::default() };

        let (tx, rx) = mpsc::channel();
        let (engine, notices) = mpsc::channel();
        tx.send(refused).unwrap();
        tx.send(unwritable).unwrap();
        drop(tx);
        work(rx, &engine);

        let mut kinds = Vec::new();
        for input in notices.try_iter() {
            if let Input::Notice { kind, .. } = input {
                kinds.push(kind);
            }
        }
        assert_eq!(kinds.len(), 2, "one notice per failure");
        assert!(matches!(kinds[0], NoticeKind::Webhook(_)), "the refused connection is reported");
        assert!(matches!(kinds[1], NoticeKind::Export(_)), "the folder that cannot be made is reported");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_name_keeps_a_name_safe_for_every_file_system() {
        assert_eq!(file_name(&job("Portão / Entrada", 1_700_000_000_123, None)), "7-portão-entrada-1700000000123.jpg");
    }

    #[test]
    fn body_carries_the_detection_and_no_image_unless_asked() {
        let body = body(&job("Portão", 1_700_000_000_123, None), Payload::None);
        assert_eq!(body["sensor"]["id"], 7);
        assert_eq!(body["sensor"]["name"], "Portão");
        assert_eq!(body["at"], 1_700_000_000_123u64);
        assert_eq!(body["changed"], true);
        assert!(body.get("image").is_none());
    }

    #[test]
    fn body_embeds_the_same_bytes_base64_when_asked() {
        let body = body(&job("Portão", 0, None), Payload::Base64);
        assert_eq!(body["image"]["encoding"], "base64");
        assert_eq!(body["image"]["jpeg"], base64::engine::general_purpose::STANDARD.encode(b"jpegb"));
    }

    #[test]
    fn multipart_holds_the_json_and_the_jpeg() {
        let bytes = multipart("xyz", &body(&job("A", 0, None), Payload::None), b"jpegb");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("--xyz\r\n"));
        assert!(text.contains("name=\"payload\""));
        assert!(text.contains("\"confidence\":0.93"));
        assert!(text.contains("name=\"image\"; filename=\"detection.jpg\""));
        assert!(text.ends_with("\r\n--xyz--\r\n"));
    }

    #[test]
    fn export_writes_the_kept_bytes_into_the_folder() {
        let dir = std::env::temp_dir().join(format!("intelsim-notify-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut j = job("Portão", 42, None);
        j.image.export = Some(dir.to_string_lossy().to_string().try_into().unwrap());
        export(j.image.export.as_ref().unwrap().as_str(), &j).unwrap();
        assert_eq!(fs::read(dir.join(file_name(&j))).unwrap(), b"jpegb");
        let _ = fs::remove_dir_all(&dir);
    }
}
