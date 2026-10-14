//! Adding media whose orientation differs from the timeline's (a portrait
//! clip in a landscape timeline or the other way round): asks first how to
//! fit it, as Premiere's clip mismatch warning does.

use gtk::{gio, prelude::*};
use nox_media::MediaInfo;
use nox_project::EditError;
use std::rc::Rc;

use crate::i18n::{fill, gettext};
use crate::project::{Framing, Project};

/// Adds `media` as `Project::add_media` does. If its orientation differs
/// from the timeline's and `choice` doesn't say how to fit it, asks first
/// (over `parent`); cancelling adds nothing. `done` gets the result and,
/// if a choice was made, the choice, so a batch can reuse it.
pub fn add_media(
    parent: &impl IsA<gtk::Widget>,
    project: &Rc<Project>,
    media: MediaInfo,
    (track, frame, insert): (Option<usize>, i64, bool),
    choice: Option<Framing>,
    done: impl FnOnce(Result<i64, EditError>, Option<Framing>) + 'static,
) {
    if !project.orientation_differs(&media) || choice.is_some() {
        let framing = if project.orientation_differs(&media) {
            choice.unwrap_or_default()
        } else {
            Framing::Keep
        };
        let result = project.add_media(&media, track, frame, insert, framing);
        done(result, choice);
        return;
    }
    let Some(video) = media.video.clone() else {
        return;
    };
    let (width, height) = {
        let timeline = project.timeline();
        (timeline.settings.width, timeline.settings.height)
    };
    let message = if video.height > video.width {
        gettext("This clip is portrait, the timeline is landscape")
    } else {
        gettext("This clip is landscape, the timeline is portrait")
    };
    let detail = fill(
        &gettext(
            "Fit shows all of the picture, small between black bars. Fill zooms \
             it to cover the {width}×{height} frame, cropping the rest; its zoom \
             and position can be changed in the inspector. Or the timeline can \
             change to the clip's {clip_width}×{clip_height}, fitting the other clips.",
        ),
        &[
            ("width", &width),
            ("height", &height),
            ("clip_width", &video.width),
            ("clip_height", &video.height),
        ],
    );
    let dialog = gtk::AlertDialog::builder()
        .message(message)
        .detail(detail)
        .buttons([
            gettext("Cancel"),
            gettext("Fit"),
            gettext("Fill"),
            gettext("Change Timeline"),
        ])
        .cancel_button(0)
        .default_button(2)
        .modal(true)
        .build();
    let window = parent.root().and_downcast::<gtk::Window>();
    let project = project.clone();
    dialog.choose(window.as_ref(), gio::Cancellable::NONE, move |answer| {
        let framing = match answer {
            Ok(1) => Framing::Keep,
            Ok(2) => Framing::Fill,
            Ok(3) => Framing::MatchTimeline,
            _ => return,
        };
        let result = project.add_media(&media, track, frame, insert, framing);
        done(result, Some(framing));
    });
}
