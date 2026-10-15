//! Where things are: frames ↔ x, rows ↔ y, what's under the pointer,
//! snapping, and scrolling and zooming.

use super::*;

impl imp::TimelineView {
    pub(super) fn clip_rect(&self, range: TimeRange, y: f32) -> graphene::Rect {
        let x0 = self.frame_to_x(self.time_to_frames(range.start));
        let x1 = self.frame_to_x(self.time_to_frames(range.end()));
        // Keep tiny clips visible.
        let width = (x1 - x0).max(2.0);
        graphene::Rect::new(x0 as f32, y + 2.0, width as f32, TRACK_HEIGHT - 5.0)
    }

    /// Widget x coordinate of a (fractional) frame position.
    pub(super) fn frame_to_x(&self, frame: f64) -> f64 {
        HEADER_WIDTH as f64 + (frame - self.scroll.get()) * self.pixels_per_frame.get()
    }

    pub(super) fn x_to_frame_f64(&self, x: f64) -> f64 {
        (x - HEADER_WIDTH as f64) / self.pixels_per_frame.get() + self.scroll.get()
    }

    pub fn x_to_frame(&self, x: f64) -> i64 {
        self.x_to_frame_f64(x).round().max(0.0) as i64
    }

    /// Stack index of the track at widget y.
    pub(super) fn track_at_y(&self, y: f64) -> Option<usize> {
        if y < RULER_HEIGHT as f64 {
            return None;
        }
        let row = ((y - RULER_HEIGHT as f64) / TRACK_HEIGHT as f64) as usize;
        rows(&self.project().timeline()).get(row).copied()
    }

    pub(super) fn hit(&self, x: f64, y: f64) -> Hit {
        if y < RULER_HEIGHT as f64 {
            let project = self.project();
            let timeline = project.timeline();
            let near = timeline.markers().iter().position(|m| {
                let mx = self.frame_to_x(self.time_to_frames(m.marked_range.start));
                (mx - x).abs() <= 6.0
            });
            return near.map_or(Hit::Ruler, Hit::Marker);
        }
        let Some(track_index) = self.track_at_y(y) else {
            return Hit::Nothing;
        };
        let in_row = (y - RULER_HEIGHT as f64) % TRACK_HEIGHT as f64;
        if x < HEADER_WIDTH as f64 {
            return Hit::Header { track: track_index };
        }
        let project = self.project();
        let timeline = project.timeline();
        let track = timeline.track(track_index).expect("track");
        if in_row < FADE_GRAB_HEIGHT {
            for (index, (range, child)) in track.child_ranges().enumerate() {
                let Composable::Clip(clip) = child else {
                    continue;
                };
                let item = ItemRef {
                    track: track_index,
                    index,
                };
                if !self.shows_fade_handles(item, range) {
                    continue;
                }
                for edge in [Edge::Start, Edge::End] {
                    let handle = self.fade_handle_x(range, clip, edge);
                    if (x - handle).abs() <= FADE_GRAB {
                        return Hit::Fade { item, edge };
                    }
                }
            }
        }
        // Keyframe diamonds along the bottom of clips.
        if in_row >= TRACK_HEIGHT as f64 - 14.0 {
            for (index, (range, child)) in track.child_ranges().enumerate() {
                let Composable::Clip(clip) = child else {
                    continue;
                };
                let near = clip.item.keyframe_times().into_iter().find(|&time| {
                    self.keyframe_x(clip, range, time)
                        .is_some_and(|kx| (kx - x).abs() <= 5.0)
                });
                if let Some(time) = near {
                    let item = ItemRef {
                        track: track_index,
                        index,
                    };
                    return Hit::Keyframe { item, time };
                }
            }
        }
        // Transitions are drawn over the lower half of the cut.
        if in_row >= TRACK_HEIGHT as f64 / 2.0 {
            for (index, (range, child)) in track.child_ranges().enumerate() {
                if !matches!(child, Composable::Transition(_)) {
                    continue;
                }
                let x0 = self.frame_to_x(self.time_to_frames(range.start));
                let x1 = self.frame_to_x(self.time_to_frames(range.end()));
                let item = ItemRef {
                    track: track_index,
                    index,
                };
                // Edges of transitions wide enough to grab in the middle.
                if x1 - x0 > 3.0 * EDGE_GRAB {
                    if (x - x0).abs() <= EDGE_GRAB {
                        return Hit::TransitionEdge {
                            item,
                            edge: Edge::Start,
                        };
                    }
                    if (x - x1).abs() <= EDGE_GRAB {
                        return Hit::TransitionEdge {
                            item,
                            edge: Edge::End,
                        };
                    }
                }
                if x >= x0.min(x1 - 4.0) && x <= x1.max(x0 + 4.0) {
                    return Hit::Item {
                        item,
                        edge: None,
                        roll: false,
                    };
                }
            }
        }
        for (index, (range, child)) in track.child_ranges().enumerate() {
            if child.is_gap() || matches!(child, Composable::Transition(_)) {
                continue;
            }
            let x0 = self.frame_to_x(self.time_to_frames(range.start));
            let x1 = self.frame_to_x(self.time_to_frames(range.end()));
            if x < x0 - EDGE_GRAB || x > x1 + EDGE_GRAB {
                continue;
            }
            let item = ItemRef {
                track: track_index,
                index,
            };
            // Edges only for clips, and only if the clip is wide enough
            // to still be grabbed in the middle.
            let trimmable = matches!(child, Composable::Clip(_)) && x1 - x0 > 3.0 * EDGE_GRAB;
            let edge = if trimmable && (x - x0).abs() <= EDGE_GRAB {
                Edge::Start
            } else if trimmable && (x - x1).abs() <= EDGE_GRAB {
                Edge::End
            } else if x < x0 || x > x1 {
                continue;
            } else {
                return Hit::Item {
                    item,
                    edge: None,
                    roll: false,
                };
            };
            let cut = timeline.is_cut(item, edge);
            // Past the end of a clip at a cut: the next clip's start.
            if edge == Edge::End && x > x1 && cut {
                continue;
            }
            let edge_x = if edge == Edge::Start { x0 } else { x1 };
            return Hit::Item {
                item,
                edge: Some(edge),
                roll: cut && (x - edge_x).abs() <= ROLL_GRAB,
            };
        }
        Hit::Track
    }

