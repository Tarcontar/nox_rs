//! Drawing the timeline: ruler, marks and markers, tracks with their
//! clips (filmstrips, waveforms, fades, keyframes), transitions, and what
//! a drag would do.

use super::*;

impl imp::TimelineView {
    pub(super) fn draw_ruler(&self, snapshot: &gtk::Snapshot, width: f32) {
        let ppf = self.pixels_per_frame.get();
        let fps = self.project().frame_rate();
        let nominal = nox_project::nominal_fps(fps) as i64;
        let step = [1, 2, 5, 10]
            .into_iter()
            .chain([1, 2, 5, 10, 30, 60, 120, 300, 600, 1800, 3600].map(|s| s * nominal))
            .find(|&s| s as f64 * ppf >= MIN_TICK_SPACING)
            .unwrap_or(3600 * nominal);
        let minor = if step % 5 == 0 { step / 5 } else { step };

        let first = (self.scroll.get() / minor as f64).floor() as i64 * minor;
        let mut frame = first.max(0);
        loop {
            let x = self.frame_to_x(frame as f64) as f32;
            if x >= width {
                break;
            }
            let major = frame % step == 0;
            let tick = if major { 10.0 } else { 4.0 };
            snapshot.append_color(
                &TICK,
                &graphene::Rect::new(x, RULER_HEIGHT - tick, 1.0, tick),
            );
            if major {
                // Centered over its tick; the first one kept clear of
                // the track headers.
                let layout = self
                    .obj()
                    .create_pango_layout(Some(&self.project().timecode(frame)));
                let width = layout.pixel_size().0 as f32;
                let left = (x - width / 2.0).max(HEADER_WIDTH + 2.0);
                snapshot.save();
                snapshot.translate(&graphene::Point::new(left, 2.0));
                snapshot.append_layout(&layout, &TEXT);
                snapshot.restore();
            }
            frame += minor;
        }
    }

    /// Timeline markers as flags on the ruler (the selected one outlined;
    /// one being dragged where it goes).
    pub(super) fn draw_markers(
        &self,
        snapshot: &gtk::Snapshot,
        timeline: &Timeline,
        drag: Option<&Drag>,
    ) {
        for (index, marker) in timeline.markers().iter().enumerate() {
            let time = match drag {
                Some(Drag::Marker { index: i, to, .. }) if *i == index => *to,
                _ => marker.marked_range.start,
            };
            let x = self.frame_to_x(self.time_to_frames(time)) as f32;
            let flag = graphene::Rect::new(x - 4.0, RULER_HEIGHT - 12.0, 9.0, 9.0);
            snapshot.append_color(&marker_color(&marker.color), &flag);
            snapshot.append_color(
                &with_alpha(marker_color(&marker.color), 0.8),
                &graphene::Rect::new(x - 0.5, RULER_HEIGHT - 3.0, 1.5, 3.0),
            );
            if self.marker.get() == Some(index) {
                outline(snapshot, flag, 0.0, 1.5, gdk::RGBA::WHITE);
            }
        }
    }

