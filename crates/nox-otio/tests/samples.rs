//! Round trips of OTIO's own sample files (from
//! https://github.com/AcademySoftwareFoundation/OpenTimelineIO, Apache-2.0,
//! `tests/sample_data`).
//!
//! When `NOX_OTIO_PYTHON` names a Python interpreter with the
//! `opentimelineio` package, Nox's output is also loaded with OTIO itself
//! and compared to OTIO's reading of the original.

use nox_otio::{read_file, read_str, write_string};
use nox_project::{Composable, Rational, Time, Timeline, TrackKind};
use std::path::{Path, PathBuf};

fn samples() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/otio");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "otio"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 13);
    files
}

fn sample(name: &str) -> Timeline {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/otio")
        .join(name);
    read_file(&path).unwrap_or_else(|err| panic!("{name}: {err}"))
}

#[test]
fn every_sample_round_trips() {
    for path in samples() {
        let timeline = read_file(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        let written = write_string(&timeline);
        let reread = read_str(&written).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        assert_eq!(reread, timeline, "{}", path.display());
        // Writing is deterministic.
        assert_eq!(write_string(&reread), written, "{}", path.display());
    }
}

fn frames(n: i64) -> Time {
    Time::from_frames(n, Rational::new(24, 1))
}

#[test]
fn simple_cut() {
    let timeline = sample("simple_cut.otio");
    assert_eq!(timeline.name, "Figure 1 - Simple Cut List");
    let (_, track) = timeline.tracks().next().unwrap();
    assert_eq!(track.kind, TrackKind::Video);
    let names: Vec<_> = track.children.iter().map(|c| c.name().to_owned()).collect();
    assert_eq!(names, ["Clip-001", "Clip-002", "Clip-003", "Clip-004"]);
    let clip = track.children[0].as_clip().unwrap();
    assert_eq!(
        clip.media_reference().unwrap().path().unwrap(),
        Path::new("/folder/titles.mov")
    );
    assert_eq!(timeline.frame_rate(), Rational::new(24, 1));
    let resolved = timeline.video_at(frames(0)).unwrap();
    assert_eq!(
        (resolved.clip.item.name.as_str(), resolved.source_time),
        ("Clip-001", frames(3))
    );
}

#[test]
fn transitions_and_nesting() {
    let timeline = sample("transition.otio");
    let transitions = timeline
        .tracks()
        .flat_map(|(_, t)| &t.children)
        .filter(|c| matches!(c, Composable::Transition(_)))
        .count();
    assert!(transitions > 0);

    let nested = sample("nested_example.otio");
    let has_nested_stack = nested
        .tracks()
        .flat_map(|(_, t)| &t.children)
        .any(|c| matches!(c, Composable::Stack(_)));
    assert!(has_nested_stack);
    assert!(!nested.duration().is_zero());
}

#[test]
fn old_and_new_schema_versions() {
    // Sequence.1 is the old name of Track.1.
    let timeline = sample("generator_reference_test.otio");
    assert_eq!(timeline.tracks().count(), 1);
    // Marker.3 with Color.1 objects.
    let timeline = sample("premiere_example.otio");
    let markers: Vec<_> = timeline
        .tracks
        .item
        .markers
        .iter()
        .chain(timeline.clips().flat_map(|c| &c.item.markers))
        .map(|m| m.color.as_str())
        .collect();
    assert!(markers.contains(&"RED"), "{markers:?}");
}

#[test]
fn big_numbers_and_non_finite_metadata() {
    let timeline = sample("big_int.otio");
    let clip = timeline.clips().next().unwrap();
    let ints = clip
        .item
        .metadata
        .get("int_test")
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(ints.get("verybig").unwrap().as_i64(), Some(3450100000));
    let floats = clip
        .item
        .metadata
        .get("float")
        .unwrap()
        .as_object()
        .unwrap();
    assert!(floats.get("floatnan").unwrap().as_f64().unwrap().is_nan());
    assert_eq!(
        floats.get("floatinf").unwrap().as_f64(),
        Some(f64::INFINITY)
    );
}

#[test]
fn matches_reference_implementation() {
    let Ok(python) = std::env::var("NOX_OTIO_PYTHON") else {
        eprintln!("skipping: set NOX_OTIO_PYTHON to a Python with opentimelineio to compare");
        return;
    };
    let out = std::env::temp_dir().join(format!("nox-otio-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    let mut pairs = Vec::new();
    for path in samples() {
        let written = out.join(path.file_name().unwrap());
        std::fs::write(&written, write_string(&read_file(&path).unwrap())).unwrap();
        pairs.push((path, written));
    }
    // OTIO reads the original (where its version can) and Nox's output, and
    // compares them.
    let script = r#"
import json, math, sys, opentimelineio as otio
failed = False
for original, written in zip(sys.argv[1::2], sys.argv[2::2]):
    ours = otio.adapters.read_from_file(written)
    try:
        theirs = otio.adapters.read_from_file(original)
    except Exception as err:
        print(f"{original}: OTIO cannot read the original ({err}); output loads fine")
        continue
    # OTIO's reader rounds some long decimals imprecisely (Nox's reader
    # rounds correctly), so floats are compared with a tolerance.
    def same(a, b, path):
        if isinstance(a, float) or isinstance(b, float):
            if not isinstance(a, (int, float)) or not isinstance(b, (int, float)):
                return False
            if math.isnan(a) and math.isnan(b) or a == b:
                return True
            if abs(a - b) <= 1e-15 * max(abs(a), abs(b)):
                return True
        elif isinstance(a, dict) and isinstance(b, dict):
            if a.keys() == b.keys() and all(same(a[k], b[k], f"{path}.{k}") for k in a):
                return True
        elif isinstance(a, list) and isinstance(b, list):
            if len(a) == len(b) and all(same(x, y, f"{path}[{i}]") for i, (x, y) in enumerate(zip(a, b))):
                return True
        elif a == b:
            return True
        print(f"  {path}: {a!r:.80} != {b!r:.80}")
        return False
    as_json = lambda t: json.loads(otio.adapters.write_to_string(t))
    if not same(as_json(theirs), as_json(ours), ""):
        print(f"MISMATCH {original}")
        failed = True
sys.exit(1 if failed else 0)
"#;
    let mut command = std::process::Command::new(python);
    command.arg("-c").arg(script);
    for (original, written) in &pairs {
        command.arg(original).arg(written);
    }
    let output = command.output().expect("running Python");
    eprintln!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_dir_all(&out).ok();
}
