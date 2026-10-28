use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui::{Color32, ColorImage};
use serde::{Deserialize, Serialize};

use crate::model::CameraId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum StreamKind {
    Sub,
    Main,
}

impl StreamKind {
    pub const fn subtype(self) -> u8 {
        match self {
            Self::Sub => 1,
            Self::Main => 0,
        }
    }

    /// Fixed output size per kind. ffmpeg scales to it, so the byte framing is known before
    /// spawn and a resolution change on the DVR can never desync the pipe.
    pub const fn size(self) -> [usize; 2] {
        match self {
            Self::Sub => [640, 360],
            Self::Main => [1920, 1080],
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Sub => "sub",
            Self::Main => "main",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct StreamKey {
    pub camera: CameraId,
    pub kind: StreamKind,
}

/// One writer (the reader thread) and one reader (the UI). Latest frame wins; older ones are dropped
/// whole, never partially.
#[derive(Default)]
pub struct FrameSlot {
    latest: Mutex<Option<Arc<ColorImage>>>,
    frames: AtomicU64,
}

impl FrameSlot {
    pub fn take(&self) -> Option<Arc<ColorImage>> {
        self.latest.lock().unwrap().take()
    }

    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    fn publish(&self, image: Arc<ColorImage>) {
        *self.latest.lock().unwrap() = Some(image);
        self.frames.fetch_add(1, Ordering::Relaxed);
    }
}

const POOL: usize = 4;

/// Reads fixed-size RGBA frames until EOF. Images are recycled once egui has dropped its reference,
/// so steady state allocates nothing. Video is opaque, so the RGBA bytes are valid premultiplied
/// `Color32` as they come.
pub fn pump_frames(mut stdout: impl Read, kind: StreamKind, slot: &FrameSlot, wake: &dyn Fn()) -> io::Result<()> {
    let size = kind.size();
    let mut pool: Vec<Arc<ColorImage>> = Vec::with_capacity(POOL);
    loop {
        let mut image = match pool.iter_mut().position(|img| Arc::get_mut(img).is_some()) {
            Some(i) => pool.swap_remove(i),
            None => Arc::new(ColorImage::filled(size, Color32::BLACK)),
        };
        let pixels = &mut Arc::get_mut(&mut image).expect("pooled image is unique").pixels;
        match stdout.read_exact(bytemuck::cast_slice_mut(pixels)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        if pool.len() < POOL {
            pool.push(Arc::clone(&image));
        }
        slot.publish(image);
        wake();
    }
}

/// The picture size on one of ffmpeg's stream lines, such as
/// `  Stream #0:0: Video: h264 (Main), yuv420p(progressive), 704x480, 25 fps`. ffmpeg prints the
/// input's streams before the output's, so the first match is the input's size.
pub fn parse_input_geometry(line: &str) -> Option<[u32; 2]> {
    let (_, video) = line.trim_start().strip_prefix("Stream #")?.split_once(": Video: ")?;
    video.split([' ', ',']).find_map(|word| {
        let (w, h) = word.split_once('x')?;
        let size = [w.parse().ok()?, h.parse().ok()?];
        (size[0] > 0 && size[1] > 0).then_some(size)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_keeps_only_the_latest_whole_frame() {
        let kind = StreamKind::Sub;
        let [w, h] = kind.size();
        let mut bytes = vec![10u8; w * h * 4];
        bytes.extend(vec![20u8; w * h * 4]);
        bytes.extend(vec![30u8; w * h * 2]);
        let slot = FrameSlot::default();
        pump_frames(bytes.as_slice(), kind, &slot, &|| {}).unwrap();
        assert_eq!(slot.frames(), 2);
        let frame = slot.take().unwrap();
        assert_eq!(frame.size, [w, h]);
        assert!(frame.pixels.iter().all(|p| p.to_array() == [20; 4]));
        assert!(slot.take().is_none());
    }

    #[test]
    fn input_geometry_comes_from_stream_lines_only() {
        let lines = [
            "Input #0, rtsp, from 'rtsp://***@127.0.0.1:5704/cam':",
            "  Stream #0:0: Video: h264 (Main), yuv420p(progressive), 704x480, 25 fps, 25 tbr, 90k tbn",
            "  Stream #0:0 -> #0:0 (h264 (native) -> rawvideo (native))",
            "  Stream #0:0: Video: rawvideo (RGBA / 0x41424752), rgba(pc, gbr/unknown/unknown, progressive), \
             640x360 [SAR 33:40 DAR 22:15], q=2-31, 14745 kb/s, 2 fps, 2 tbn",
        ];
        let sizes: Vec<_> = lines.iter().map(|l| parse_input_geometry(l)).collect();
        assert_eq!(sizes, [None, Some([704, 480]), None, Some([640, 360])]);
        assert_eq!(
            parse_input_geometry("  Stream #0:1[0x1](und): Video: hevc (avc1 / 0x31637661), yuv420p(tv, bt709), 1920x1080"),
            Some([1920, 1080])
        );
    }
}
