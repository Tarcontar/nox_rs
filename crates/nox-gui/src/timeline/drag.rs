//! Dragging: scrubbing, moving and trimming clips, fades, keyframes,
//! band selection, and the pointer's hover feedback.

use super::*;

impl imp::TimelineView {
    /// Sets the fade at `edge` of clip `item` to `seconds`, merging with
    /// earlier edits under `key`.
    pub fn set_fade(&self, item: ItemRef, edge: Edge, seconds: f64, key: &str) {
        let label = match edge {
            Edge::Start => crate::i18n::gettext("Fade In"),
            Edge::End => crate::i18n::gettext("Fade Out"),
        };
        let _ = self.project().edit_merging(&label, key, |t| {
            let clip = t
                .track_mut(item.track)
                .ok_or(EditError::NoTrack(item.track))?
                .children
                .get_mut(item.index)
                .and_then(Composable::as_clip_mut)
                .ok_or(EditError::NoItem {
                    track: item.track,
                    index: item.index,
                })?;
            match edge {
                Edge::Start => clip.item.set_fade_in(seconds),
                Edge::End => clip.item.set_fade_out(seconds),
            }
            Ok::<_, EditError>(())
        });
    }

    pub(super) fn hover(&self, x: f64, y: f64) {
        self.pointer_x.set(x);
        if self.drag.borrow().is_some() {
            return;
        }
        let hit = self.hit(x, y);
        let hovered = match &hit {
            Hit::Item { item, .. } | Hit::Fade { item, .. } => Some(*item),
            _ => None,
        };
        if hovered != self.hovered.replace(hovered) {
            self.obj().queue_draw();
        }
        let cursor = match hit {
            Hit::Fade { .. } => Some("ew-resize"),
            Hit::TransitionEdge { .. } => Some("col-resize"),
            Hit::Keyframe { .. } => Some("grab"),
            Hit::Item {
                edge: Some(_),
                roll: true,
                ..
            } => Some("ew-resize"),
            Hit::Item { edge: Some(_), .. } => Some("col-resize"),
            _ => None,
        };
        self.obj().set_cursor_from_name(cursor);
    }