    /// The In/Out marks: the range on the ruler, with brackets at the
    /// marks, and everything outside it shaded.
    pub(super) fn draw_marks(
        &self,
        snapshot: &gtk::Snapshot,
        timeline: &Timeline,
        width: f32,
        height: f32,
    ) {
        let settings = &timeline.settings;
        if settings.mark_in.is_none() && settings.mark_out.is_none() {
            return;
        }
        let x_of = |time: Time| self.frame_to_x(self.time_to_frames(time)) as f32;
        let x0 = settings.mark_in.map_or(HEADER_WIDTH, x_of);
        let x1 = settings.mark_out.map_or(width, x_of);
        let tracks = height - RULER_HEIGHT;
        if x0 > HEADER_WIDTH {
            snapshot.append_color(
                &OUTSIDE_MARKS,
                &graphene::Rect::new(HEADER_WIDTH, RULER_HEIGHT, x0 - HEADER_WIDTH, tracks),
            );
        }
        if x1 < width {
            snapshot.append_color(
                &OUTSIDE_MARKS,
                &graphene::Rect::new(x1, RULER_HEIGHT, width - x1, tracks),
            );
        }
        snapshot.append_color(
            &with_alpha(MARK, 0.3),
            &graphene::Rect::new(x0, RULER_HEIGHT - 8.0, (x1 - x0).max(0.0), 8.0),
        );
        // Brackets: [ at In, ] at Out.
        let bracket = |x: f32, inward: f32| {
            snapshot.append_color(&MARK, &graphene::Rect::new(x - 1.0, 0.0, 2.0, height));
            let foot = if inward > 0.0 { x - 1.0 } else { x - 5.0 };
            snapshot.append_color(&MARK, &graphene::Rect::new(foot, 0.0, 6.0, 2.0));
            snapshot.append_color(
                &MARK,
                &graphene::Rect::new(foot, RULER_HEIGHT - 2.0, 6.0, 2.0),
            );
        };
        if settings.mark_in.is_some() {
            bracket(x0, 1.0);
        }
        if settings.mark_out.is_some() {
            bracket(x1, -1.0);
        }
    }

