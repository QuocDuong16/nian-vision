//! Camera-local motion detector consuming thumbnails decoded from an existing
//! shared ingest subscription. The output is **scene motion**, not people or
//! object recognition. No compressed packet size or audio heuristic is used.

use nian_media_ffmpeg::{LUMA_HEIGHT, LUMA_WIDTH, LumaThumbnail};

const PIXELS: usize = LUMA_WIDTH * LUMA_HEIGHT;
const CHANGED_PIXEL_THRESHOLD: i16 = 23;
const MIN_CHANGED_PIXELS: usize = PIXELS / 7; // ~14% of the 32x18 scene.
const ACTIVE_CONFIRM_FRAMES: u8 = 2;
const IDLE_CONFIRM_FRAMES: u8 = 4;

/// `Some(true)` begins an episode, `Some(false)` ends it, `None` means no
/// transition. A new generation uses a fresh detector, never stale pixels.
#[derive(Default)]
pub struct MotionDetector {
    previous: Option<LumaThumbnail>,
    active: bool,
    moving_frames: u8,
    quiet_frames: u8,
}

impl MotionDetector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, thumbnail: LumaThumbnail) -> Option<bool> {
        let moving = self
            .previous
            .as_ref()
            .is_some_and(|previous| scene_changed(previous.pixels(), thumbnail.pixels()));
        self.previous = Some(thumbnail);
        if moving {
            self.quiet_frames = 0;
            self.moving_frames = self.moving_frames.saturating_add(1);
            if !self.active && self.moving_frames >= ACTIVE_CONFIRM_FRAMES {
                self.active = true;
                return Some(true);
            }
        } else {
            self.moving_frames = 0;
            self.quiet_frames = self.quiet_frames.saturating_add(1);
            if self.active && self.quiet_frames >= IDLE_CONFIRM_FRAMES {
                self.active = false;
                return Some(false);
            }
        }
        None
    }

    pub fn is_active(&self) -> bool {
        self.active
    }
}

fn scene_changed(previous: &[u8; PIXELS], current: &[u8; PIXELS]) -> bool {
    // Subtract scene-wide brightness shift before thresholding to ignore
    // auto-exposure and day/night illumination fluctuations. Local pixel
    // differences remain, unlike a simplistic average-brightness detector.
    let global_delta: i32 = current
        .iter()
        .zip(previous.iter())
        .map(|(after, before)| i32::from(*after) - i32::from(*before))
        .sum::<i32>()
        / PIXELS as i32;
    let mut changed = 0;
    for (after, before) in current.iter().zip(previous.iter()) {
        let residual = i32::from(*after) - i32::from(*before) - global_delta;
        if residual.abs() > i32::from(CHANGED_PIXEL_THRESHOLD) {
            changed += 1;
            if changed >= MIN_CHANGED_PIXELS {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(brightness: u8, object_start: Option<usize>) -> LumaThumbnail {
        let mut pixels = [brightness; PIXELS];
        if let Some(start) = object_start {
            for y in 3..15 {
                for x in start..start + 12 {
                    pixels[y * LUMA_WIDTH + x] = brightness.saturating_add(130);
                }
            }
        }
        LumaThumbnail::from_pixels(pixels)
    }

    #[test]
    fn scene_motion_produces_one_start_and_one_end_with_hysteresis() {
        let mut detector = MotionDetector::new();
        assert_eq!(detector.observe(scene(35, None)), None);
        assert_eq!(detector.observe(scene(35, Some(0))), None);
        assert_eq!(detector.observe(scene(35, Some(14))), Some(true));
        assert!(detector.is_active());
        assert_eq!(detector.observe(scene(35, Some(14))), None);
        for _ in 0..2 {
            assert_eq!(detector.observe(scene(35, Some(14))), None);
        }
        assert_eq!(detector.observe(scene(35, Some(14))), Some(false));
        assert!(!detector.is_active());
    }

    #[test]
    fn whole_scene_exposure_changes_do_not_generate_motion() {
        let mut detector = MotionDetector::new();
        for brightness in [20, 100, 180, 40, 190, 20] {
            assert_eq!(detector.observe(scene(brightness, None)), None);
        }
    }

    #[test]
    fn new_detector_does_not_inherit_motion_across_ingest_generations() {
        let mut first = MotionDetector::new();
        first.observe(scene(20, None));
        first.observe(scene(20, Some(0)));
        assert_eq!(first.observe(scene(20, Some(14))), Some(true));
        let mut next = MotionDetector::new();
        assert_eq!(next.observe(scene(20, Some(14))), None);
        assert!(!next.is_active());
    }
}