    /// Starts a drag at `x`, `y`. With Ctrl held (`ripple`), dragging
    /// an edge ripple-trims instead of trimming or rolling, and clicking
    /// a clip adds it to or removes it from the selection; with Shift,
    /// clicking a clip adds it. Dragging on empty track space draws a
    /// rubber band; dragging the ruler scrubs.
    pub(super) fn drag_begin(&self, x: f64, y: f64, ripple: bool, extend: bool, alt: bool) {
        tracing::trace!("timeline press at {x:.0}, {y:.0}");
        let obj = self.obj();
        obj.grab_focus();
        let project = self.project().clone();
        let drag = match self.hit(x, y) {
            Hit::Keyframe { item, time } => {
                let timeline = project.timeline();
                let Ok(Composable::Clip(clip)) = timeline.item(item) else {
                    return;
                };
                let Some(trimmed) = clip.trimmed_range() else {
                    return;
                };
                // Between the neighboring keyframes, inside the clip.
                let fps = timeline.frame_rate().as_f64();
                let speed = clip.item.speed();
                let frame = speed / fps;
                let start = trimmed.start.as_seconds();
                let end = start + clip.duration().as_seconds() * speed;
                let times = clip.item.keyframe_times();
                let before = times
                    .iter()
                    .copied()
                    .filter(|&t| t < time - 1e-4)
                    .fold(f64::NAN, f64::max);
                let after = times
                    .iter()
                    .copied()
                    .filter(|&t| t > time + 1e-4)
                    .fold(f64::NAN, f64::min);
                let min = if before.is_nan() {
                    start
                } else {
                    before + frame
                };
                let max = if after.is_nan() {
                    end - frame
                } else {
                    after - frame
                };
                drop(timeline);
                self.set_selection(Some(item));
                self.fade_drags.set(self.fade_drags.get() + 1);
                Drag::Keyframe {
                    item,
                    origin: time,
                    at: time,
                    min,
                    max,
                    trimmed_start: start,
                    speed,
                    key: format!("keyframe-drag:{}", self.fade_drags.get()),
                }
            }
            Hit::TransitionEdge { item, edge } => {
                let start = match project.timeline().item(item) {
                    Ok(Composable::Transition(t)) => (t.in_offset + t.out_offset).as_seconds(),
                    _ => return,
                };
                self.set_selection(Some(item));
                self.fade_drags.set(self.fade_drags.get() + 1);
                Drag::TransitionLength {
                    item,
                    edge,
                    start,
                    key: format!("transition-drag:{}", self.fade_drags.get()),
                }
            }
            Hit::Fade { item, edge } => {
                let timeline = project.timeline();
                let (Ok(Composable::Clip(clip)), Ok(range)) =
                    (timeline.item(item), timeline.item_range(item))
                else {
                    return;
                };
                let (start, other) = match edge {
                    Edge::Start => (clip.item.fade_in(), clip.item.fade_out()),
                    Edge::End => (clip.item.fade_out(), clip.item.fade_in()),
                };
                let max = (range.duration.as_seconds() - other).max(0.0);
                drop(timeline);
                self.set_selection(Some(item));
                self.fade_drags.set(self.fade_drags.get() + 1);
                Drag::Fade {
                    item,
                    edge,
                    start,
                    max,
                    key: format!("fade-drag:{}", self.fade_drags.get()),
                }
            }
            Hit::Header { .. } => return,
            Hit::Ruler => {
                obj.scrub_to(x);
                Drag::Scrub
            }
            Hit::Marker(index) => {
                let Some(from) = project
                    .timeline()
                    .markers()
                    .get(index)
                    .map(|m| m.marked_range.start)
                else {
                    return;
                };
                self.select_marker(Some(index));
                obj.set_playhead(from.frame(project.frame_rate()));
                Drag::Marker {
                    index,
                    from,
                    to: from,
                    snap_points: self.snap_points(&[]),
                }
            }
            Hit::Track | Hit::Nothing => {
                let base = if ripple || extend {
                    self.selected_items()
                } else {
                    Vec::new()
                };
                self.select_items(base.clone());
                Drag::Band {
                    from: (x, y),
                    to: (x, y),
                    base,
                }
            }
            Hit::Item { item, edge, roll } => {
                let body = edge.is_none() && !roll;
                // Alt: slip, Alt+Shift: slide (one clip and its links).
                if body && alt {
                    self.set_selection(Some(item));
                    let timeline = project.timeline();
                    let locked = timeline
                        .linked(item)
                        .iter()
                        .any(|r| timeline.track(r.track).is_none_or(|t| t.item.locked()));
                    let (Ok(range), false) = (timeline.item_range(item), locked) else {
                        return;
                    };
                    let limits = if extend {
                        timeline
                            .slide_limits(item)
                            .map(|(min, max)| (Some(min), max))
                    } else {
                        timeline.slip_limits(item)
                    };
                    let Ok((min, max)) = limits else {
                        return;
                    };
                    drop(timeline);
                    self.drag.replace(Some(Drag::Shift {
                        item,
                        slide: extend,
                        range,
                        min,
                        max,
                        delta: Time::zero(range.start.timebase),
                    }));
                    return;
                }
                if body && ripple {
                    self.toggle_selected(item);
                    return;
                }
                if body && extend {
                    if !self.is_selected(item) {
                        let mut items = self.selected_items();
                        items.insert(0, item);
                        self.select_items(items);
                    }
                    return;
                }
                if body && self.is_selected(item) {
                    // Keep the selection; this clip becomes primary.
                    let mut items = self.selected_items();
                    items.retain(|&r| r != item);
                    items.insert(0, item);
                    self.select_items(items);
                } else {
                    self.set_selection(Some(item));
                }
                let timeline = project.timeline();
                // Transitions are selected (to delete them), not dragged,
                // and clips on locked tracks stay where they are.
                if matches!(timeline.item(item), Ok(Composable::Transition(_)))
                    || timeline.track(item.track).is_none_or(|t| t.item.locked())
                {
                    return;
                }
                let Ok(range) = timeline.item_range(item) else {
                    return;
                };
                let original = |edge: Edge| match edge {
                    Edge::Start => range.start,
                    Edge::End => range.end(),
                };
                match edge {
                    Some(edge) if roll && !ripple => {
                        let Ok((min, max)) = timeline.roll_limits(item, edge) else {
                            return;
                        };
                        drop(timeline);
                        Drag::Roll {
                            item,
                            edge,
                            cut: original(edge),
                            min,
                            max,
                            snap_points: self.snap_points(&[item]),
                            to: original(edge),
                            snapped: None,
                        }
                    }
                    Some(edge) => {
                        let limits = if ripple {
                            timeline.ripple_limits(item, edge)
                        } else {
                            timeline
                                .track(item.track)
                                .expect("track")
                                .trim_limits(item.index, edge)
                        };
                        let Ok((min, max)) = limits else {
                            return;
                        };
                        drop(timeline);
                        Drag::Trim {
                            item,
                            edge,
                            ripple,
                            range,
                            min,
                            max,
                            snap_points: self.snap_points(&[item]),
                            to: original(edge),
                            snapped: None,
                        }
                    }
                    None => {
                        // Every selected clip moves; transitions and
                        // clips on locked tracks keep the rest in place.
                        let items = self.selected_items();
                        let movable = items.iter().all(|&r| {
                            matches!(timeline.item(r), Ok(Composable::Clip(_)))
                                && timeline.track(r.track).is_some_and(|t| !t.item.locked())
                        });
                        if !movable {
                            return;
                        }
                        let mut linked: Vec<(ItemRef, TimeRange)> = Vec::new();
                        for &selected in &items {
                            for r in timeline.linked(selected) {
                                if !linked.iter().any(|(other, _)| *other == r)
                                    && let Ok(range) = timeline.item_range(r)
                                {
                                    linked.push((r, range));
                                }
                            }
                        }
                        let single_track = items.iter().all(|r| r.track == item.track);
                        let kind = timeline.track(item.track).expect("track").kind.clone();
                        drop(timeline);
                        let refs: Vec<ItemRef> = linked.iter().map(|(r, _)| *r).collect();
                        Drag::Move {
                            item,
                            items,
                            single_track,
                            linked,
                            range,
                            kind,
                            snap_points: self.snap_points(&refs),
                            offset: Time::zero(range.start.timebase),
                            target_track: item.track,
                            snapped: None,
                        }
                    }
                }
            }
        };
        self.drag.replace(Some(drag));
    }

