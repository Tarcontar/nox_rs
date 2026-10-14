//! Rendered frames, so scrubbing back over them and playback are instant.

use std::collections::HashMap;
use std::sync::Arc;

use crate::RenderedFrame;

/// What a frame was rendered from: the timeline version and the output
/// size. Frames of other keys are stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
}

/// Frames by sequence frame number, evicted least recently used first
/// once `budget` bytes are exceeded.
pub(crate) struct FrameCache {
    key: Option<CacheKey>,
    frames: HashMap<i64, (Arc<RenderedFrame>, u64)>,
    bytes: usize,
    budget: usize,
    clock: u64,
}

impl FrameCache {
    pub fn new(budget: usize) -> Self {
        Self {
            key: None,
            frames: HashMap::new(),
            bytes: 0,
            budget,
            clock: 0,
        }
    }

    /// Drops everything if `key` differs from the frames' key.
    pub fn validate(&mut self, key: CacheKey) {
        if self.key != Some(key) {
            self.frames.clear();
            self.bytes = 0;
            self.key = Some(key);
        }
    }

    pub fn get(&mut self, key: CacheKey, frame: i64) -> Option<Arc<RenderedFrame>> {
        if self.key != Some(key) {
            return None;
        }
        self.clock += 1;
        let clock = self.clock;
        self.frames.get_mut(&frame).map(|(rendered, used)| {
            *used = clock;
            rendered.clone()
        })
    }

    /// The cached frame closest to `frame`, at most `distance` away (the
    /// earlier one on ties).
    pub fn nearest(
        &mut self,
        key: CacheKey,
        frame: i64,
        distance: i64,
    ) -> Option<Arc<RenderedFrame>> {
        if self.key != Some(key) {
            return None;
        }
        let best = (0..=distance)
            .flat_map(|d| [frame - d, frame + d])
            .find(|f| self.frames.contains_key(f))?;
        self.get(key, best)
    }

    pub fn contains(&self, key: CacheKey, frame: i64) -> bool {
        self.key == Some(key) && self.frames.contains_key(&frame)
    }

    /// Adds a frame rendered for `key` (ignored if the key is stale).
    /// `protect` frames (around the playhead) are evicted last.
    pub fn insert(
        &mut self,
        key: CacheKey,
        rendered: Arc<RenderedFrame>,
        protect: std::ops::Range<i64>,
    ) {
        if self.key != Some(key) {
            return;
        }
        self.clock += 1;
        self.bytes += rendered.bytes();
        if let Some((old, _)) = self.frames.insert(rendered.frame, (rendered, self.clock)) {
            self.bytes -= old.bytes();
        }
        while self.bytes > self.budget && self.frames.len() > 1 {
            let victim = self
                .frames
                .iter()
                .min_by_key(|(frame, (_, used))| (protect.contains(frame), *used))
                .map(|(&frame, _)| frame)
                .expect("non-empty");
            if let Some((old, _)) = self.frames.remove(&victim) {
                self.bytes -= old.bytes();
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.frames.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FrameImage;

    fn frame(n: i64) -> Arc<RenderedFrame> {
        Arc::new(RenderedFrame {
            frame: n,
            exact: true,
            scopes: None,
            image: FrameImage::Pixels(nox_gpu::Image {
                width: 10,
                height: 10,
                stride: 40,
                data: vec![0; 400],
            }),
        })
    }

    #[test]
    fn evicts_least_recently_used_outside_the_protected_range() {
        let key = CacheKey {
            generation: 1,
            width: 10,
            height: 10,
        };
        let mut cache = FrameCache::new(1000); // two frames
        cache.validate(key);
        cache.insert(key, frame(1), 0..0);
        cache.insert(key, frame(2), 0..0);
        assert!(cache.get(key, 1).is_some()); // 1 is now more recent
        cache.insert(key, frame(3), 0..0);
        assert!(!cache.contains(key, 2));
        assert!(cache.contains(key, 1) && cache.contains(key, 3));
        // Protected frames survive even if older.
        cache.insert(key, frame(4), 1..2);
        assert!(cache.contains(key, 1) && cache.contains(key, 4));
        assert_eq!(cache.len(), 2);
        // A new key invalidates everything.
        let other = CacheKey {
            generation: 2,
            ..key
        };
        assert!(cache.get(other, 1).is_none());
        cache.validate(other);
        assert_eq!(cache.len(), 0);
        cache.insert(key, frame(5), 0..0); // stale, ignored
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn nearest_frame_within_a_distance() {
        let key = CacheKey {
            generation: 1,
            width: 10,
            height: 10,
        };
        let mut cache = FrameCache::new(1 << 20);
        cache.validate(key);
        for n in [10, 20, 30] {
            cache.insert(key, frame(n), 0..0);
        }
        let nearest = |cache: &mut FrameCache, f, d| cache.nearest(key, f, d).map(|r| r.frame);
        assert_eq!(nearest(&mut cache, 20, 0), Some(20));
        assert_eq!(nearest(&mut cache, 23, 5), Some(20));
        assert_eq!(nearest(&mut cache, 27, 5), Some(30));
        assert_eq!(nearest(&mut cache, 25, 5), Some(20)); // tie: earlier
        assert_eq!(nearest(&mut cache, 45, 5), None);
    }
}
