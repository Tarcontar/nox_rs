//! The open project: a timeline document with undo, its file, and change
//! notification for the widgets that show it.

use crate::i18n::{fill, gettext};
use nox_media::MediaInfo;
use nox_project::{
    Clip, Composable, Document, EditError, MediaReference, Rational, Time, TimeRange, Timeline,
    Track, TrackKind, format_timecode, nominal_fps,
};
use std::cell::{Ref, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Frame rate of new projects.
pub const DEFAULT_FRAME_RATE: Rational = Rational::new(25, 1);

/// Length of still images added to the timeline.
const STILL_SECONDS: i64 = 5;

/// How a clip whose shape doesn't match the timeline is fitted
/// (`framing.rs` asks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Framing {
    /// As the project's image scaling says (by default all of it, with
    /// black bars).
    #[default]
    Keep,
    /// Zoomed so it covers the frame (cropping the overhang).
    Fill,
    /// The timeline takes the clip's size.
    MatchTimeline,
}

/// The zoom that makes a picture of `size` cover a frame of `frame` size
/// after being fitted into it.
pub fn fill_zoom(size: (u32, u32), frame: (u32, u32)) -> f64 {
    let (w, h) = (f64::from(size.0.max(1)), f64::from(size.1.max(1)));
    let (fw, fh) = (f64::from(frame.0.max(1)), f64::from(frame.1.max(1)));
    let fit = (fw / w).min(fh / h);
    (fw / (w * fit)).max(fh / (h * fit))
}

/// `hint` if it is a track of `kind`, else the first track of that kind,
/// else a new one.
fn pick_track(timeline: &mut Timeline, hint: Option<usize>, kind: TrackKind) -> usize {
    hint.filter(|&i| timeline.track(i).is_some_and(|t| t.kind == kind))
        .or_else(|| {
            timeline
                .tracks()
                .find(|(_, t)| t.kind == kind)
                .map(|(i, _)| i)
        })
        .unwrap_or_else(|| timeline.add_track(kind))
}

type Listener = Box<dyn Fn(&Project)>;

pub struct Project {
    document: RefCell<Document>,
    path: RefCell<Option<PathBuf>>,
    listeners: RefCell<Vec<Rc<Listener>>>,
}

impl Project {
    pub fn new() -> Rc<Self> {
        let mut document = Document::new(Timeline::new(gettext("Untitled"), DEFAULT_FRAME_RATE));
        document.mark_saved();
        Rc::new(Self {
            document: RefCell::new(document),
            path: RefCell::default(),
            listeners: RefCell::default(),
        })
    }

    /// Calls `f` after every change: edits, undo, redo, open, save.
    pub fn connect_changed(&self, f: impl Fn(&Project) + 'static) {
        self.listeners.borrow_mut().push(Rc::new(Box::new(f)));
    }

    fn notify(&self) {
        // Clone the list so listeners may connect more listeners.
        let listeners = self.listeners.borrow().clone();
        for listener in listeners {
            listener(self);
        }
    }