    pub(super) fn drag_update(&self, x: f64, y: f64, dx: f64, dy: f64) {
        let fade = match self.drag.borrow().as_ref() {
            Some(Drag::Fade {
                item,
                edge,
                start,
                max,
                key,
            }) => Some((*item, *edge, *start, *max, key.clone())),
            _ => None,
        };
        let keyframe = match self.drag.borrow().as_ref() {
            Some(Drag::Keyframe {
                item,
                origin,
                at,
                min,
                max,
                trimmed_start,
                speed,
                key,
            }) => Some((
                *item,
                *origin,
                *at,
                *min,
                *max,
                *trimmed_start,
                *speed,
                key.clone(),
            )),
            _ => None,
        };
        if let Some((item, origin, at, min, max, trimmed_start, speed, key)) = keyframe {
            // Whole frames of the timeline, in media time.
            let fps = self.fps();
            let local = (origin - trimmed_start) / speed + dx / self.pixels_per_frame.get() / fps;
            let to = (trimmed_start + (local * fps).round() / fps * speed).clamp(min, max.max(min));
            if (to - at).abs() > 1e-9 {
                let moved = self.project().edit_merging(
                    &crate::i18n::gettext("Move Keyframe"),
                    &key,
                    |t| {
                        let clip = t
                            .track_mut(item.track)
                            .and_then(|track| track.children.get_mut(item.index))
                            .and_then(Composable::as_clip_mut)
                            .ok_or(EditError::NoItem {
                                track: item.track,
                                index: item.index,
                            })?;
                        clip.item.move_keyframes(at, to);
                        Ok::<_, EditError>(())
                    },
                );
                if moved.is_ok()
                    && let Some(Drag::Keyframe { at, .. }) = self.drag.borrow_mut().as_mut()
                {
                    *at = to;
                }
            }
            return;
        }
        let transition = match self.drag.borrow().as_ref() {
            Some(Drag::TransitionLength {
                item,
                edge,
                start,
                key,
            }) => Some((*item, *edge, *start, key.clone())),
            _ => None,
        };
        if let Some((item, edge, start, key)) = transition {
            let fps = self.project().frame_rate();
            let moved = dx / self.pixels_per_frame.get() / fps.as_f64();
            let seconds = match edge {
                Edge::End => start + 2.0 * moved,
                Edge::Start => start - 2.0 * moved,
            };
            let frames = (seconds * fps.as_f64()).round().max(2.0) as i64;
            let length = Time::from_frames(frames, fps);
            let _ = self.project().edit_merging(
                &crate::i18n::gettext("Change Transition Length"),
                &key,
                |t| t.set_transition_length(item, length),
            );
            return;
        }
        if let Some((item, edge, start, max, key)) = fade {
            // Inwards lengthens the fade; whole frames.
            let fps = self.fps();
            let seconds = dx / self.pixels_per_frame.get() / fps;
            let seconds = match edge {
                Edge::Start => start + seconds,
                Edge::End => start - seconds,
            };
            let seconds = ((seconds.clamp(0.0, max) * fps).round() / fps).min(max);
            self.set_fade(item, edge, seconds, &key);
            return;
        }
        let mut drag = self.drag.borrow_mut();
        let Some(drag) = drag.as_mut() else { return };
        let frames = dx / self.pixels_per_frame.get();
        match drag {
            Drag::Scrub => self.obj().scrub_to(x + dx),
            // Applied above, outside the borrow.
            Drag::Fade { .. } | Drag::TransitionLength { .. } | Drag::Keyframe { .. } => {}
            Drag::Shift {
                slide,
                min,
                max,
                delta,
                ..
            } => {
                // Slipping right shows earlier media, as with Premiere's
                // slip tool; sliding right moves the clip right.
                let moved = if *slide { frames } else { -frames };
                let mut time = self.frame_time(moved.round() as i64);
                if let Some(min) = min {
                    time = time.max(*min);
                }
                if let Some(max) = max {
                    time = time.min(*max);
                }
                *delta = time;
            }
            Drag::Marker {
                from,
                to,
                snap_points,
                ..
            } => {
                let candidate = self.time_to_frames(*from) + frames;
                let snapped = self.snap(snap_points, &[candidate]).map(|(_, p)| p);
                let frame = snapped.unwrap_or(candidate).round().max(0.0) as i64;
                *to = self.frame_time(frame);
            }
            Drag::Band { from, to, base } => {
                *to = (x + dx, y + dy);
                let found = self.clips_in_band(*from, *to);
                let mut items = base.clone();
                items.extend(found.into_iter().filter(|r| !base.contains(r)));
                self.select_items(items);
            }
            Drag::Move {
                linked,
                single_track,
                range,
                kind,
                snap_points,
                offset,
                target_track,
                snapped,
                ..
            } => {
                let start = self.time_to_frames(range.start) + frames;
                let length = self.time_to_frames(range.duration);
                let (mut new_start, snap) = match self.snap(snap_points, &[start, start + length]) {
                    Some((0, point)) => (point, Some(point)),
                    Some((_, point)) => (point - length, Some(point)),
                    None => (start.round(), None),
                };
                // Linked clips can't move before zero either.
                let earliest = linked
                    .iter()
                    .map(|(_, r)| self.time_to_frames(r.start))
                    .fold(f64::INFINITY, f64::min);
                let lead = self.time_to_frames(range.start) - earliest;
                new_start = new_start.max(lead);
                *offset = self.frame_time(new_start.round() as i64) - range.start;
                if snap.is_some_and(|s| {
                    (s - new_start).abs() < 1e-6 || (s - new_start - length).abs() < 1e-6
                }) {
                    *snapped = snap;
                } else {
                    *snapped = None;
                }
                let project = self.project();
                let timeline = project.timeline();
                if let Some(track) = self.track_at_y(y + dy)
                    && *single_track
                    && timeline
                        .track(track)
                        .is_some_and(|t| t.kind == *kind && !t.item.locked())
                {
                    *target_track = track;
                }
            }
            Drag::Trim {
                edge,
                range,
                min,
                max,
                snap_points,
                to,
                snapped,
                ..
            } => {
                let original = match edge {
                    Edge::Start => range.start,
                    Edge::End => range.end(),
                };
                let candidate = self.time_to_frames(original) + frames;
                let snap = self.snap(snap_points, &[candidate]).map(|(_, p)| p);
                let frame = snap.unwrap_or(candidate).round() as i64;
                let one_frame = self.frame_time(1);
                let mut time = self.frame_time(frame);
                // Keep at least one frame and stay within the limits.
                time = match edge {
                    Edge::Start => time.min(range.end() - one_frame).max(*min),
                    Edge::End => time.max(range.start + one_frame),
                };
                if let Some(max) = max {
                    time = time.min(*max);
                }
                *to = time;
                *snapped = snap.filter(|&s| (s - self.time_to_frames(time)).abs() < 1e-6);
            }
            Drag::Roll {
                cut,
                min,
                max,
                snap_points,
                to,
                snapped,
                ..
            } => {
                let candidate = self.time_to_frames(*cut) + frames;
                let snap = self.snap(snap_points, &[candidate]).map(|(_, p)| p);
                let frame = snap.unwrap_or(candidate).round() as i64;
                let time = self.frame_time(frame).max(*min).min(*max);
                *to = time;
                *snapped = snap.filter(|&s| (s - self.time_to_frames(time)).abs() < 1e-6);
            }
        }
        self.obj().queue_draw();
    }

