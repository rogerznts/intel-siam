use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use eframe::egui;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{ChannelIndex, DvrSerial};

const AUTH: &str = "https://api.conta.intelbras.com";
const API: &str = "https://api.simplay.isec.intelbras.com.br/api/";
const CLIENT_ID_ENV: &str = "INTEL_SIAM_CLIENT_ID";
const APP_VERSION: &str = "1.4";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// A credential. `Debug` never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DvrCreds {
    pub user: String,
    pub pass: Secret,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dvr {
    pub serial: DvrSerial,
    pub name: String,
    pub creds: DvrCreds,
    pub channels: Vec<(ChannelIndex, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloudError {
    Network(String),
    Http(u16),
    Parse(String),
    /// The browser step failed: the IdP returned an error, or no callback arrived in time.
    Login(String),
    Storage(String),
    MissingEnv(&'static str),
}

impl fmt::Display for CloudError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(e) => write!(f, "sem conexão com a nuvem: {e}"),
            Self::Http(code) => write!(f, "a nuvem respondeu HTTP {code}"),
            Self::Parse(e) => write!(f, "resposta inesperada da nuvem: {e}"),
            Self::Login(e) => write!(f, "login não concluído: {e}"),
            Self::Storage(e) => write!(f, "não foi possível gravar o login: {e}"),
            Self::MissingEnv(name) => write!(f, "defina a variável de ambiente {name}"),
        }
    }
}

impl From<ureq::Error> for CloudError {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::StatusCode(code) => Self::Http(code),
            other => Self::Network(other.to_string()),
        }
    }
}

#[derive(Deserialize)]
struct DevicesPage {
    results: Vec<DeviceEntry>,
}

#[derive(Deserialize)]
struct DeviceEntry {
    serial_number: String,
    data: DeviceData,
}

#[derive(Deserialize)]
struct DeviceData {
    name: String,
    username: String,
    password: String,
    #[serde(default)]
    channels: Vec<ChannelEntry>,
}

#[derive(Deserialize)]
struct ChannelEntry {
    name: String,
    index: u8,
}

/// Boundary parse of `GET v1/video-devices`.
pub fn parse_devices(json: &[u8]) -> Result<Vec<Dvr>, CloudError> {
    let page: DevicesPage = serde_json::from_slice(json).map_err(|e| CloudError::Parse(e.to_string()))?;
    Ok(page
        .results
        .into_iter()
        .map(|d| Dvr {
            serial: DvrSerial::new(d.serial_number),
            name: d.data.name,
            creds: DvrCreds { user: d.data.username, pass: Secret::new(d.data.password) },
            channels: d.data.channels.into_iter().map(|c| (ChannelIndex::new(c.index), c.name)).collect(),
        })
        .collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Refreshing,
    AwaitingBrowser,
    LoadingDevices,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Account {
    SignedOut,
    Working(Stage),
    Ready { devices: BTreeMap<DvrSerial, Dvr> },
    Failed(CloudError),
}

enum Job {
    /// Refresh token from `secrets.json` if there is one, else `SignedOut`.
    Resume,
    /// Browser login with PKCE.
    SignIn,
}

/// UI-side view of the cloud worker. The worker is the only writer of `secrets.json`.
pub struct AccountHandle {
    state: Account,
    rx: Option<mpsc::Receiver<Account>>,
    secrets: PathBuf,
    ctx: egui::Context,
}

impl AccountHandle {
    pub fn start(secrets: PathBuf, ctx: egui::Context) -> Self {
        let mut handle = Self { state: Account::SignedOut, rx: None, secrets, ctx };
        handle.spawn(Job::Resume);
        handle
    }

    pub fn sign_in(&mut self) {
        self.spawn(Job::SignIn);
    }

    pub fn retry(&mut self) {
        self.spawn(Job::Resume);
    }

    pub fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        loop {
            match rx.try_recv() {
                Ok(state) => self.state = state,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.rx = None;
                    break;
                }
            }
        }
    }

    pub fn state(&self) -> &Account {
        &self.state
    }

    pub fn devices(&self) -> Option<&BTreeMap<DvrSerial, Dvr>> {
        match &self.state {
            Account::Ready { devices } => Some(devices),
            _ => None,
        }
    }

    /// At most one job runs at a time, so `secrets.json` has a single writer.
    fn spawn(&mut self, job: Job) {
        if self.rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.state = Account::Working(match job {
            Job::Resume => Stage::Refreshing,
            Job::SignIn => Stage::AwaitingBrowser,
        });
        let (secrets, ctx) = (self.secrets.clone(), self.ctx.clone());
        thread::spawn(move || {
            let send = |state: Account| {
                let _ = tx.send(state);
                ctx.request_repaint();
            };
            let end = match job {
                Job::Resume => resume(&secrets, &send),
                Job::SignIn => browser_login(&secrets, &send),
            };
            send(end);
        });
    }
}

fn client_id() -> Result<String, CloudError> {
    std::env::var(CLIENT_ID_ENV).map_err(|_| CloudError::MissingEnv(CLIENT_ID_ENV))
}

