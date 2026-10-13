//! Editing operations on tracks and timelines.
//!
//! Edits overwrite: placing an item replaces whatever was in its range,
//! splitting items at the edges, and empty space is filled with gaps.
//! Transitions touching an edited range are removed, since their neighbors
//! change. After every edit a track is normalized: adjacent gaps merged,
//! empty and trailing gaps removed.
//!
//! The ripple edits (insert, ripple delete, ripple trim) are the exception:
//! they move everything after the edit on the tracks involved. A roll edit
//! moves the cut between two clips, so nothing else moves.
//!
//! Locked tracks (`Item::locked`) reject every edit of their contents.

use crate::metadata::Object;
use crate::time::{Time, TimeRange};
use crate::timeline::{Composable, Gap, Timeline, Track, TrackKind, Transition};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EditError {
    #[error("there is no track {0}")]
    NoTrack(usize),
    #[error("track {track} has no item {index}")]
    NoItem { track: usize, index: usize },
    #[error("only clips can be trimmed")]
    NotTrimmable,
    #[error("the edit would start before the beginning of the timeline")]
    BeforeStart,
    #[error("the trim goes beyond the media or a neighboring item")]
    OutOfRange,
    #[error("{0} items cannot go on {1} tracks")]
    WrongTrackKind(String, String),
    #[error("there is no clip on the other side of this cut")]
    NoCut,
    #[error("the clips have no media beyond the cut to dissolve with")]
    NoHandles,
    #[error("track {0} is locked")]
    Locked(usize),
}

/// An item on a top-level track: its stack index and its index in the track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ItemRef {
    pub track: usize,
    pub index: usize,
}

/// Which end of an item a trim moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Start,
    End,
}

impl Track {
    /// Makes sure an item boundary exists at `time`, splitting the item
    /// there or padding the end with a gap. Returns the index of the first
    /// child at or after `time`.
    pub fn split_at(&mut self, time: Time) -> usize {
        let mut start = Time::zero(time.timebase);
        for i in 0..self.children.len() {
            let duration = self.children[i].duration();
            let end = start + duration;
            if time <= start {
                return i;
            }
            if time < end && !duration.is_zero() {
                let (head, tail) = split(&self.children[i], time - start);
                self.children[i] = head;
                self.children.insert(i + 1, tail);
                return i + 1;
            }
            start = end;
        }
        if time > start {
            self.children.push(Composable::Gap(Gap::new(time - start)));
        }
        self.children.len()
    }

    /// Places `item` at `at`, replacing everything in its range.
    pub fn overwrite(&mut self, item: Composable, at: Time) -> Result<(), EditError> {
        if at.is_negative() {
            return Err(EditError::BeforeStart);
        }
        let range = TimeRange::new(at, item.duration());
        self.remove_transitions_touching(range);
        let first = self.split_at(range.start);
        let last = self.split_at(range.end());
        self.children.splice(first..last, [item]);
        self.normalize();
        Ok(())
    }

    /// Inserts `item` at `at`, pushing everything after it later.
    pub fn insert(&mut self, item: Composable, at: Time) -> Result<(), EditError> {
        if at.is_negative() {
            return Err(EditError::BeforeStart);
        }
        self.remove_transitions_touching(TimeRange::new(at, Time::zero(at.timebase)));
        let index = self.split_at(at);
        self.children.insert(index, item);
        self.normalize();
        Ok(())
    }

    /// Replaces everything in `range` with a gap.
    pub fn lift(&mut self, range: TimeRange) -> Result<(), EditError> {
        self.overwrite(Composable::Gap(Gap::new(range.duration)), range.start)
    }

    /// Removes child `index`, leaving a gap. Returns the removed child.
    pub fn remove(&mut self, index: usize) -> Option<Composable> {
        let child = self.children.get(index)?.clone();
        if matches!(child, Composable::Transition(_)) {
            self.children.remove(index);
        } else {
            let range = self.range_of_child(index);
            self.lift(range).ok()?;
        }
        Some(child)
    }

    /// How far each edge of clip `index` can be trimmed: earliest and latest
    /// position in track time (`None`: unlimited). Limited by neighboring
    /// items (only gaps can be overwritten), the start of the track and the
    /// media's available range. The item must keep a positive duration.
    pub fn trim_limits(&self, index: usize, edge: Edge) -> Result<(Time, Option<Time>), EditError> {
        let clip = self
            .children
            .get(index)
            .and_then(Composable::as_clip)
            .ok_or(EditError::NotTrimmable)?;
        let range = self.range_of_child(index);
        let source = clip.trimmed_range().ok_or(EditError::NotTrimmable)?;
        let available = clip.available_range();
        let neighbor = |step: isize| {
            let mut i = index as isize + step;
            while let Some(child) = usize::try_from(i).ok().and_then(|i| self.children.get(i)) {
                if !matches!(child, Composable::Transition(_)) {
                    return Some((i as usize, child));
                }
                i += step;
            }
            None
        };
        Ok(match edge {
            Edge::Start => {
                let mut min = match neighbor(-1) {
                    Some((i, child)) if child.is_gap() => self.range_of_child(i).start,
                    _ => range.start,
                };
                if let Some(available) = available {
                    min = min.max(range.start - (source.start - available.start));
                }
                (min, Some(range.end()))
            }
            Edge::End => {
                let mut max = match neighbor(1) {
                    Some((i, child)) if child.is_gap() => Some(self.range_of_child(i).end()),
                    Some(_) => Some(range.end()),
                    None => None,
                };
                if let Some(available) = available {
                    let media_end = range.end() + (available.end() - source.end());
                    max = Some(max.map_or(media_end, |max| max.min(media_end)));
                }
                (range.start, max)
            }
        })
    }

    /// Moves one edge of clip `index` to track time `to`. Trimming in leaves
    /// a gap; trimming out overwrites the neighboring gap.
    pub fn trim(&mut self, index: usize, edge: Edge, to: Time) -> Result<(), EditError> {
        let (min, max) = self.trim_limits(index, edge)?;
        let out_of_range = match edge {
            Edge::Start => to < min || max.is_some_and(|max| to >= max),
            Edge::End => to <= min || max.is_some_and(|max| to > max),
        };
        if out_of_range {
            return Err(EditError::OutOfRange);
        }
        let range = self.range_of_child(index);
        let mut clip = self.children[index].as_clip().expect("clip").clone();
        let mut source = clip.trimmed_range().expect("range");
        let new_range = match edge {
            Edge::Start => {
                source.start += to - range.start;
                TimeRange::from_start_end(to, range.end())
            }
            Edge::End => TimeRange::from_start_end(range.start, to),
        };
        source.duration = new_range.duration;
        clip.item.source_range = Some(source);
        self.lift(range)?;
        self.overwrite(Composable::Clip(clip), new_range.start)
    }

    /// Removes everything in `range` and closes the gap: later items move
    /// back by its duration.
    pub fn ripple_delete(&mut self, range: TimeRange) {
        self.remove_transitions_touching(range);
        let first = self.split_at(range.start);
        let last = self.split_at(range.end());
        self.children.drain(first..last);
        self.normalize();
    }