    pub(super) fn draw_track(
        &self,
        snapshot: &gtk::Snapshot,
        timeline: &Timeline,
        index: usize,
        y: f32,
        drag: Option<&Drag>,
    ) {
        let track = timeline.track(index).expect("track");
        let moving: Vec<ItemRef> = match drag {
            Some(Drag::Move { linked, .. }) => linked.iter().map(|(r, _)| *r).collect(),
            Some(Drag::Trim { item, .. }) => vec![*item],
            _ => Vec::new(),
        };
        for (i, (range, child)) in track.child_ranges().enumerate() {
            let item = ItemRef {
                track: index,
                index: i,
            };
            let offline = match child {
                Composable::Clip(clip) => clip
                    .media_reference()
                    .and_then(|r| timeline.media_path(r))
                    .is_some_and(|path| self.offline.borrow().contains(&path)),
                _ => false,
            };
            let color = match child {
                Composable::Gap(_) => continue,
                // Drawn below, over both neighbors.
                Composable::Transition(_) => continue,
                _ if offline => OFFLINE_CLIP,
                Composable::Clip(_) if track.kind == TrackKind::Video => VIDEO_CLIP,
                Composable::Clip(_) => AUDIO_CLIP,
                _ => NESTED_CLIP,
            };
            // Clips of muted or hidden tracks are dimmed like disabled clips.
            let enabled = child.item().is_none_or(|i| i.enabled) && track.item.enabled;
            let alpha = if moving.contains(&item) {
                0.35
            } else if enabled {
                1.0
            } else {
                0.5
            };
            let selected = self.is_selected(item) && moving.is_empty();
            let mut label = clip_label(child);
            if offline {
                label += &format!(" · {}", crate::i18n::gettext("Offline"));
            }
            self.draw_clip(
                snapshot,
                range,
                y,
                &label,
                with_alpha(color, alpha),
                selected,
            );
            let preferences = crate::preferences::Preferences::get();
            if let Composable::Clip(clip) = child
                && !offline
            {
                match track.kind {
                    TrackKind::Audio if preferences.timeline_waveforms() => {
                        self.draw_waveform(snapshot, timeline, clip, range, y, alpha);
                    }
                    TrackKind::Video if preferences.timeline_thumbnails() => {
                        self.draw_thumbnails(snapshot, timeline, clip, range, y, alpha);
                    }
                    _ => {}
                }
            }
            if let Composable::Clip(clip) = child
                && !moving.contains(&item)
            {
                self.draw_fades(snapshot, item, clip, range, y);
                self.draw_keyframes(snapshot, clip, range, y);
            }
            // Markers of the clip (from imported files), in its media's
            // time.
            if let Composable::Clip(clip) = child
                && let Some(trimmed) = clip.trimmed_range()
            {
                for marker in &clip.item.markers {
                    let offset = marker.marked_range.start - trimmed.start;
                    if offset.is_negative() || offset > trimmed.duration {
                        continue;
                    }
                    let x = self.frame_to_x(self.time_to_frames(range.start + offset)) as f32;
                    snapshot.append_color(
                        &marker_color(&marker.color),
                        &graphene::Rect::new(x - 2.0, y + 3.0, 5.0, 5.0),
                    );
                }
            }
        }
        for (i, (range, child)) in track.child_ranges().enumerate() {
            if let Composable::Transition(_) = child {
                let selected = self.is_selected(ItemRef {
                    track: index,
                    index: i,
                });
                self.draw_transition(snapshot, range, y, selected);
            }
        }
        if track.item.locked() {
            // Diagonal stripes over the whole row, as other editors do.
            let width = self.obj().width() as f32;
            let row =
                graphene::Rect::new(HEADER_WIDTH, y, width - HEADER_WIDTH, TRACK_HEIGHT - 1.0);
            snapshot.push_repeat(&row, Some(&graphene::Rect::new(0.0, y, 12.0, TRACK_HEIGHT)));
            for step in 0..12 {
                let x = step as f32;
                let rect =
                    graphene::Rect::new(x, y + x * TRACK_HEIGHT / 12.0, 2.0, TRACK_HEIGHT / 12.0);
                snapshot.append_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.3), &rect);
            }
            snapshot.pop();
        }
    }

    pub(super) fn draw_clip(
        &self,
        snapshot: &gtk::Snapshot,
        range: TimeRange,
        y: f32,
        name: &str,
        color: gdk::RGBA,
        selected: bool,
    ) {
        let rect = self.clip_rect(range, y);
        if rect.x() > self.obj().width() as f32 || rect.x() + rect.width() < HEADER_WIDTH {
            return;
        }
        let rounded = gsk::RoundedRect::from_rect(rect, 3.0);
        snapshot.push_rounded_clip(&rounded);
        snapshot.append_color(&color, &rect);
        if rect.width() > 16.0 {
            self.draw_text(
                snapshot,
                name,
                rect.x().max(HEADER_WIDTH) + 5.0,
                y + 5.0,
                &with_alpha(CLIP_TEXT, color.alpha()),
            );
        }
        snapshot.pop();
        let (border, width) = if selected {
            (SELECTION, 2.0)
        } else {
            (CLIP_BORDER, 1.0)
        };
        snapshot.append_border(&rounded, &[width; 4], &[border; 4]);
    }

    /// The clip's waveform below its name: the peak level of each pixel
    /// column's stretch of media, scaled by the clip's volume.
    pub(super) fn draw_waveform(
        &self,
        snapshot: &gtk::Snapshot,
        timeline: &Timeline,
        clip: &nox_project::Clip,
        range: TimeRange,
        y: f32,
        alpha: f32,
    ) {
        let rect = self.clip_rect(range, y);
        let (Some(path), Some(trimmed), Some(waveforms)) = (
            clip.media_reference().and_then(|r| timeline.media_path(r)),
            clip.trimmed_range(),
            self.waveforms.get(),
        ) else {
            return;
        };
        let left = rect.x().max(HEADER_WIDTH).floor();
        let right = (rect.x() + rect.width()).min(self.obj().width() as f32);
        if right - left < 2.0 {
            return;
        }
        let Some(waveform) = waveforms.get(&path) else {
            return;
        };
        // Seconds into the file at widget x.
        let file_start = clip
            .available_range()
            .map_or(trimmed.start, |available| trimmed.start - available.start);
        let fps = self.fps();
        let start_frame = self.time_to_frames(range.start);
        // Sped-up clips play their media faster; freeze frames none.
        let speed = clip.item.speed();
        let seconds_at = |x: f32| {
            (self.x_to_frame_f64(f64::from(x)) - start_frame) / fps * speed
                + file_start.as_seconds()
        };
        let top = rect.y() + 16.0;
        let height = rect.y() + rect.height() - 2.0 - top;
        let middle = top + height / 2.0;
        let gain = clip.item.volume().min(1.0) as f32;
        let color = with_alpha(CLIP_TEXT, 0.45 * alpha);
        let mut x = left;
        while x < right {
            let peak = waveform.peak(seconds_at(x), seconds_at(x + 1.0)) * gain;
            let bar = (peak * height).max(if peak > 0.0 { 1.0 } else { 0.0 });
            snapshot.append_color(
                &color,
                &graphene::Rect::new(x, middle - bar / 2.0, 1.0, bar),
            );
            x += 1.0;
        }
    }

    /// The clip's filmstrip below its name: thumbnails side by side
    /// from the clip's start, each of the media at its left edge.
    pub(super) fn draw_thumbnails(
        &self,
        snapshot: &gtk::Snapshot,
        timeline: &Timeline,
        clip: &nox_project::Clip,
        range: TimeRange,
        y: f32,
        alpha: f32,
    ) {
        let rect = self.clip_rect(range, y);
        let (Some(path), Some(trimmed), Some(thumbnails)) = (
            clip.media_reference().and_then(|r| timeline.media_path(r)),
            clip.trimmed_range(),
            self.thumbnails.get(),
        ) else {
            return;
        };
        // Inside the clip's border (2 px when selected).
        let left = (rect.x() + 2.0).max(HEADER_WIDTH);
        let right = (rect.x() + rect.width() - 2.0).min(self.obj().width() as f32);
        let top = rect.y() + 16.0;
        let height = rect.y() + rect.height() - 2.0 - top;
        let width = (height * thumbnails.aspect(&path)).round();
        if right - left < 4.0 || height < 8.0 || thumbnails.failed(&path) {
            return;
        }
        // Seconds into the file at widget x, as for waveforms.
        let file_start = clip
            .available_range()
            .map_or(trimmed.start, |available| trimmed.start - available.start);
        let fps = self.fps();
        let start_frame = self.time_to_frames(range.start);
        let speed = clip.item.speed();
        let seconds_at = |x: f32| {
            (self.x_to_frame_f64(f64::from(x)) - start_frame) / fps * speed
                + file_start.as_seconds()
        };
        // Times are rounded to a power of two milliseconds near the
        // length a thumbnail covers, so zooming reuses thumbnails.
        let covered = (seconds_at(width) - seconds_at(0.0)).abs() * 1000.0;
        let step = (covered.max(40.0).log2().ceil().exp2() as i64).max(1);
        let area = graphene::Rect::new(left, top, right - left, height);
        snapshot.push_clip(&area);
        snapshot.push_opacity(f64::from(alpha));
        let first = ((left - rect.x()) / width).floor().max(0.0) as i64;
        for i in first.. {
            let x = rect.x() + i as f32 * width;
            if x >= right {
                break;
            }
            let ms = (seconds_at(x).max(0.0) * 1000.0) as i64 / step * step;
            if let Some(texture) = thumbnails.get(&path, ms) {
                snapshot.append_texture(&texture, &graphene::Rect::new(x, top, width, height));
            }
        }
        snapshot.pop();
        snapshot.pop();
    }

    pub(super) fn draw_transition(
        &self,
        snapshot: &gtk::Snapshot,
        range: TimeRange,
        y: f32,
        selected: bool,
    ) {
        let rect = self.clip_rect(range, y);
        let rect = graphene::Rect::new(
            rect.x(),
            y + TRACK_HEIGHT / 2.0,
            rect.width(),
            TRACK_HEIGHT / 2.0 - 3.0,
        );
        snapshot.append_color(&with_alpha(CLIP_TEXT, 0.35), &rect);
        if selected {
            outline(snapshot, rect, 2.0, 2.0, SELECTION);
        }
    }

    /// The moving or trimmed clips at their new position, and the snap
    /// line.
    pub(super) fn draw_drag(
        &self,
        snapshot: &gtk::Snapshot,
        timeline: &Timeline,
        rows: &[usize],
        drag: Option<&Drag>,
        height: f32,
    ) {
        let row_of = |track: usize| rows.iter().position(|&i| i == track);
        let snapped = match drag {
            Some(Drag::Move {
                item,
                linked,
                offset,
                target_track,
                snapped,
                ..
            }) => {
                for (r, range) in linked {
                    let track = if r == item { *target_track } else { r.track };
                    let (Some(row), Ok(child)) = (row_of(track), timeline.item(*r)) else {
                        continue;
                    };
                    let kind = &timeline.track(track).expect("track").kind;
                    let color = if *kind == TrackKind::Video {
                        VIDEO_CLIP
                    } else {
                        AUDIO_CLIP
                    };
                    let moved = TimeRange::new(range.start + *offset, range.duration);
                    self.draw_clip(snapshot, moved, row_y(row), child.name(), color, true);
                }
                *snapped
            }
            Some(Drag::Trim {
                item,
                edge,
                ripple,
                range,
                to,
                snapped,
                ..
            }) => {
                if let (Some(row), Ok(child)) = (row_of(item.track), timeline.item(*item)) {
                    let trimmed = match edge {
                        // A ripple-trimmed start stays in place.
                        Edge::Start if *ripple => TimeRange::new(range.start, range.end() - *to),
                        Edge::Start => TimeRange::from_start_end(*to, range.end()),
                        Edge::End => TimeRange::from_start_end(range.start, *to),
                    };
                    let kind = &timeline.track(item.track).expect("track").kind;
                    let color = if *kind == TrackKind::Video {
                        VIDEO_CLIP
                    } else {
                        AUDIO_CLIP
                    };
                    self.draw_clip(snapshot, trimmed, row_y(row), child.name(), color, true);
                }
                *snapped
            }
            Some(Drag::Roll {
                item, to, snapped, ..
            }) => {
                if let Some(row) = row_of(item.track) {
                    let x = self.frame_to_x(self.time_to_frames(*to)) as f32;
                    snapshot.append_color(
                        &SELECTION,
                        &graphene::Rect::new(x - 1.5, row_y(row), 3.0, TRACK_HEIGHT - 1.0),
                    );
                }
                *snapped
            }
            Some(Drag::Shift {
                item,
                slide,
                range,
                delta,
                ..
            }) => {
                if let (Some(row), Ok(child)) = (row_of(item.track), timeline.item(*item)) {
                    let fps = timeline.frame_rate();
                    let frames = delta.frame(fps);
                    let (shown, label) = if *slide {
                        (
                            TimeRange::new(range.start + *delta, range.duration),
                            crate::i18n::fill(
                                &crate::i18n::gettext("Slide {frames}"),
                                &[("frames", &format!("{frames:+}"))],
                            ),
                        )
                    } else {
                        (
                            *range,
                            crate::i18n::fill(
                                &crate::i18n::gettext("Slip {frames}"),
                                &[("frames", &format!("{frames:+}"))],
                            ),
                        )
                    };
                    let kind = &timeline.track(item.track).expect("track").kind;
                    let color = if *kind == TrackKind::Video {
                        VIDEO_CLIP
                    } else {
                        AUDIO_CLIP
                    };
                    let text = format!("{} · {label}", child.name()); // no-i18n
                    self.draw_clip(snapshot, shown, row_y(row), &text, color, true);
                }
                None
            }
            Some(Drag::Band { from, to, .. }) => {
                let (x0, y0) = (from.0.min(to.0) as f32, from.1.min(to.1) as f32);
                let (x1, y1) = (from.0.max(to.0) as f32, from.1.max(to.1) as f32);
                let band = graphene::Rect::new(x0, y0, x1 - x0, y1 - y0);
                snapshot.append_color(&with_alpha(SELECTION, 0.15), &band);
                outline(snapshot, band, 0.0, 1.0, SELECTION);
                None
            }
            _ => None,
        };
        if let Some(frame) = snapped {
            let x = self.frame_to_x(frame) as f32;
            snapshot.append_color(
                &SNAP_LINE,
                &graphene::Rect::new(x - 0.5, RULER_HEIGHT, 1.0, height),
            );
        }
    }

    pub(super) fn draw_text(
        &self,
        snapshot: &gtk::Snapshot,
        text: &str,
        x: f32,
        y: f32,
        color: &gdk::RGBA,
    ) {
        let layout = self.obj().create_pango_layout(Some(text));
        snapshot.save();
        snapshot.translate(&graphene::Point::new(x, y));
        snapshot.append_layout(&layout, color);
        snapshot.restore();
    }

    /// Small diamonds along a clip's bottom edge where it has keyframes
    /// (of any property).
    pub(super) fn draw_keyframes(
        &self,
        snapshot: &gtk::Snapshot,
        clip: &nox_project::Clip,
        range: TimeRange,
        y: f32,
    ) {
        let times = clip.item.keyframe_times();
        if times.is_empty() {
            return;
        }
        let rect = self.clip_rect(range, y);
        let bottom = rect.y() + rect.height() - 5.0;
        let path = gsk::PathBuilder::new();
        for time in times {
            let Some(x) = self.keyframe_x(clip, range, time) else {
                continue;
            };
            let x = x as f32;
            path.move_to(x, bottom - 3.5);
            path.line_to(x + 3.5, bottom);
            path.line_to(x, bottom + 3.5);
            path.line_to(x - 3.5, bottom);
            path.close();
        }
        snapshot.append_fill(&path.to_path(), gsk::FillRule::Winding, &KEYFRAME);
    }

    /// A clip's fade ramps (the faded part shaded above a line rising
    /// to full level), and its handles if it shows them.
    pub(super) fn draw_fades(
        &self,
        snapshot: &gtk::Snapshot,
        item: ItemRef,
        clip: &nox_project::Clip,
        range: TimeRange,
        y: f32,
    ) {
        let rect = self.clip_rect(range, y);
        let (top, bottom) = (rect.y(), rect.y() + rect.height());
        let (x0, x1) = (rect.x(), rect.x() + rect.width());
        let line = with_alpha(CLIP_TEXT, 0.7);
        for edge in [Edge::Start, Edge::End] {
            let (corner, end) = match edge {
                Edge::Start => (x0, self.fade_handle_x(range, clip, edge) as f32),
                Edge::End => (x1, self.fade_handle_x(range, clip, edge) as f32),
            };
            if (end - corner).abs() < 0.5 {
                continue;
            }
            let shade = gsk::PathBuilder::new();
            shade.move_to(corner, top);
            shade.line_to(end, top);
            shade.line_to(corner, bottom);
            shade.close();
            snapshot.append_fill(&shade.to_path(), gsk::FillRule::Winding, &FADE_SHADE);
            let ramp = gsk::PathBuilder::new();
            ramp.move_to(corner, bottom);
            ramp.line_to(end, top);
            snapshot.append_stroke(&ramp.to_path(), &gsk::Stroke::new(1.0), &line);
        }
        if self.shows_fade_handles(item, range) {
            for edge in [Edge::Start, Edge::End] {
                let x = self.fade_handle_x(range, clip, edge) as f32;
                let x = x.clamp(x0 + 3.0, x1 - 3.0);
                let handle = graphene::Rect::new(x - 3.5, top, 7.0, 7.0);
                snapshot.append_color(&CLIP_TEXT, &handle);
                outline(snapshot, handle, 0.0, 1.0, CLIP_BORDER);
            }
        }
    }
}

/// Draws the outline of `rect`, with corners rounded by `radius`.
fn outline(
    snapshot: &gtk::Snapshot,
    rect: graphene::Rect,
    radius: f32,
    width: f32,
    color: gdk::RGBA,
) {
    snapshot.append_border(
        &gsk::RoundedRect::from_rect(rect, radius),
        &[width; 4],
        &[color; 4],
    );
}