fn resume(secrets: &Path, send: &dyn Fn(Account)) -> Account {
    let Some(refresh_token) = load_refresh_token(secrets) else { return Account::SignedOut };
    let client_id = match client_id() {
        Ok(id) => id,
        Err(e) => return Account::Failed(e),
    };
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", client_id.as_str()),
        ("refresh_token", refresh_token.expose()),
    ];
    match request_tokens(&form) {
        Ok(tokens) => finish(secrets, tokens, send),
        // The IdP rejects an expired or revoked refresh token with 400/401: a fresh login is needed.
        Err(CloudError::Http(400 | 401)) => Account::SignedOut,
        Err(e) => Account::Failed(e),
    }
}

fn browser_login(secrets: &Path, send: &dyn Fn(Account)) -> Account {
    match client_id().and_then(|client_id| {
        let (code, redirect, verifier) = browser_code(&client_id)?;
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("scope", "openid"),
        ];
        request_tokens(&form)
    }) {
        Ok(tokens) => finish(secrets, tokens, send),
        Err(e) => Account::Failed(e),
    }
}

fn finish(secrets: &Path, tokens: Tokens, send: &dyn Fn(Account)) -> Account {
    if let Some(refresh) = &tokens.refresh_token
        && let Err(e) = save_refresh_token(secrets, &Secret::new(refresh.clone()))
    {
        return Account::Failed(CloudError::Storage(e.to_string()));
    }
    send(Account::Working(Stage::LoadingDevices));
    match load_devices(&tokens.access_token) {
        Ok(dvrs) => Account::Ready { devices: dvrs.into_iter().map(|d| (d.serial.clone(), d)).collect() },
        Err(e) => Account::Failed(e),
    }
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
}

fn request_tokens(form: &[(&str, &str)]) -> Result<Tokens, CloudError> {
    let body = ureq::post(format!("{AUTH}/auth/token")).send_form(form.iter().copied())?.body_mut().read_to_vec()?;
    serde_json::from_slice(&body).map_err(|e| CloudError::Parse(e.to_string()))
}

fn load_devices(access_token: &str) -> Result<Vec<Dvr>, CloudError> {
    let login = ureq::post(format!("{API}v1/auth/login"))
        .header("Authorization", access_token)
        .header("X-SIMPLAY-APP-VERSION", APP_VERSION)
        .header("Content-Type", "application/json")
        .send_empty()?;
    let session = login
        .headers()
        .get("x-simplay-session-token")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| CloudError::Parse("login sem x-simplay-session-token".into()))?
        .to_owned();
    let body = ureq::get(format!("{API}v1/video-devices?take=50&page=1"))
        .header("Authorization", access_token)
        .header("X-SIMPLAY-APP-VERSION", APP_VERSION)
        .header("X-SIMPLAY-SESSION-TOKEN", &session)
        .call()?
        .body_mut()
        .read_to_vec()?;
    parse_devices(&body)
}

const PKCE_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// 128 characters of `[A-Za-z0-9]`, as SIMPlay generates it.
fn pkce_verifier() -> String {
    let mut out = String::with_capacity(128);
    let mut buf = [0u8; 64];
    while out.len() < 128 {
        getrandom::fill(&mut buf).expect("OS random source");
        // Rejection sampling keeps the 62 symbols uniform: 248 is the largest multiple of 62 below 256.
        for &b in buf.iter().filter(|&&b| b < 248) {
            if out.len() < 128 {
                out.push(PKCE_ALPHABET[(b % 62) as usize] as char);
            }
        }
    }
    out
}

fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Percent-encodes everything outside RFC 3986 unreserved characters.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = match bytes[i] {
            b'%' if i + 2 < bytes.len() => hex(bytes[i + 1]).zip(hex(bytes[i + 2])).map(|(h, l)| (h * 16 + l) as u8),
            _ => None,
        };
        match (decoded, bytes[i]) {
            (Some(b), _) => {
                out.push(b);
                i += 3;
            }
            (None, b'+') => {
                out.push(b' ');
                i += 1;
            }
            (None, b) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (percent_decode(k) == name).then(|| percent_decode(v))
    })
}

/// Opens the browser on the IdP and waits on a loopback listener for the redirect.
/// Returns `(code, redirect_uri, verifier)`.
fn browser_code(client_id: &str) -> Result<(String, String, String), CloudError> {
    let server = tiny_http::Server::http("127.0.0.1:0").map_err(|e| CloudError::Login(e.to_string()))?;
    let port = server.server_addr().to_ip().map(|a| a.port()).ok_or_else(|| CloudError::Login("loopback sem porta".into()))?;
    let redirect = format!("http://127.0.0.1:{port}/");
    let verifier = pkce_verifier();
    let url = format!(
        "{AUTH}/auth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&scope=openid\
         &code_challenge={}&code_challenge_method=S256",
        percent_encode(&redirect),
        pkce_challenge(&verifier),
    );
    eprintln!("intel-siam: login em {url}");
    open::that_detached(&url).map_err(|e| CloudError::Login(format!("não abriu o navegador: {e}")))?;
    let code = wait_for_code(&server, Instant::now() + LOGIN_TIMEOUT)?;
    Ok((code, redirect, verifier))
}