    /// How far each edge of clip `index` can be ripple-trimmed, in the
    /// terms of [`Track::trim_limits`]: `to` is where the edge is dragged.
    /// Neighbors don't limit a ripple trim, since they move along; only
    /// the media does (and for stills, the start of the track).
    pub fn ripple_trim_limits(
        &self,
        index: usize,
        edge: Edge,
    ) -> Result<(Time, Option<Time>), EditError> {
        let clip = self
            .children
            .get(index)
            .and_then(Composable::as_clip)
            .ok_or(EditError::NotTrimmable)?;
        let range = self.range_of_child(index);
        let source = clip.trimmed_range().ok_or(EditError::NotTrimmable)?;
        let available = clip.available_range();
        Ok(match edge {
            Edge::Start => {
                let min = match available {
                    Some(available) => range.start - (source.start - available.start),
                    None => Time::zero(range.start.timebase),
                };
                (min, Some(range.end()))
            }
            Edge::End => {
                let max = available.map(|available| range.end() + (available.end() - source.end()));
                (range.start, max)
            }
        })
    }

    /// Ripple-trims one edge of clip `index` to track time `to`: the clip
    /// gets longer or shorter and everything after it moves along. A
    /// trimmed start stays where it is; `to` only says how much media to
    /// add or remove there (as if the edge had been dragged to `to`).
    pub fn ripple_trim(&mut self, index: usize, edge: Edge, to: Time) -> Result<(), EditError> {
        let (min, max) = self.ripple_trim_limits(index, edge)?;
        let out_of_range = match edge {
            Edge::Start => to < min || max.is_some_and(|max| to >= max),
            Edge::End => to <= min || max.is_some_and(|max| to > max),
        };
        if out_of_range {
            return Err(EditError::OutOfRange);
        }
        let range = self.range_of_child(index);
        let clip = self.children[index].as_clip_mut().expect("clip");
        let mut source = clip.trimmed_range().expect("range");
        match edge {
            Edge::Start => {
                source.start += to - range.start;
                source.duration = range.end() - to;
            }
            Edge::End => source.duration = to - range.start,
        }
        clip.item.source_range = Some(source);
        self.remove_adjacent_transitions(index);
        self.normalize();
        Ok(())
    }

    /// Where the cut after clip `index` (between it and the next clip) can
    /// be rolled to: `min..=max`, limited by the media both clips have
    /// beyond the cut. The cut must also stay strictly inside the two
    /// clips, which the limits don't include.
    pub fn roll_limits(&self, index: usize) -> Result<(Time, Time), EditError> {
        let (left, right) = self.cut_after(index)?;
        let left_range = self.range_of_child(left);
        let right_range = self.range_of_child(right);
        let clip = |i: usize| self.children[i].as_clip().expect("clip");
        let (a, b) = (clip(left), clip(right));
        let (a_source, b_source) = (
            a.trimmed_range().ok_or(EditError::NotTrimmable)?,
            b.trimmed_range().ok_or(EditError::NotTrimmable)?,
        );
        let mut min = left_range.start;
        let mut max = right_range.end();
        if let Some(available) = a.available_range() {
            max = max.min(left_range.end() + (available.end() - a_source.end()));
        }
        if let Some(available) = b.available_range() {
            min = min.max(right_range.start - (b_source.start - available.start));
        }
        Ok((min, max))
    }

    /// Rolls the cut after clip `index` to track time `to`: the clip ends
    /// there and the next clip starts there, so nothing else moves. A
    /// transition at the cut is removed.
    pub fn roll(&mut self, index: usize, to: Time) -> Result<(), EditError> {
        let (min, max) = self.roll_limits(index)?;
        let (left, right) = self.cut_after(index)?;
        let left_range = self.range_of_child(left);
        let right_range = self.range_of_child(right);
        if to < min || to > max || to <= left_range.start || to >= right_range.end() {
            return Err(EditError::OutOfRange);
        }
        let delta = to - left_range.end();
        let a = self.children[left].as_clip_mut().expect("clip");
        let mut source = a.trimmed_range().expect("range");
        source.duration = to - left_range.start;
        a.item.source_range = Some(source);
        let b = self.children[right].as_clip_mut().expect("clip");
        let mut source = b.trimmed_range().expect("range");
        source.start += delta;
        source.duration = right_range.end() - to;
        b.item.source_range = Some(source);
        if right == left + 2 {
            self.children.remove(left + 1);
        }
        Ok(())
    }

    /// The clips on both sides of the cut after clip `index`, skipping a
    /// transition there.
    fn cut_after(&self, index: usize) -> Result<(usize, usize), EditError> {
        let is_clip = |i: usize| self.children.get(i).and_then(Composable::as_clip).is_some();
        if !is_clip(index) {
            return Err(EditError::NotTrimmable);
        }
        let right = match self.children.get(index + 1) {
            Some(Composable::Transition(_)) => index + 2,
            _ => index + 1,
        };
        if !is_clip(right) {
            return Err(EditError::NoCut);
        }
        Ok((index, right))
    }

    /// Removes the transitions right before and after child `index`.
    fn remove_adjacent_transitions(&mut self, index: usize) {
        let is_transition = |track: &Self, i: usize| {
            matches!(track.children.get(i), Some(Composable::Transition(_)))
        };
        if is_transition(self, index + 1) {
            self.children.remove(index + 1);
        }
        if index > 0 && is_transition(self, index - 1) {
            self.children.remove(index - 1);
        }
    }

    /// Merges adjacent gaps and drops empty and trailing gaps.
    pub fn normalize(&mut self) {
        let mut children: Vec<Composable> = Vec::with_capacity(self.children.len());
        for child in self.children.drain(..) {
            if child.is_gap() && child.duration().is_zero() {
                continue;
            }
            if let (Some(Composable::Gap(prev)), Composable::Gap(gap)) =
                (children.last_mut(), &child)
            {
                let duration = prev.duration() + gap.duration();
                let range = prev.item.source_range.as_mut().expect("gap duration");
                range.duration = duration;
                continue;
            }
            children.push(child);
        }
        while children.last().is_some_and(Composable::is_gap) {
            children.pop();
        }
        self.children = children;
    }

    /// Removes transitions that overlap or touch `range`.
    fn remove_transitions_touching(&mut self, range: TimeRange) {
        let doomed: Vec<usize> = self
            .child_ranges()
            .enumerate()
            .filter(|(_, (r, child))| {
                matches!(child, Composable::Transition(_))
                    && r.start <= range.end()
                    && range.start <= r.end()
            })
            .map(|(i, _)| i)
            .collect();
        for i in doomed.into_iter().rev() {
            self.children.remove(i);
        }
    }
}

