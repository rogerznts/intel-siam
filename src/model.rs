use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DvrSerial(String);

impl DvrSerial {
    pub fn new(serial: impl Into<String>) -> Self {
        Self(serial.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 0-based, as in `channels[].index` of `/video-devices`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelIndex(u8);

impl ChannelIndex {
    pub const fn new(index: u8) -> Self {
        Self(index)
    }

    /// RTSP `channel=` is 1-based while the cloud index is 0-based; this is the only place they meet.
    pub const fn rtsp_channel(self) -> u16 {
        self.0 as u16 + 1
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct CameraId {
    pub dvr: DvrSerial,
    pub channel: ChannelIndex,
}

pub const MAX_TILES: usize = 12;

/// Invariant: at most `MAX_TILES` unique cameras; index is the grid position. Only `reduce` mutates it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    tiles: Vec<CameraId>,
}

impl Layout {
    pub fn tiles(&self) -> &[CameraId] {
        &self.tiles
    }

    pub fn contains(&self, cam: &CameraId) -> bool {
        self.tiles.contains(cam)
    }

    pub fn is_full(&self) -> bool {
        self.tiles.len() >= MAX_TILES
    }

    /// Saved data is untrusted input: duplicates are dropped and the list is cut to `MAX_TILES`.
    pub fn from_saved(saved: SavedLayout) -> Self {
        let mut tiles: Vec<CameraId> = Vec::with_capacity(MAX_TILES);
        for cam in saved.tiles {
            if tiles.len() == MAX_TILES {
                break;
            }
            if !tiles.contains(&cam) {
                tiles.push(cam);
            }
        }
        Self { tiles }
    }
}

pub const LAYOUT_KEY: &str = "intel-siam.layout";

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedLayout {
    pub version: u32,
    pub tiles: Vec<CameraId>,
}

impl From<&Layout> for SavedLayout {
    fn from(layout: &Layout) -> Self {
        Self { version: 1, tiles: layout.tiles.clone() }
    }
}

/// Runtime only: solo never touches the saved layout (FR-004).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    Grid,
    Solo(CameraId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    Swap(usize, usize),
    ToggleSolo(CameraId),
    AddCamera(CameraId),
    RemoveCamera(CameraId),
}

pub fn reduce(layout: &mut Layout, view: &mut ViewMode, intent: Intent) {
    match intent {
        Intent::Swap(a, b) => {
            if a < layout.tiles.len() && b < layout.tiles.len() {
                layout.tiles.swap(a, b);
            }
        }
        Intent::ToggleSolo(cam) => {
            *view = match view {
                ViewMode::Solo(current) if *current == cam => ViewMode::Grid,
                _ if layout.contains(&cam) => ViewMode::Solo(cam),
                _ => view.clone(),
            };
        }
        Intent::AddCamera(cam) => {
            if !layout.is_full() && !layout.contains(&cam) {
                layout.tiles.push(cam);
            }
        }
        Intent::RemoveCamera(cam) => {
            layout.tiles.retain(|t| *t != cam);
            if *view == ViewMode::Solo(cam) {
                *view = ViewMode::Grid;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(channel: u8) -> CameraId {
        CameraId { dvr: DvrSerial::new("ABC0000000001"), channel: ChannelIndex::new(channel) }
    }

    fn layout_of(channels: &[u8]) -> Layout {
        Layout::from_saved(SavedLayout { version: 1, tiles: channels.iter().map(|&c| cam(c)).collect() })
    }

    fn channels(layout: &Layout) -> Vec<u8> {
        layout.tiles().iter().map(|t| t.channel.0).collect()
    }

    #[test]
    fn swap_reorders_two_tiles() {
        let mut layout = layout_of(&[0, 1, 2, 3]);
        let mut view = ViewMode::Grid;
        reduce(&mut layout, &mut view, Intent::Swap(0, 2));
        assert_eq!(channels(&layout), [2, 1, 0, 3]);
    }

    #[test]
    fn swap_out_of_range_changes_nothing() {
        let mut layout = layout_of(&[0, 1]);
        let mut view = ViewMode::Grid;
        reduce(&mut layout, &mut view, Intent::Swap(0, 5));
        assert_eq!(channels(&layout), [0, 1]);
    }

    #[test]
    fn solo_and_back_leaves_layout_intact() {
        let mut layout = layout_of(&[3, 1, 4]);
        let before = layout.clone();
        let mut view = ViewMode::Grid;
        reduce(&mut layout, &mut view, Intent::ToggleSolo(cam(1)));
        assert_eq!(view, ViewMode::Solo(cam(1)));
        assert_eq!(layout, before);
        reduce(&mut layout, &mut view, Intent::ToggleSolo(cam(1)));
        assert_eq!(view, ViewMode::Grid);
        assert_eq!(layout, before);
    }

    #[test]
    fn solo_on_camera_outside_layout_is_ignored() {
        let mut layout = layout_of(&[0]);
        let mut view = ViewMode::Grid;
        reduce(&mut layout, &mut view, Intent::ToggleSolo(cam(7)));
        assert_eq!(view, ViewMode::Grid);
    }

    #[test]
    fn add_appends_once_and_stops_at_twelve() {
        let mut layout = Layout::default();
        let mut view = ViewMode::Grid;
        for c in 0..15 {
            reduce(&mut layout, &mut view, Intent::AddCamera(cam(c)));
        }
        reduce(&mut layout, &mut view, Intent::AddCamera(cam(0)));
        assert_eq!(channels(&layout), [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
    }

    #[test]
    fn removing_the_solo_camera_returns_to_grid() {
        let mut layout = layout_of(&[0, 1]);
        let mut view = ViewMode::Solo(cam(1));
        reduce(&mut layout, &mut view, Intent::RemoveCamera(cam(1)));
        assert_eq!(channels(&layout), [0]);
        assert_eq!(view, ViewMode::Grid);
    }

    #[test]
    fn from_saved_drops_duplicates_and_truncates_to_twelve() {
        let layout = layout_of(&[5, 5, 0, 1, 0, 2, 3, 4, 6, 7, 8, 9, 10, 11, 12, 13]);
        assert_eq!(channels(&layout), [5, 0, 1, 2, 3, 4, 6, 7, 8, 9, 10, 11]);
    }

    #[test]
    fn saved_layout_round_trips_order() {
        let layout = layout_of(&[2, 0, 1]);
        let json = serde_json::to_string(&SavedLayout::from(&layout)).unwrap();
        let back = Layout::from_saved(serde_json::from_str(&json).unwrap());
        assert_eq!(back, layout);
    }
}