fn wait_for_code(server: &tiny_http::Server, deadline: Instant) -> Result<String, CloudError> {
    let html = |body: &str| {
        tiny_http::Response::from_string(format!("<html><body><p>{body}</p></body></html>")).with_header(
            tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8").expect("static header"),
        )
    };
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(CloudError::Login("tempo esgotado esperando o navegador".into()));
        }
        let Some(request) = server.recv_timeout(left).map_err(|e| CloudError::Login(e.to_string()))? else { continue };
        let url = request.url().to_owned();
        if let Some(code) = query_param(&url, "code") {
            let _ = request.respond(html("Login concluído. Pode voltar ao Intel SIAM."));
            return Ok(code);
        }
        if let Some(error) = query_param(&url, "error") {
            let _ = request.respond(html("Login recusado. Volte ao Intel SIAM e tente de novo."));
            return Err(CloudError::Login(error));
        }
        // Browsers also ask for /favicon.ico; anything without code or error is not the redirect.
        let _ = request.respond(tiny_http::Response::empty(404));
    }
}

#[derive(Serialize, Deserialize)]
struct SecretsFile {
    version: u32,
    refresh_token: String,
}

fn load_refresh_token(path: &Path) -> Option<Secret> {
    let file: SecretsFile = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    Some(Secret::new(file.refresh_token))
}

/// tmp + rename, so a crash never leaves a half-written token; created 0600 so no other user reads it.
fn save_refresh_token(path: &Path, token: &Secret) -> io::Result<()> {
    use std::io::Write;

    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp)?;
    let json = serde_json::to_vec(&SecretsFile { version: 1, refresh_token: token.expose().to_owned() })?;
    file.write_all(&json)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../testdata/video-devices.redacted.json");

    #[test]
    fn parses_the_real_device_list_shape() {
        let dvrs = parse_devices(FIXTURE).unwrap();
        assert_eq!(dvrs.len(), 1);
        let dvr = &dvrs[0];
        assert_eq!(dvr.serial, DvrSerial::new("ABC0000000001"));
        assert_eq!(dvr.name, "casa");
        assert_eq!(dvr.creds.user, "fake-user");
        assert_eq!(dvr.creds.pass.expose(), "fake-pass");
        assert_eq!(dvr.channels.len(), 8);
        assert_eq!(dvr.channels[0], (ChannelIndex::new(0), "Canal1".to_owned()));
        assert_eq!(dvr.channels[7].0.rtsp_channel(), 8);
    }

    #[test]
    fn debug_output_never_contains_the_password() {
        let dvrs = parse_devices(FIXTURE).unwrap();
        assert!(!format!("{dvrs:?}").contains("fake-pass"));
    }

    #[test]
    fn malformed_body_is_a_parse_error() {
        assert!(matches!(parse_devices(br#"{"results": [{"serial_number": 1}]}"#), Err(CloudError::Parse(_))));
    }

    #[test]
    fn verifier_is_128_alphanumerics_and_fresh_each_time() {
        let v = pkce_verifier();
        assert_eq!(v.len(), 128);
        assert!(v.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_ne!(v, pkce_verifier());
    }

    #[test]
    fn challenge_is_unpadded_base64url_sha256() {
        // Expected value from Python's hashlib + base64.urlsafe_b64encode.
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mJ92K9ZgsfJkzVTxvYQ1Fz4-fJo"), "p88FeKNm3Af-Fi7FVIKJsbn-ZjEzi_VfEiVN-CkrMno");
    }

    #[test]
    fn loopback_ignores_favicon_and_returns_the_decoded_code() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let browser = thread::spawn(move || {
            let base = format!("http://127.0.0.1:{port}");
            let favicon = ureq::get(format!("{base}/favicon.ico")).call();
            let page = ureq::get(format!("{base}/?code=ab%2Fc+d&session_state=x")).call().unwrap().body_mut().read_to_string();
            (favicon.err(), page.unwrap())
        });
        let code = wait_for_code(&server, Instant::now() + Duration::from_secs(5)).unwrap();
        let (favicon, page) = browser.join().unwrap();
        assert_eq!(code, "ab/c d");
        assert!(matches!(favicon, Some(ureq::Error::StatusCode(404))));
        assert!(page.contains("Login concluído"));
    }

    #[test]
    fn idp_error_redirect_fails_the_login() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        thread::spawn(move || ureq::get(format!("http://127.0.0.1:{port}/?error=access_denied")).call());
        let err = wait_for_code(&server, Instant::now() + Duration::from_secs(5)).unwrap_err();
        assert_eq!(err, CloudError::Login("access_denied".into()));
    }

    #[cfg(unix)]
    #[test]
    fn refresh_token_survives_restart_and_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("intel-siam-secrets-{}", std::process::id()));
        let path = dir.join("secrets.json");
        assert!(load_refresh_token(&path).is_none());
        save_refresh_token(&path, &Secret::new("r1")).unwrap();
        save_refresh_token(&path, &Secret::new("r2")).unwrap();
        assert_eq!(load_refresh_token(&path).unwrap().expose(), "r2");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(dir).unwrap();
    }
}
