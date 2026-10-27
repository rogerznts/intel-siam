use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, ColorImage};

use crate::model::CameraId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
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

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
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
pub fn pump_frames(mut stdout: impl Read, kind: StreamKind, slot: &FrameSlot, ctx: &egui::Context) -> io::Result<()> {
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
        ctx.request_repaint();
    }
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
        pump_frames(bytes.as_slice(), kind, &slot, &egui::Context::default()).unwrap();
        assert_eq!(slot.frames(), 2);
        let frame = slot.take().unwrap();
        assert_eq!(frame.size, [w, h]);
        assert!(frame.pixels.iter().all(|p| p.to_array() == [20; 4]));
        assert!(slot.take().is_none());
    }
}