    pub fn timeline(&self) -> Ref<'_, Timeline> {
        Ref::map(self.document.borrow(), Document::timeline)
    }

    pub fn frame_rate(&self) -> Rational {
        self.timeline().frame_rate()
    }

    /// Timecode for a sequence frame, honoring the drop-frame setting.
    pub fn timecode(&self, frame: i64) -> String {
        let timeline = self.timeline();
        format_timecode(frame, timeline.frame_rate(), timeline.settings.drop_frame)
    }

    /// Length of the sequence in frames.
    pub fn duration_frames(&self) -> i64 {
        let timeline = self.timeline();
        let fps = timeline.frame_rate();
        let duration = timeline.duration();
        // A partial last frame still shows.
        let frames = duration.frame(fps);
        if Time::from_frames(frames, fps) < duration {
            frames + 1
        } else {
            frames
        }
    }

    /// Applies an undoable edit and notifies listeners.
    pub fn edit<T, E: std::fmt::Display>(
        &self,
        label: &str,
        edit: impl FnOnce(&mut Timeline) -> Result<T, E>,
    ) -> Result<T, E> {
        let result = self.document.borrow_mut().edit(label, edit);
        match &result {
            Ok(_) => {
                tracing::debug!("edit: {label}");
                self.notify();
            }
            Err(err) => tracing::info!("{label} not possible: {err}"),
        }
        result
    }

    /// Applies an undoable edit that can't fail and notifies listeners.
    pub fn change(&self, label: &str, change: impl FnOnce(&mut Timeline)) {
        let _ = self.edit(label, |timeline| {
            change(timeline);
            Ok::<_, std::convert::Infallible>(())
        });
    }

    /// Applies an undoable edit to track `index` (if there is one) and
    /// notifies listeners.
    pub fn edit_track(&self, index: usize, label: &str, change: impl FnOnce(&mut Track)) {
        let _ = self.edit(label, |timeline| {
            let track = timeline
                .track_mut(index)
                .ok_or(nox_project::EditError::NoTrack(index))?;
            change(track);
            Ok::<_, nox_project::EditError>(())
        });
    }

    /// Like `edit`, but consecutive edits with the same `merge_key` make
    /// one undo step (see `Document::edit_merging`), e.g. while a slider is
    /// dragged.
    pub fn edit_merging<T, E: std::fmt::Display>(
        &self,
        label: &str,
        merge_key: &str,
        edit: impl FnOnce(&mut Timeline) -> Result<T, E>,
    ) -> Result<T, E> {
        let result = self
            .document
            .borrow_mut()
            .edit_merging(label, merge_key, edit);
        match &result {
            Ok(_) => self.notify(),
            Err(err) => tracing::info!("{label} not possible: {err}"),
        }
        result
    }

    /// Adds `media` at sequence frame `frame`: its video on `track` (or the
    /// lowest video track) and its audio on `track` (or the first audio
    /// track), linked. Stills get a fixed length. Overwrites what is there,
    /// or with `insert`, pushes it (and sync-locked tracks) later. Returns
    /// the clip's length in sequence frames.
    pub fn add_media(
        &self,
        media: &MediaInfo,
        track: Option<usize>,
        frame: i64,
        insert: bool,
        framing: Framing,
    ) -> Result<i64, EditError> {
        let label = if insert {
            gettext("Insert Clip")
        } else {
            gettext("Add Clip")
        };
        self.edit(&label, |timeline| {
            // The first clip sets up the sequence: its frame rate and size.
            if timeline.clips().next().is_none()
                && let Some(video) = &media.video
            {
                if let Some(rate) = video.frame_rate {
                    timeline.set_frame_rate(rate);
                }
                if !media.is_still_image() {
                    (timeline.settings.width, timeline.settings.height) =
                        (video.width, video.height);
                }
                tracing::info!(
                    "sequence set to {}×{} at {:.3} fps from {}",
                    timeline.settings.width,
                    timeline.settings.height,
                    timeline.frame_rate().as_f64(),
                    media.path.display()
                );
            }
            let mut zoom = None;
            if let Some(video) = &media.video {
                let size = (video.width, video.height);
                match framing {
                    Framing::Keep => {}
                    Framing::MatchTimeline => {
                        (timeline.settings.width, timeline.settings.height) = size;
                    }
                    Framing::Fill => {
                        let frame = (timeline.settings.width, timeline.settings.height);
                        zoom = Some(fill_zoom(size, frame));
                    }
                }
            }
            let fps = timeline.frame_rate();
            let at = Time::from_frames(frame.max(0), fps);
            let name = media
                .path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            // The available range, at the media's own rate.
            let available = media.duration.map(|duration| {
                let timebase = match (&media.video, media.audio.first()) {
                    (Some(video), _) if video.frame_rate.is_some() => {
                        video.frame_rate.expect("rate").inverse()
                    }
                    (_, Some(audio)) => Rational::new(1, audio.sample_rate.max(1).into()),
                    _ => duration.timebase,
                };
                TimeRange::new(Time::zero(timebase), duration.rescale_floor(timebase))
            });
            let length = match available {
                Some(range) => range.duration.frame(fps).max(1),
                None => STILL_SECONDS * nominal_fps(fps) as i64,
            };
            let source = TimeRange::new(Time::zero(fps.inverse()), Time::from_frames(length, fps));
            let reference = MediaReference::from_path(&media.path, available);
            let has_video = media.video.is_some();
            let has_audio = !media.audio.is_empty();
            let link = (has_video && has_audio).then(|| timeline.new_link_id());

            let mut clips = Vec::new();
            for (kind, wanted) in [(TrackKind::Video, has_video), (TrackKind::Audio, has_audio)] {
                if wanted {
                    let video = kind == TrackKind::Video;
                    let index = pick_track(timeline, track, kind);
                    let mut clip = Clip::new(name.clone(), reference.clone(), source);
                    clip.item.set_link(link);
                    if let (Some(zoom), true) = (zoom, video) {
                        clip.item.set_transform(nox_project::Transform {
                            zoom,
                            ..nox_project::Transform::IDENTITY
                        });
                    }
                    clips.push((index, Composable::Clip(clip)));
                }
            }
            if insert {
                timeline.insert_clips(clips, at)?;
            } else {
                for (index, clip) in clips {
                    timeline.place(index, clip, at)?;
                }
            }
            Ok(length)
        })
    }

    /// Whether `media`'s picture is portrait where the timeline is
    /// landscape or the other way round, so fitting it would leave it small
    /// between bars. Only asked once the timeline has clips (the first one
    /// sets its size) and while the image scaling fits pictures.
    pub fn orientation_differs(&self, media: &MediaInfo) -> bool {
        let timeline = self.timeline();
        let Some(video) = &media.video else {
            return false;
        };
        let settings = &timeline.settings;
        let portrait = |w: u32, h: u32| h > w;
        timeline.clips().next().is_some()
            && settings.scaling == nox_project::ScalingMode::Fit
            && video.width != video.height
            && settings.width != settings.height
            && portrait(video.width, video.height) != portrait(settings.width, settings.height)
    }

    pub fn undo(&self) {
        let label = self.document.borrow_mut().undo();
        if let Some(label) = label {
            tracing::debug!("undo: {label}");
            self.notify();
        }
    }

    pub fn redo(&self) {
        let label = self.document.borrow_mut().redo();
        if let Some(label) = label {
            tracing::debug!("redo: {label}");
            self.notify();
        }
    }

    pub fn undo_label(&self) -> Option<String> {
        self.document.borrow().undo_label().map(str::to_owned)
    }

    pub fn redo_label(&self) -> Option<String> {
        self.document.borrow().redo_label().map(str::to_owned)
    }

    pub fn is_modified(&self) -> bool {
        self.document.borrow().is_modified()
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.path.borrow().clone()
    }

    /// The window title: file name, marked when there are unsaved changes.
    pub fn title(&self) -> String {
        let name = self
            .path()
            .and_then(|p| p.file_stem().map(|n| n.to_string_lossy().into_owned()));
        let name = name.unwrap_or_else(|| self.timeline().name.clone());
        let title = if self.is_modified() {
            gettext("• {name} — Nox")
        } else {
            gettext("{name} — Nox")
        };
        fill(&title, &[("name", &name)])
    }

    /// Replaces the project with a new, empty one.
    pub fn reset(&self) {
        let mut document = Document::new(Timeline::new(gettext("Untitled"), DEFAULT_FRAME_RATE));
        document.mark_saved();
        self.document.replace(document);
        self.path.replace(None);
        self.notify();
    }

    pub fn open(&self, path: &Path) -> Result<(), nox_otio::Error> {
        let mut timeline = nox_otio::read_file(path)?;
        timeline.media_dir = std::path::absolute(path)
            .ok()
            .and_then(|p| p.parent().map(Path::to_owned));
        tracing::info!(
            "opened {} ({} clips, {} fps)",
            path.display(),
            timeline.clips().count(),
            timeline.frame_rate().as_f64()
        );
        self.document.replace(Document::new(timeline));
        self.path.replace(Some(path.to_owned()));
        self.notify();
        Ok(())
    }

    pub fn save_as(&self, path: &Path) -> Result<(), nox_otio::Error> {
        let document = self.document.borrow();
        let timeline = document.timeline();
        // Relative media paths only stay valid next to the original file.
        let dir = std::path::absolute(path)
            .ok()
            .and_then(|p| p.parent().map(Path::to_owned));
        if timeline.media_dir.is_some() && timeline.media_dir != dir {
            let mut copy = timeline.clone();
            copy.make_media_paths_absolute();
            nox_otio::write_file(path, &copy)?;
        } else {
            nox_otio::write_file(path, timeline)?;
        }
        drop(document);
        self.document.borrow_mut().mark_saved();
        self.path.replace(Some(path.to_owned()));
        tracing::info!("saved {}", path.display());
        self.notify();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::fill_zoom;

    #[test]
    fn fill_zoom_covers_the_frame() {
        // Landscape UHD in a portrait UHD frame: fitted to 2160 × 1215, so
        // 316% to fill the height.
        let zoom = fill_zoom((3840, 2160), (2160, 3840));
        assert!((zoom - 3840.0 / 1215.0).abs() < 1e-3, "{zoom}");
        // The same shape needs no zoom; a 4:3 picture in 16:9 fills by width.
        assert_eq!(fill_zoom((1920, 1080), (3840, 2160)), 1.0);
        assert!((fill_zoom((1440, 1080), (1920, 1080)) - 4.0 / 3.0).abs() < 1e-9);
    }
}