    pub(super) fn drag_end(&self) {
        let Some(drag) = self.drag.take() else { return };
        let project = self.project().clone();
        match drag {
            Drag::Scrub
            | Drag::Band { .. }
            | Drag::Fade { .. }
            | Drag::TransitionLength { .. }
            | Drag::Keyframe { .. } => {}
            Drag::Shift {
                item, slide, delta, ..
            } => {
                if !delta.is_zero() {
                    let result = if slide {
                        project.edit(&crate::i18n::gettext("Slide Clip"), |t| {
                            t.slide(item, delta)
                        })
                    } else {
                        project
                            .edit(&crate::i18n::gettext("Slip Clip"), |t| t.slip(item, delta))
                            .map(|()| item)
                    };
                    if let Ok(item) = result {
                        self.set_selection(Some(item));
                    }
                }
            }
            Drag::Marker {
                index, from, to, ..
            } => {
                if to != from {
                    let moved = project.edit(&crate::i18n::gettext("Move Marker"), |t| {
                        t.move_marker(index, to)
                    });
                    if let Ok(index) = moved {
                        self.select_marker(Some(index));
                        self.obj().set_playhead(to.frame(project.frame_rate()));
                    }
                }
            }
            Drag::Move {
                item,
                items,
                offset,
                target_track,
                ..
            } => {
                if !offset.is_zero() || target_track != item.track {
                    let label =
                        crate::i18n::ngettext("Move Clip", "Move Clips", items.len() as u32);
                    let moved =
                        project.edit(&label, |t| t.move_items(&items, item, target_track, offset));
                    if let Ok(moved) = moved {
                        self.select_items(moved);
                    }
                }
            }
            Drag::Trim {
                item,
                edge,
                ripple,
                range,
                to,
                ..
            } => {
                let original = match edge {
                    Edge::Start => range.start,
                    Edge::End => range.end(),
                };
                if to != original {
                    let start = if ripple {
                        let _ = project.edit(&crate::i18n::gettext("Ripple Trim"), |t| {
                            t.ripple_trim(item, edge, to)
                        });
                        range.start
                    } else {
                        let _ = project.edit(&crate::i18n::gettext("Trim Clip"), |t| {
                            t.trim_item(item, edge, to)
                        });
                        match edge {
                            Edge::Start => to,
                            Edge::End => range.start,
                        }
                    };
                    self.set_selection(project.timeline().item_at(item.track, start));
                }
            }
            Drag::Roll {
                item,
                edge,
                cut,
                to,
                ..
            } => {
                if to != cut {
                    let _ = project.edit(&crate::i18n::gettext("Roll Edit"), |t| {
                        t.roll(item, edge, to)
                    });
                    let start = match edge {
                        Edge::Start => to,
                        Edge::End => project.timeline().item_range(item).map_or(to, |r| r.start),
                    };
                    self.set_selection(project.timeline().item_at(item.track, start));
                }
            }
        }
        self.obj().queue_draw();
    }
}
