//! The open project: a timeline document with undo, its file, and change
//! notification for the widgets that show it.

use crate::i18n::{fill, gettext};
use nox_media::MediaInfo;
use nox_project::{
    Clip, Composable, Document, EditError, MediaReference, Rational, Time, TimeRange, Timeline,
    TrackKind, format_timecode, nominal_fps,
};
use std::cell::{Ref, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Frame rate of new projects.
pub const DEFAULT_FRAME_RATE: Rational = Rational::new(25, 1);

/// Length of still images added to the timeline.
const STILL_SECONDS: i64 = 5;

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

    /// Adds `media` at sequence frame `frame`: its video on `track` (or the
    /// lowest video track) and its audio on `track` (or the first audio
    /// track), linked. Stills get a fixed length.
    pub fn add_media(
        &self,
        media: &MediaInfo,
        track: Option<usize>,
        frame: i64,
    ) -> Result<(), EditError> {
        self.edit(&gettext("Add Clip"), |timeline| {
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

            let mut place = |kind: TrackKind| {
                let index = pick_track(timeline, track, kind);
                let mut clip = Clip::new(name.clone(), reference.clone(), source);
                clip.item.set_link(link);
                timeline.place(index, Composable::Clip(clip), at)
            };
            if has_video {
                place(TrackKind::Video)?;
            }
            if has_audio {
                place(TrackKind::Audio)?;
            }
            Ok(())
        })
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
        let timeline = nox_otio::read_file(path)?;
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
        nox_otio::write_file(path, self.document.borrow().timeline())?;
        self.document.borrow_mut().mark_saved();
        self.path.replace(Some(path.to_owned()));
        tracing::info!("saved {}", path.display());
        self.notify();
        Ok(())
    }
}