/// Splits a clip, gap or nested composition `offset` into its duration.
fn split(child: &Composable, offset: Time) -> (Composable, Composable) {
    let duration = child.duration();
    let mut head = child.clone();
    let mut tail = child.clone();
    let range = match child {
        Composable::Clip(clip) => clip.trimmed_range(),
        Composable::Gap(gap) => gap.item.source_range,
        _ => None,
    }
    .unwrap_or(TimeRange::new(Time::zero(duration.timebase), duration));
    if let Some(item) = head.item_mut() {
        item.source_range = Some(TimeRange::new(range.start, offset));
    }
    if let Some(item) = tail.item_mut() {
        item.source_range = Some(TimeRange::new(range.start + offset, duration - offset));
        // Markers belong to whichever half contains them.
        item.markers
            .retain(|m| m.marked_range.start >= range.start + offset);
    }
    if let Some(item) = head.item_mut() {
        item.markers
            .retain(|m| m.marked_range.start < range.start + offset);
    }
    (head, tail)
}

impl Timeline {
    fn track_checked(&self, track: usize) -> Result<&Track, EditError> {
        self.track(track).ok_or(EditError::NoTrack(track))
    }

    /// A track to edit; locked tracks refuse.
    fn track_checked_mut(&mut self, track: usize) -> Result<&mut Track, EditError> {
        let target = self.track_mut(track).ok_or(EditError::NoTrack(track))?;
        if target.item.locked() {
            return Err(EditError::Locked(track));
        }
        Ok(target)
    }

    pub fn item(&self, item: ItemRef) -> Result<&Composable, EditError> {
        self.track_checked(item.track)?
            .children
            .get(item.index)
            .ok_or(EditError::NoItem {
                track: item.track,
                index: item.index,
            })
    }

    /// Where `item` sits in timeline time.
    pub fn item_range(&self, item: ItemRef) -> Result<TimeRange, EditError> {
        self.item(item)?;
        Ok(self.track_checked(item.track)?.range_of_child(item.index))
    }

    /// The item on `track` that covers `time`.
    pub fn item_at(&self, track: usize, time: Time) -> Option<ItemRef> {
        let (index, _) = self.track(track)?.child_at(time)?;
        Some(ItemRef { track, index })
    }

    /// `item` and every clip linked to it.
    pub fn linked(&self, item: ItemRef) -> Vec<ItemRef> {
        let link = self.item(item).ok().and_then(|c| c.item()?.link());
        let Some(link) = link else {
            return vec![item];
        };
        let mut items = vec![item];
        for (t, track) in self.tracks() {
            for (index, child) in track.children.iter().enumerate() {
                let other = ItemRef { track: t, index };
                if other != item && child.item().and_then(|i| i.link()) == Some(link) {
                    items.push(other);
                }
            }
        }
        items
    }

    /// A link id not used by any clip yet.
    pub fn new_link_id(&self) -> i64 {
        self.clips()
            .filter_map(|c| c.item.link())
            .max()
            .map_or(1, |id| id + 1)
    }

    /// Overwrites `item` onto `track` at `at`. Returns where it landed.
    pub fn place(
        &mut self,
        track: usize,
        item: Composable,
        at: Time,
    ) -> Result<ItemRef, EditError> {
        let is_empty = item.duration().is_zero();
        let target = self.track_checked_mut(track)?;
        target.overwrite(item, at)?;
        let index = if is_empty {
            target.split_at(at)
        } else {
            target.child_at(at).expect("placed item").0
        };
        Ok(ItemRef { track, index })
    }

    /// Moves `item` (and the clips linked to it) by `offset`, and `item`
    /// itself to `to_track`. Linked clips stay on their tracks. Returns the
    /// new position of `item`.
    pub fn move_item(
        &mut self,
        item: ItemRef,
        to_track: usize,
        offset: Time,
    ) -> Result<ItemRef, EditError> {
        let target_kind = self.track_checked(to_track)?.kind.clone();
        let source_kind = self.track_checked(item.track)?.kind.clone();
        if target_kind != source_kind {
            return Err(EditError::WrongTrackKind(
                source_kind.as_str().to_owned(),
                target_kind.as_str().to_owned(),
            ));
        }
        let mut moving = Vec::new();
        for r in self.linked(item) {
            let range = self.item_range(r)?;
            if (range.start + offset).is_negative() {
                return Err(EditError::BeforeStart);
            }
            let track = if r == item { to_track } else { r.track };
            moving.push((r, track, range, self.item(r)?.clone()));
        }
        for (r, _, range, _) in &moving {
            self.track_checked_mut(r.track)?.lift(*range)?;
        }
        let mut result = item;
        let mut result_start = None;
        for (r, track, range, child) in moving {
            let placed = self.place(track, child, range.start + offset)?;
            if r == item {
                result = placed;
                result_start = Some(range.start + offset);
            }
        }
        // Later placements on the same track may have shifted indices.
        Ok(result_start
            .and_then(|start| self.item_at(to_track, start))
            .unwrap_or(result))
    }

    /// Removes `item` and the clips linked to it, leaving gaps.
    pub fn remove_item(&mut self, item: ItemRef) -> Result<(), EditError> {
        let ranges = self
            .linked(item)
            .into_iter()
            .map(|r| Ok((r, self.item_range(r)?)))
            .collect::<Result<Vec<_>, EditError>>()?;
        for (r, range) in ranges {
            if matches!(self.item(r)?, Composable::Transition(_)) {
                self.track_checked_mut(r.track)?.remove(r.index);
            } else {
                self.track_checked_mut(r.track)?.lift(range)?;
            }
        }
        Ok(())
    }

    /// Trims one edge of `item` to timeline time `to`. Linked clips with the
    /// same edge at the same time are trimmed along if they can be.
    pub fn trim_item(&mut self, item: ItemRef, edge: Edge, to: Time) -> Result<(), EditError> {
        let range = self.item_range(item)?;
        let edge_time = |r: TimeRange| match edge {
            Edge::Start => r.start,
            Edge::End => r.end(),
        };
        let followers: Vec<(usize, Time)> = self
            .linked(item)
            .into_iter()
            .filter(|&r| r != item)
            .filter_map(|r| {
                let other = self.item_range(r).ok()?;
                (edge_time(other) == edge_time(range)).then_some((r.track, other.start))
            })
            .collect();
        self.track_checked_mut(item.track)?
            .trim(item.index, edge, to)?;
        for (track, start) in followers {
            // Ignore followers that can't follow (e.g. shorter audio).
            if let Some(index) = self.item_at(track, start).map(|r| r.index) {
                let _ = self.track_checked_mut(track)?.trim(index, edge, to);
            }
        }
        Ok(())
    }

    /// Removes `item` and the clips linked to it, closing the gaps on
    /// their tracks: later items on those tracks move back.
    pub fn ripple_remove_item(&mut self, item: ItemRef) -> Result<(), EditError> {
        let mut ranges = self
            .linked(item)
            .into_iter()
            .map(|r| Ok((r, self.item_range(r)?)))
            .collect::<Result<Vec<_>, EditError>>()?;
        // From the back, so earlier ranges on the same track stay valid.
        ranges.sort_by_key(|(r, range)| std::cmp::Reverse((range.start, r.index)));
        for (r, range) in ranges {
            if matches!(self.item(r)?, Composable::Transition(_)) {
                self.track_checked_mut(r.track)?.remove(r.index);
            } else {
                self.track_checked_mut(r.track)?.ripple_delete(range);
            }
        }
        Ok(())
    }