    /// Whether clip `item` (at `range`) shows its fade handles: when
    /// selected or under the pointer, and wide enough.
    pub(super) fn shows_fade_handles(&self, item: ItemRef, range: TimeRange) -> bool {
        let width = (self.time_to_frames(range.duration) * self.pixels_per_frame.get()).abs();
        width >= FADE_MIN_WIDTH && (self.is_selected(item) || self.hovered.get() == Some(item))
    }

    /// Widget x of the fade handle at `edge` of `clip` (at `range`):
    /// where its fade ends.
    pub(super) fn fade_handle_x(
        &self,
        range: TimeRange,
        clip: &nox_project::Clip,
        edge: Edge,
    ) -> f64 {
        let pixels_per_second = self.fps() * self.pixels_per_frame.get();
        match edge {
            Edge::Start => {
                self.frame_to_x(self.time_to_frames(range.start))
                    + clip.item.fade_in() * pixels_per_second
            }
            Edge::End => {
                self.frame_to_x(self.time_to_frames(range.end()))
                    - clip.item.fade_out() * pixels_per_second
            }
        }
    }

    /// Widget x of a keyframe at media time `time` of `clip` (at
    /// `range`), if it is within the clip. Sped-up clips pass their
    /// media faster; frozen ones show no keyframes.
    pub(super) fn keyframe_x(
        &self,
        clip: &nox_project::Clip,
        range: TimeRange,
        time: f64,
    ) -> Option<f64> {
        let speed = clip.item.speed();
        let trimmed = clip.trimmed_range()?;
        if speed <= 0.0 {
            return None;
        }
        let local = (time - trimmed.start.as_seconds()) / speed;
        if local < -1e-9 || local > range.duration.as_seconds() + 1e-9 {
            return None;
        }
        Some(self.frame_to_x(self.time_to_frames(range.start) + local * self.fps()))
    }

    /// Snap targets in frames: item edges (of `timeline` with `exclude`
    /// removed) and the playhead.
    pub(super) fn snap_points(&self, exclude: &[ItemRef]) -> Vec<f64> {
        if !self.snapping.get() {
            return Vec::new();
        }
        let project = self.project();
        let mut timeline = project.timeline().clone();
        // Lift from the back so earlier indices stay valid.
        let mut exclude = exclude.to_vec();
        exclude.sort_by_key(|r| std::cmp::Reverse(r.index));
        for item in exclude {
            if let (Ok(range), Some(track)) =
                (timeline.item_range(item), timeline.track_mut(item.track))
            {
                let _ = track.lift(range);
            }
        }
        let mut points: Vec<f64> = timeline
            .edit_points()
            .into_iter()
            .map(|t| self.time_to_frames(t))
            .collect();
        points.push(self.playhead.get() as f64);
        points
    }

    /// The snap point closest to any of `candidates` (in frames), within
    /// the snapping distance: `(candidate index, point)`.
    pub(super) fn snap(&self, points: &[f64], candidates: &[f64]) -> Option<(usize, f64)> {
        let limit = SNAP_DISTANCE / self.pixels_per_frame.get();
        candidates
            .iter()
            .enumerate()
            .flat_map(|(i, &c)| points.iter().map(move |&p| (i, p, (p - c).abs())))
            .filter(|&(_, _, d)| d <= limit)
            .min_by(|a, b| a.2.total_cmp(&b.2))
            .map(|(i, p, _)| (i, p))
    }

    /// Frame for dropping media at widget x, snapped.
    pub(super) fn drop_frame(&self, x: f64) -> i64 {
        let frame = self.x_to_frame_f64(x);
        let points = self.snap_points(&[]);
        match self.snap(&points, &[frame]) {
            Some((_, point)) => point.round() as i64,
            None => frame.round().max(0.0) as i64,
        }
    }

    pub(super) fn on_scroll(&self, zoom: bool, dx: f64, dy: f64) {
        if zoom {
            // Keep the frame under the pointer in place.
            let x = self.pointer_x.get().max(HEADER_WIDTH as f64);
            let anchor = self.x_to_frame_f64(x);
            let ppf = (self.pixels_per_frame.get() * 1.15f64.powf(-dy)).clamp(0.02, 40.0);
            self.pixels_per_frame.set(ppf);
            let scroll = anchor - (x - HEADER_WIDTH as f64) / ppf;
            self.scroll.set(scroll.max(0.0));
        } else {
            let delta = (dx + dy) * 40.0 / self.pixels_per_frame.get();
            self.scroll.set((self.scroll.get() + delta).max(0.0));
        }
        self.obj().queue_draw();
    }
}
