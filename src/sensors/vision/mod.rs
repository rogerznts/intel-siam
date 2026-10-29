//! The vision process. Private to `sensors`; the binary reaches it through `sensors::run_vision`.
//! `logic`, `pixels` and the classifier in `infer` are pure: no I/O, no clock, no randomness.
//!
//! ```text
//! stdin reader ─Command─┐
//! infer worker ─Done────┤ mpsc<Input> ─▶ ENGINE ─Out─▶ stdout writer
//! model fetcher ─Model──┤               (Store, Feeds, sensors)
//! feeds waker ──Wake────┘
//! ```

mod engine;
mod feeds;
mod infer;
mod logic;
mod pixels;
#[cfg(test)]
pub(super) mod process_tests;
mod store;
mod teach;

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;

use super::LoadNetwork;
use super::wire::{self, Command};
use engine::{Engine, Out};
use feeds::Feeds;
use store::Store;

/// Everything the engine reacts to.
enum Input {
    Command(Command),
    /// stdin or stdout is gone: the app quit or stopped the process.
    Closed,
    /// A feed has a frame or changed phase.
    Wake,
    Infer(infer::Done),
    Model(infer::ModelEvent),
}

/// Paths only; DVR URLs arrive later over stdin.
struct Args {
    data: PathBuf,
    logs: PathBuf,
    ffmpeg: PathBuf,
}

impl Args {
    fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Self, String> {
        let (mut data, mut logs, mut ffmpeg) = (None, None, None);
        while let Some(flag) = args.next() {
            let slot = match flag.to_str() {
                Some("--data") => &mut data,
                Some("--logs") => &mut logs,
                Some("--ffmpeg") => &mut ffmpeg,
                _ => return Err(format!("unknown argument {flag:?}")),
            };
            *slot = Some(PathBuf::from(args.next().ok_or_else(|| format!("{flag:?} needs a value"))?));
        }
        Ok(Self {
            data: data.ok_or("missing --data")?,
            logs: logs.ok_or("missing --logs")?,
            ffmpeg: ffmpeg.ok_or("missing --ffmpeg")?,
        })
    }
}

pub(super) fn run(args: impl Iterator<Item = OsString>, load: LoadNetwork) -> ExitCode {
    let protocol = match take_stdout() {
        Ok(protocol) => protocol,
        Err(e) => {
            eprintln!("cannot set up the protocol stream: {e}");
            return ExitCode::FAILURE;
        }
    };
    let args = match Args::parse(args) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let store = match Store::open(&args.data) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("cannot open the sensor database: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (tx, rx) = mpsc::channel();
    read_commands(tx.clone());
    let out = write_reports(protocol, tx.clone());
    let infer = infer::spawn(args.data.join("models"), tx.clone(), load);
    let feeds = Feeds::new(
        args.ffmpeg,
        args.logs,
        Arc::new(move || {
            let _ = tx.send(Input::Wake);
        }),
    );
    match Engine::new(store, feeds, infer, out) {
        Ok(engine) => {
            engine.run(rx);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cannot load the sensors: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Keeps fd 1 for the protocol alone and points fd 1 at stderr, so a print from ONNX Runtime or
/// any library can never corrupt the stream. Runs before any other thread exists.
fn take_stdout() -> io::Result<File> {
    // SAFETY: plain descriptor calls on this process's own standard streams; the duplicate is
    // owned by the returned `File` alone, and close-on-exec keeps it out of the decoders.
    unsafe {
        let protocol = libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3);
        if protocol < 0 || libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(File::from_raw_fd(protocol))
    }
}

fn read_commands(engine: Sender<Input>) {
    thread::spawn(move || {
        let mut stdin = BufReader::new(io::stdin().lock());
        loop {
            let input = match wire::read_frame::<Command>(&mut stdin) {
                Ok(Some((cmd, _))) => Input::Command(cmd),
                Ok(None) => Input::Closed,
                Err(e) => {
                    eprintln!("protocol violation on stdin: {e}");
                    Input::Closed
                }
            };
            let closed = matches!(input, Input::Closed);
            if engine.send(input).is_err() || closed {
                return;
            }
        }
    });
}

fn write_reports(protocol: File, engine: Sender<Input>) -> Sender<Out> {
    let (tx, rx) = mpsc::channel::<Out>();
    thread::spawn(move || {
        let mut w = BufWriter::new(protocol);
        for out in rx {
            if let Err(e) = wire::write_frame(&mut w, &out.report, &out.payload) {
                eprintln!("cannot write to the app: {e}");
                let _ = engine.send(Input::Closed);
                return;
            }
        }
    });
    tx
}