    /// The clips trimmed along with `item`: linked clips on other tracks
    /// with the same edge at the same time.
    fn edge_followers(&self, item: ItemRef, edge: Edge) -> Result<Vec<ItemRef>, EditError> {
        let range = self.item_range(item)?;
        let edge_time = |r: TimeRange| match edge {
            Edge::Start => r.start,
            Edge::End => r.end(),
        };
        Ok(self
            .linked(item)
            .into_iter()
            .filter(|r| r.track != item.track)
            .filter(|&r| {
                self.item_range(r)
                    .is_ok_and(|other| edge_time(other) == edge_time(range))
            })
            .collect())
    }

    /// Limits for ripple-trimming `item` and the linked clips that follow
    /// it (see [`Track::ripple_trim_limits`]); all of them must be able to.
    pub fn ripple_limits(
        &self,
        item: ItemRef,
        edge: Edge,
    ) -> Result<(Time, Option<Time>), EditError> {
        let mut limits = self
            .track_checked(item.track)?
            .ripple_trim_limits(item.index, edge)?;
        for r in self.edge_followers(item, edge)? {
            let (min, max) = self
                .track_checked(r.track)?
                .ripple_trim_limits(r.index, edge)?;
            limits.0 = limits.0.max(min);
            limits.1 = match (limits.1, max) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        Ok(limits)
    }

    /// Ripple-trims one edge of `item` to `to` (see [`Track::ripple_trim`]).
    /// Linked clips with the same edge are trimmed the same way, so they
    /// stay in sync; if one of them can't be, nothing is trimmed.
    pub fn ripple_trim(&mut self, item: ItemRef, edge: Edge, to: Time) -> Result<(), EditError> {
        let followers = self.edge_followers(item, edge)?;
        self.track_checked_mut(item.track)?
            .ripple_trim(item.index, edge, to)?;
        for r in followers {
            self.track_checked_mut(r.track)?
                .ripple_trim(r.index, edge, to)?;
        }
        Ok(())
    }

    /// Whether `edge` of clip `item` is a cut: another clip follows (or
    /// precedes) it directly, possibly with a transition between them.
    pub fn is_cut(&self, item: ItemRef, edge: Edge) -> bool {
        self.cut_index(item, edge).is_ok_and(|left| {
            self.track(item.track)
                .is_some_and(|t| t.cut_after(left).is_ok())
        })
    }

    /// The cut at `edge` of clip `item` as the index of the clip before it.
    fn cut_index(&self, item: ItemRef, edge: Edge) -> Result<usize, EditError> {
        let track = self.track_checked(item.track)?;
        self.item(item)?;
        match edge {
            Edge::End => Ok(item.index),
            Edge::Start => {
                let mut index = item.index.checked_sub(1).ok_or(EditError::NoCut)?;
                if matches!(track.children[index], Composable::Transition(_)) {
                    index = index.checked_sub(1).ok_or(EditError::NoCut)?;
                }
                Ok(index)
            }
        }
    }

    /// The cut at `edge` of `item`, and the same cut on the tracks of
    /// linked clips (where the clips on both sides are linked to the ones
    /// at this cut): `(track, index of the clip before it)`.
    fn linked_cuts(&self, item: ItemRef, edge: Edge) -> Result<Vec<(usize, usize)>, EditError> {
        let left = ItemRef {
            track: item.track,
            index: self.cut_index(item, edge)?,
        };
        let track = self.track_checked(left.track)?;
        let (_, right) = track.cut_after(left.index)?;
        let link = |track: &Track, i: usize| track.children[i].item().and_then(|i| i.link());
        let right_link = link(track, right);
        let mut cuts = vec![(left.track, left.index)];
        for r in self.edge_followers(left, Edge::End)? {
            let other = self.track_checked(r.track)?;
            if let Ok((_, other_right)) = other.cut_after(r.index)
                && right_link.is_some()
                && link(other, other_right) == right_link
            {
                cuts.push((r.track, r.index));
            }
        }
        Ok(cuts)
    }

    /// Where the cut at `edge` of clip `item` can be rolled to, inclusive:
    /// within the media of the clips on both sides, of linked clips with
    /// the same cut too, and leaving each clip at least a frame.
    pub fn roll_limits(&self, item: ItemRef, edge: Edge) -> Result<(Time, Time), EditError> {
        let frame = Time::from_frames(1, self.frame_rate());
        let mut limits: Option<(Time, Time)> = None;
        for (t, index) in self.linked_cuts(item, edge)? {
            let track = self.track_checked(t)?;
            let (mut min, mut max) = track.roll_limits(index)?;
            let (left, right) = track.cut_after(index)?;
            min = min.max(track.range_of_child(left).start + frame);
            max = max.min(track.range_of_child(right).end() - frame);
            limits = Some(match limits {
                Some((a, b)) => (a.max(min), b.min(max)),
                None => (min, max),
            });
        }
        let (min, max) = limits.ok_or(EditError::NoCut)?;
        if min > max {
            return Err(EditError::OutOfRange);
        }
        Ok((min, max))
    }

    /// Rolls the cut at `edge` of clip `item` to `to` (see
    /// [`Track::roll`]), on the tracks of linked clips too.
    pub fn roll(&mut self, item: ItemRef, edge: Edge, to: Time) -> Result<(), EditError> {
        for (track, index) in self.linked_cuts(item, edge)? {
            self.track_checked_mut(track)?.roll(index, to)?;
        }
        Ok(())
    }

    /// Splits every enabled clip that spans `time` on the unlocked
    /// top-level tracks into two. Linked clips stay linked to their own
    /// half. Returns how many clips were split.
    pub fn split_at(&mut self, time: Time) -> usize {
        let mut split = Vec::new();
        for (index, track) in self.tracks() {
            if !track.item.locked()
                && let Some((child, range)) = track.child_at(time)
                && range.start < time
                && track.children[child]
                    .as_clip()
                    .is_some_and(|c| c.item.enabled)
            {
                split.push(index);
            }
        }
        // Tails of linked clips get a new link per original link.
        let mut relinked: Vec<(i64, i64)> = Vec::new();
        let mut next_link = self.new_link_id();
        for &index in &split {
            let track = self.track_mut(index).expect("track");
            let tail = track.split_at(time);
            let item = track.children[tail].item_mut().expect("clip");
            if let Some(link) = item.link() {
                let new = match relinked.iter().find(|(old, _)| *old == link) {
                    Some(&(_, new)) => new,
                    None => {
                        relinked.push((link, next_link));
                        next_link += 1;
                        next_link - 1
                    }
                };
                item.set_link(Some(new));
            }
        }
        split.len()
    }

    /// Adds a dissolve at the cut on `edge` of clip `item` (replacing one
    /// that is there), about `duration` long and centered on the cut. Each
    /// side is limited by the media the clips have beyond the cut; if one
    /// side has less, the other takes the rest. Returns the transition.
    pub fn add_dissolve(
        &mut self,
        item: ItemRef,
        edge: Edge,
        duration: Time,
    ) -> Result<ItemRef, EditError> {
        let track = self.track_checked(item.track)?;
        self.item(item)?;
        // Children around the cut, skipping an existing transition.
        let step = |i: usize, forward: bool| {
            if forward {
                i.checked_add(1)
            } else {
                i.checked_sub(1)
            }
        };
        let forward = edge == Edge::End;
        let mut other = step(item.index, forward).ok_or(EditError::NoCut)?;
        let mut existing = None;
        if let Some(Composable::Transition(_)) = track.children.get(other) {
            existing = Some(other);
            other = step(other, forward).ok_or(EditError::NoCut)?;
        }
        let (left, right) = if forward {
            (item.index, other)
        } else {
            (other, item.index)
        };
        let clip = |i: usize| track.children.get(i).and_then(Composable::as_clip);
        let (Some(a), Some(b)) = (clip(left), clip(right)) else {
            return Err(EditError::NoCut);
        };
        let (Some(a_range), Some(b_range)) = (a.trimmed_range(), b.trimmed_range()) else {
            return Err(EditError::NoCut);
        };
        // Media beyond the cut: after the end of A, before the start of B.
        let a_tail = a
            .available_range()
            .map_or(a_range.duration, |r| r.end() - a_range.end())
            .min(b_range.duration);
        let b_head = b
            .available_range()
            .map_or(b_range.duration, |r| b_range.start - r.start)
            .min(a_range.duration);
        let zero = Time::zero(duration.timebase);
        let half = Time::new(duration.ticks / 2, duration.timebase);
        let mut in_offset = half.min(b_head).max(zero);
        let out_offset = (duration - in_offset).min(a_tail).max(zero);
        in_offset = (duration - out_offset).min(b_head).max(zero);
        if (in_offset + out_offset).is_zero() {
            return Err(EditError::NoHandles);
        }
        let transition = Composable::Transition(Transition {
            name: "Cross Dissolve".into(),
            metadata: Object::new(),
            transition_type: "SMPTE_Dissolve".into(),
            in_offset,
            out_offset,
            extra: Object::new(),
        });
        let track = self.track_checked_mut(item.track)?;
        let index = match existing {
            Some(index) => {
                track.children[index] = transition;
                index
            }
            None => {
                track.children.insert(right, transition);
                right
            }
        };
        Ok(ItemRef {
            track: item.track,
            index,
        })
    }

    /// Adds an empty track of `kind`, above the existing tracks of that kind
    /// for video and below them for audio. Returns its stack index.
    pub fn add_track(&mut self, kind: TrackKind) -> usize {
        let count = self.tracks().filter(|(_, t)| t.kind == kind).count();
        let prefix = match kind {
            TrackKind::Video => "V",
            TrackKind::Audio => "A",
            TrackKind::Other(_) => "T",
        };
        let track = Track::new(format!("{prefix}{}", count + 1), kind.clone());
        // Video stacks upwards, so new video tracks go after the last video
        // track; audio tracks are listed below video, so they go first.
        let index = match kind {
            TrackKind::Video => self
                .tracks()
                .filter(|(_, t)| t.kind == kind)
                .map(|(i, _)| i + 1)
                .next_back()
                .unwrap_or(self.tracks.children.len()),
            _ => self.tracks.children.len(),
        };
        self.tracks.children.insert(index, Composable::Track(track));
        index
    }

    /// Removes the track at stack index `index` with its contents. Clips
    /// linked to its clips stay where they are.
    pub fn remove_track(&mut self, index: usize) -> Result<Track, EditError> {
        self.track_checked_mut(index)?;
        match self.tracks.children.remove(index) {
            Composable::Track(track) => Ok(track),
            _ => unreachable!("checked above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Rational;
    use crate::timeline::{Clip, MediaReference, Transition};
    use std::path::Path;

    fn f(n: i64) -> Time {
        Time::from_frames(n, Rational::new(24, 1))
    }

    fn media(name: &str, length: i64) -> MediaReference {
        MediaReference::from_path(
            Path::new(&format!("/m/{name}.mov")),
            Some(TimeRange::new(f(0), f(length))),
        )
    }

    fn clip(name: &str, start: i64, duration: i64) -> Composable {
        Composable::Clip(Clip::new(
            name,
            media(name, 100),
            TimeRange::new(f(start), f(duration)),
        ))
    }

    /// The track as `name@start+duration` (gaps as `_`), with source starts
    /// for clips: `a[10]@0+5`.
    fn layout(track: &Track) -> String {
        track
            .child_ranges()
            .map(|(range, child)| {
                let (s, d) = (
                    range.start.frame(Rational::new(24, 1)),
                    range.duration.frame(Rational::new(24, 1)),
                );
                match child {
                    Composable::Clip(c) => format!(
                        "{}[{}]@{s}+{d}",
                        c.item.name,
                        c.trimmed_range().unwrap().start.frame(Rational::new(24, 1))
                    ),
                    Composable::Gap(_) => format!("_@{s}+{d}"),
                    Composable::Transition(_) => format!("x@{s}+{d}"),
                    _ => "?".into(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn track(children: Vec<Composable>) -> Track {
        let mut track = Track::new("V1", TrackKind::Video);
        track.children = children;
        track
    }

    #[test]
    fn overwrite_into_empty_track_pads_with_gap() {
        let mut t = track(vec![]);
        t.overwrite(clip("a", 0, 5), f(10)).unwrap();
        assert_eq!(layout(&t), "_@0+10 a[0]@10+5");
    }

    #[test]
    fn overwrite_splits_and_replaces() {
        let mut t = track(vec![clip("a", 0, 10), clip("b", 50, 10)]);
        t.overwrite(clip("c", 0, 6), f(7)).unwrap();
        assert_eq!(layout(&t), "a[0]@0+7 c[0]@7+6 b[53]@13+7");
    }

    #[test]
    fn overwrite_inside_one_clip() {
        let mut t = track(vec![clip("a", 0, 10)]);
        t.overwrite(clip("c", 0, 2), f(3)).unwrap();
        assert_eq!(layout(&t), "a[0]@0+3 c[0]@3+2 a[5]@5+5");
    }

    #[test]
    fn overwrite_rejects_negative_time() {
        let mut t = track(vec![]);
        assert_eq!(
            t.overwrite(clip("a", 0, 5), f(-1)),
            Err(EditError::BeforeStart)
        );
    }

    #[test]
    fn lift_merges_gaps_and_drops_trailing() {
        let mut t = track(vec![
            clip("a", 0, 5),
            Composable::Gap(Gap::new(f(5))),
            clip("b", 0, 5),
            clip("c", 0, 5),
        ]);
        t.remove(2).unwrap();
        assert_eq!(layout(&t), "a[0]@0+5 _@5+10 c[0]@15+5");
        t.remove(2).unwrap();
        assert_eq!(layout(&t), "a[0]@0+5");
    }

    #[test]
    fn insert_ripples() {
        let mut t = track(vec![clip("a", 0, 10)]);
        t.insert(clip("c", 0, 2), f(4)).unwrap();
        assert_eq!(layout(&t), "a[0]@0+4 c[0]@4+2 a[4]@6+6");
    }

    #[test]
    fn transitions_touching_an_edit_are_removed() {
        let dissolve = Composable::Transition(Transition {
            name: "d".into(),
            metadata: Default::default(),
            transition_type: "SMPTE_Dissolve".into(),
            in_offset: f(2),
            out_offset: f(2),
            extra: Default::default(),
        });
        let mut t = track(vec![
            clip("a", 0, 10),
            dissolve.clone(),
            clip("b", 0, 10),
            clip("c", 0, 10),
        ]);
        // An edit far away keeps it.
        t.overwrite(clip("d", 0, 2), f(25)).unwrap();
        assert_eq!(
            layout(&t),
            "a[0]@0+10 x@8+4 b[0]@10+10 c[0]@20+5 d[0]@25+2 c[7]@27+3"
        );
        // Overwriting the start of `b` removes it.
        t.overwrite(clip("e", 0, 1), f(10)).unwrap();
        assert!(
            !t.children
                .iter()
                .any(|c| matches!(c, Composable::Transition(_)))
        );
    }

    #[test]
    fn trim_limits_follow_gaps_and_media() {
        // a uses frames 10..20 of 100 available; gap 5 before, b right after.
        let mut t = track(vec![
            Composable::Gap(Gap::new(f(5))),
            clip("a", 10, 10),
            clip("b", 0, 10),
        ]);
        assert_eq!(t.trim_limits(1, Edge::Start).unwrap(), (f(0), Some(f(15))));
        assert_eq!(t.trim_limits(1, Edge::End).unwrap(), (f(5), Some(f(15))));
        // The last clip can extend to the end of its media.
        assert_eq!(t.trim_limits(2, Edge::End).unwrap(), (f(15), Some(f(115))));
        assert_eq!(t.trim_limits(0, Edge::End), Err(EditError::NotTrimmable));

        // a is at frame 5 using source 10: extending the start by 3 frames
        // reveals source frame 7.
        t.trim(1, Edge::Start, f(2)).unwrap();
        assert_eq!(layout(&t), "_@0+2 a[7]@2+13 b[0]@15+10");
        t.trim(1, Edge::Start, f(6)).unwrap();
        assert_eq!(layout(&t), "_@0+6 a[11]@6+9 b[0]@15+10");
        t.trim(2, Edge::End, f(20)).unwrap();
        assert_eq!(layout(&t), "_@0+6 a[11]@6+9 b[0]@15+5");
        t.trim(1, Edge::End, f(10)).unwrap();
        assert_eq!(layout(&t), "_@0+6 a[11]@6+4 _@10+5 b[0]@15+5");
        assert_eq!(t.trim(1, Edge::End, f(16)), Err(EditError::OutOfRange));
        assert_eq!(t.trim(1, Edge::End, f(6)), Err(EditError::OutOfRange));
        // Media limit: source starts at 11, so the start can move back 11 frames
        // but the gap only allows 6.
        assert_eq!(t.trim(1, Edge::Start, f(-1)), Err(EditError::OutOfRange));
    }

    fn timeline_with_linked_clip() -> Timeline {
        let mut timeline = Timeline::new("t", Rational::new(24, 1));
        let link = timeline.new_link_id();
        for track in [0, 1] {
            let mut c = clip("a", 0, 10);
            c.item_mut().unwrap().set_link(Some(link));
            timeline.place(track, c, f(5)).unwrap();
        }
        timeline
    }

    #[test]
    fn moving_takes_linked_clips_along() {
        let mut timeline = timeline_with_linked_clip();
        let v2 = timeline.add_track(TrackKind::Video);
        assert_eq!(v2, 1);
        assert_eq!(timeline.track(1).unwrap().item.name, "V2");
        let item = timeline.item_at(0, f(5)).unwrap();
        let moved = timeline.move_item(item, v2, f(3)).unwrap();
        assert_eq!(moved, ItemRef { track: 1, index: 1 });
        assert_eq!(layout(timeline.track(0).unwrap()), "");
        assert_eq!(layout(timeline.track(1).unwrap()), "_@0+8 a[0]@8+10");
        assert_eq!(layout(timeline.track(2).unwrap()), "_@0+8 a[0]@8+10");

        assert_eq!(
            timeline.move_item(moved, v2, f(-9)),
            Err(EditError::BeforeStart)
        );
        assert!(matches!(
            timeline.move_item(moved, 2, f(0)),
            Err(EditError::WrongTrackKind(..))
        ));
    }

    #[test]
    fn moving_within_a_track_overlapping_itself() {
        let mut timeline = Timeline::new("t", Rational::new(24, 1));
        timeline.place(0, clip("a", 0, 10), f(0)).unwrap();
        timeline.place(0, clip("b", 0, 10), f(10)).unwrap();
        let b = timeline.item_at(0, f(10)).unwrap();
        let moved = timeline.move_item(b, 0, f(-4)).unwrap();
        assert_eq!(moved, ItemRef { track: 0, index: 1 });
        assert_eq!(layout(timeline.track(0).unwrap()), "a[0]@0+6 b[0]@6+10");
    }

    #[test]
    fn removing_and_trimming_linked_clips() {
        let mut timeline = timeline_with_linked_clip();
        let video = timeline.item_at(0, f(5)).unwrap();
        timeline.trim_item(video, Edge::End, f(12)).unwrap();
        assert_eq!(layout(timeline.track(0).unwrap()), "_@0+5 a[0]@5+7");
        assert_eq!(layout(timeline.track(1).unwrap()), "_@0+5 a[0]@5+7");
        timeline.remove_item(video).unwrap();
        assert!(timeline.track(0).unwrap().children.is_empty());
        assert!(timeline.track(1).unwrap().children.is_empty());
    }

    #[test]
    fn edits_keep_the_sequence_timebase() {
        let fps = Rational::new(25, 1);
        let at = |n| Time::from_frames(n, fps);
        let mut t = track(vec![]);
        let clip = Composable::Clip(Clip::new(
            "a",
            media("a", 100),
            TimeRange::new(at(0), at(10)),
        ));
        t.overwrite(clip, at(7)).unwrap();
        t.trim(1, Edge::End, at(12)).unwrap();
        // What gets stored (and written to files) stays at 1/25.
        for (range, child) in t.child_ranges() {
            assert!(
                range.start.is_zero() || range.start.timebase == fps.inverse(),
                "{child:?}"
            );
            let stored = child.item().unwrap().source_range.unwrap();
            assert_eq!(stored.start.timebase, fps.inverse(), "{child:?}");
            assert_eq!(stored.duration.timebase, fps.inverse(), "{child:?}");
        }
    }

    #[test]
    fn splitting_keeps_halves_linked_separately() {
        let mut timeline = timeline_with_linked_clip();
        assert_eq!(timeline.split_at(f(5)), 0); // at an edge
        assert_eq!(timeline.split_at(f(9)), 2);
        assert_eq!(
            layout(timeline.track(0).unwrap()),
            "_@0+5 a[0]@5+4 a[4]@9+6"
        );
        let head = timeline.item_at(0, f(5)).unwrap();
        let tail = timeline.item_at(0, f(9)).unwrap();
        assert_eq!(timeline.linked(head).len(), 2);
        assert_eq!(timeline.linked(tail).len(), 2);
        assert!(!timeline.linked(head).contains(&tail));
        // Moving the tail leaves the heads in place.
        timeline.move_item(tail, 0, f(10)).unwrap();
        assert_eq!(
            layout(timeline.track(1).unwrap()),
            "_@0+5 a[0]@5+4 _@9+10 a[4]@19+6"
        );
    }

    #[test]
    fn dissolves_are_centered_and_limited_by_handles() {
        let mut timeline = Timeline::new("t", Rational::new(24, 1));
        // a uses 10..20 of 100 (10 frames of tail), b uses 3..13 (3 of head).
        timeline.place(0, clip("a", 10, 10), f(0)).unwrap();
        timeline.place(0, clip("b", 3, 10), f(10)).unwrap();
        let a = ItemRef { track: 0, index: 0 };
        let dissolve = timeline.add_dissolve(a, Edge::End, f(8)).unwrap();
        let offsets = |t: &Timeline, r: ItemRef| match t.item(r).unwrap() {
            Composable::Transition(t) => (t.in_offset, t.out_offset),
            _ => panic!("not a transition"),
        };
        // 4 + 4 wanted; b has only 3 frames of head, so a gives 5.
        assert_eq!(offsets(&timeline, dissolve), (f(3), f(5)));
        // Again from the other side replaces it.
        let b = ItemRef { track: 0, index: 2 };
        let again = timeline.add_dissolve(b, Edge::Start, f(2)).unwrap();
        assert_eq!(again, dissolve);
        assert_eq!(offsets(&timeline, again), (f(1), f(1)));
        assert_eq!(timeline.track(0).unwrap().children.len(), 3);
        // No neighbor, or no handles.
        assert_eq!(
            timeline.add_dissolve(a, Edge::Start, f(8)),
            Err(EditError::NoCut)
        );
        let mut tight = Timeline::new("t", Rational::new(24, 1));
        let whole = |name| {
            let reference = MediaReference::from_path(
                Path::new(&format!("/m/{name}.mov")),
                Some(TimeRange::new(f(0), f(10))),
            );
            Composable::Clip(Clip::new(name, reference, TimeRange::new(f(0), f(10))))
        };
        tight.place(0, whole("a"), f(0)).unwrap();
        tight.place(0, whole("b"), f(10)).unwrap();
        assert_eq!(
            tight.add_dissolve(a, Edge::End, f(8)),
            Err(EditError::NoHandles)
        );
    }

    #[test]
    fn dissolve_layers_and_audio_crossfade() {
        let fps = Rational::new(24, 1);
        let mut timeline = Timeline::new("t", fps);
        for track in [0, 1] {
            timeline.place(track, clip("a", 10, 10), f(0)).unwrap();
            timeline.place(track, clip("b", 10, 10), f(10)).unwrap();
            let a = ItemRef { track, index: 0 };
            timeline.add_dissolve(a, Edge::End, f(4)).unwrap();
        }
        // Before the dissolve: only a.
        let names = |t: f64| {
            timeline
                .video_layers(Time::new((t * 24.0) as i64, fps.inverse()))
                .iter()
                .map(|l| {
                    (
                        l.clip.item.name.clone(),
                        l.source_time.frame(fps),
                        (l.opacity * 100.0).round() as i64,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(names(7.0 / 24.0), [("a".into(), 17, 100)]);
        // The dissolve runs from frame 8 to 12; at 9 it is a quarter done.
        // b starts early: its source frame 10 is at the cut (frame 10).
        assert_eq!(
            names(9.0 / 24.0),
            [("a".into(), 19, 100), ("b".into(), 9, 25)]
        );
        assert_eq!(
            names(11.0 / 24.0),
            [("a".into(), 21, 100), ("b".into(), 11, 75)]
        );
        assert_eq!(names(12.0 / 24.0), [("b".into(), 12, 100)]);

        let range = TimeRange::new(f(0), f(20));
        let segments = timeline.audio_segments(range);
        assert_eq!(segments.len(), 2);
        let (a, b) = (&segments[0], &segments[1]);
        assert_eq!((a.place, a.fade_out), (TimeRange::new(f(0), f(12)), f(4)));
        assert_eq!(
            (b.place, b.source_start, b.fade_in),
            (TimeRange::new(f(8), f(12)), f(8), f(4))
        );
        // Equal power: the gains' squares add up to one across the fade.
        for frame in 8..12 {
            let t = frame as f64 / 24.0 + 0.01;
            let power = a.gain_at(t).powi(2) + b.gain_at(t).powi(2);
            assert!((power - 1.0).abs() < 1e-9, "{frame}: {power}");
        }
        assert_eq!(a.gain_at(0.1), 1.0);
        assert_eq!(b.gain_at(0.1), 0.0);
        // Only overlapping segments are listed.
        assert_eq!(
            timeline.audio_segments(TimeRange::new(f(13), f(2))).len(),
            1
        );
    }

    #[test]
    fn clip_properties_live_in_nox_metadata() {
        let mut c = clip("a", 0, 10);
        let item = c.item_mut().unwrap();
        assert_eq!((item.opacity(), item.gain()), (1.0, 1.0));
        item.set_opacity(0.5);
        item.set_blend_mode(crate::timeline::BlendMode::Screen);
        item.set_gain(0.25);
        item.set_link(Some(3));
        assert_eq!(item.opacity(), 0.5);
        assert_eq!(item.blend_mode(), crate::timeline::BlendMode::Screen);
        assert_eq!(item.gain(), 0.25);
        item.set_opacity(1.0);
        item.set_blend_mode(crate::timeline::BlendMode::Normal);
        item.set_gain(1.0);
        assert_eq!(item.nox_value("opacity"), None);
        assert_eq!(item.link(), Some(3));
        item.set_link(None);
        assert!(item.metadata.is_empty());
    }

    #[test]
    fn edit_points() {
        let timeline = timeline_with_linked_clip();
        assert_eq!(timeline.edit_points(), [f(0), f(5), f(15)]);
    }

    /// `a` at 0 and `b` at 10 on V1 and A1, each using frames 10..20 of
    /// 100 and linked with its audio.
    fn timeline_with_two_linked_clips() -> Timeline {
        let mut timeline = Timeline::new("t", Rational::new(24, 1));
        for (name, at) in [("a", 0), ("b", 10)] {
            let link = timeline.new_link_id();
            for track in [0, 1] {
                let mut c = clip(name, 10, 10);
                c.item_mut().unwrap().set_link(Some(link));
                timeline.place(track, c, f(at)).unwrap();
            }
        }
        timeline
    }

    fn layouts(timeline: &Timeline) -> [String; 2] {
        [0, 1].map(|t| layout(timeline.track(t).unwrap()))
    }

    #[test]
    fn ripple_delete_closes_the_gap() {
        let mut t = track(vec![
            clip("a", 0, 5),
            Composable::Gap(Gap::new(f(5))),
            clip("b", 0, 5),
            clip("c", 0, 5),
        ]);
        t.ripple_delete(t.range_of_child(2));
        assert_eq!(layout(&t), "a[0]@0+5 _@5+5 c[0]@10+5");
        // Part of a clip, and part of a gap.
        t.ripple_delete(TimeRange::new(f(3), f(4)));
        assert_eq!(layout(&t), "a[0]@0+3 _@3+3 c[0]@6+5");

        let mut timeline = timeline_with_two_linked_clips();
        let a = timeline.item_at(0, f(0)).unwrap();
        timeline.ripple_remove_item(a).unwrap();
        assert_eq!(layouts(&timeline), ["b[10]@0+10", "b[10]@0+10"]);
    }

    #[test]
    fn ripple_trims_move_what_follows() {
        let mut timeline = timeline_with_two_linked_clips();
        let a = ItemRef { track: 0, index: 0 };
        // The end may extend over b (which moves) up to the end of the media.
        assert_eq!(
            timeline.ripple_limits(a, Edge::End),
            Ok((f(0), Some(f(90))))
        );
        timeline.ripple_trim(a, Edge::End, f(14)).unwrap();
        let both = |s: &str| [s.to_owned(), s.to_owned()];
        assert_eq!(layouts(&timeline), both("a[10]@0+14 b[10]@14+10"));
        timeline.ripple_trim(a, Edge::End, f(6)).unwrap();
        assert_eq!(layouts(&timeline), both("a[10]@0+6 b[10]@6+10"));

        // The start stays in place; dragging it right takes media away.
        let b = ItemRef { track: 1, index: 1 };
        assert_eq!(
            timeline.ripple_limits(b, Edge::Start),
            Ok((f(-4), Some(f(16))))
        );
        timeline.ripple_trim(b, Edge::Start, f(8)).unwrap();
        assert_eq!(layouts(&timeline), both("a[10]@0+6 b[12]@6+8"));
        timeline.ripple_trim(b, Edge::Start, f(-6)).unwrap();
        assert_eq!(layouts(&timeline), both("a[10]@0+6 b[0]@6+20"));
        assert_eq!(
            timeline.ripple_trim(b, Edge::Start, f(-7)),
            Err(EditError::OutOfRange)
        );
        assert_eq!(
            timeline.ripple_trim(a, Edge::End, f(0)),
            Err(EditError::OutOfRange)
        );

        // A dissolve at the trimmed edge goes.
        timeline.add_dissolve(a, Edge::End, f(4)).unwrap();
        timeline.ripple_trim(a, Edge::End, f(5)).unwrap();
        assert_eq!(layout(timeline.track(0).unwrap()), "a[10]@0+5 b[0]@5+20");
    }

    #[test]
    fn roll_moves_the_cut_within_the_handles() {
        let mut t = track(vec![clip("a", 10, 10), clip("b", 2, 10)]);
        // b has 2 frames before its start, a 80 after its end.
        assert_eq!(t.roll_limits(0), Ok((f(8), f(20))));
        assert_eq!(t.roll(0, f(7)), Err(EditError::OutOfRange));
        assert_eq!(t.roll(0, f(20)), Err(EditError::OutOfRange));
        t.roll(0, f(8)).unwrap();
        assert_eq!(layout(&t), "a[10]@0+8 b[0]@8+12");
        t.roll(0, f(15)).unwrap();
        assert_eq!(layout(&t), "a[10]@0+15 b[7]@15+5");
        assert_eq!(t.roll_limits(1), Err(EditError::NoCut));

        // Linked clips roll along; either side of the cut names it.
        let mut timeline = timeline_with_two_linked_clips();
        let a = ItemRef { track: 0, index: 0 };
        let b = ItemRef { track: 1, index: 1 };
        assert!(timeline.is_cut(a, Edge::End) && timeline.is_cut(b, Edge::Start));
        assert!(!timeline.is_cut(a, Edge::Start) && !timeline.is_cut(b, Edge::End));
        assert_eq!(timeline.roll_limits(a, Edge::End), Ok((f(1), f(19))));
        assert_eq!(timeline.roll_limits(b, Edge::Start), Ok((f(1), f(19))));
        timeline.roll(a, Edge::End, f(13)).unwrap();
        let both = |s: &str| [s.to_owned(), s.to_owned()];
        assert_eq!(layouts(&timeline), both("a[10]@0+13 b[13]@13+7"));
        timeline.roll(b, Edge::Start, f(4)).unwrap();
        assert_eq!(layouts(&timeline), both("a[10]@0+4 b[4]@4+16"));
        assert_eq!(timeline.roll_limits(a, Edge::Start), Err(EditError::NoCut));

        // A dissolve at the cut goes.
        timeline.add_dissolve(a, Edge::End, f(2)).unwrap();
        timeline.roll(a, Edge::End, f(6)).unwrap();
        assert_eq!(layout(timeline.track(0).unwrap()), "a[10]@0+6 b[6]@6+14");
    }

    #[test]
    fn locked_tracks_reject_edits() {
        let mut timeline = timeline_with_linked_clip();
        timeline.track_mut(1).unwrap().item.set_locked(true);
        let video = timeline.item_at(0, f(5)).unwrap();
        // The linked audio is on a locked track, so the clip can't change.
        // (Failed edits may leave changes behind; `Document` reverts them.)
        assert_eq!(
            timeline.clone().move_item(video, 0, f(2)),
            Err(EditError::Locked(1))
        );
        assert_eq!(
            timeline.clone().ripple_remove_item(video),
            Err(EditError::Locked(1))
        );
        assert_eq!(
            timeline.remove_track(1).map(|_| ()),
            Err(EditError::Locked(1))
        );
        assert_eq!(
            timeline.place(1, clip("c", 0, 5), f(0)),
            Err(EditError::Locked(1))
        );
        // Splitting leaves locked tracks alone.
        assert_eq!(timeline.split_at(f(9)), 1);
        assert_eq!(layout(timeline.track(1).unwrap()), "_@0+5 a[0]@5+10");

        timeline.track_mut(1).unwrap().item.set_locked(false);
        assert!(timeline.track(1).unwrap().item.metadata.is_empty());
        let removed = timeline.remove_track(1).unwrap();
        assert_eq!(removed.item.name, "A1");
        assert_eq!(timeline.tracks().count(), 1);
    }
}
