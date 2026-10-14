//! Relinking offline media: pointing clips whose files are missing to
//! files somewhere else, one at a time or by searching a folder for files
//! of the same name (as Resolve's "Relink Selected Clips" does).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::timeline::{MediaReference, ReferenceKind, Timeline, file_url};

/// How deep a folder search goes below the chosen folder.
pub const SEARCH_DEPTH: usize = 6;
/// The most directory entries a folder search looks at, so choosing `/`
/// can't hang the app.
const SEARCH_LIMIT: usize = 200_000;

impl Timeline {
    /// The local files the clips use (their active references), resolved
    /// against `media_dir`.
    pub fn media_files(&self) -> BTreeSet<PathBuf> {
        self.clips()
            .filter_map(|clip| self.media_path(clip.media_reference()?))
            .collect()
    }

    /// The media files that don't exist.
    pub fn offline_files(&self) -> BTreeSet<PathBuf> {
        self.media_files()
            .into_iter()
            .filter(|path| !path.exists())
            .collect()
    }

    /// Points every reference to `from` (any clip, active or not) at `to`.
    /// Returns how many references changed.
    pub fn relink(&mut self, from: &Path, to: &Path) -> usize {
        let dir = self.media_dir.clone();
        let resolve = |reference: &MediaReference| {
            let path = reference.path()?;
            Some(match &dir {
                Some(dir) if path.is_relative() => dir.join(path),
                _ => path,
            })
        };
        let mut changed = 0;
        self.for_each_reference_mut(|reference| {
            if resolve(reference).as_deref() == Some(from) {
                reference.kind = ReferenceKind::External {
                    target_url: file_url(to),
                };
                if let Some(name) = to.file_name() {
                    reference.name = name.to_string_lossy().into_owned();
                }
                changed += 1;
            }
        });
        changed
    }
}

/// Files in `folder` (and up to `depth` levels below) with the same name
/// as files in `missing`: pairs of (missing, found), in the order of
/// `missing`. The shallowest match wins, then the first in name order.
pub fn find_in_folder(
    missing: &BTreeSet<PathBuf>,
    folder: &Path,
    depth: usize,
) -> Vec<(PathBuf, PathBuf)> {
    let mut wanted: HashMap<&std::ffi::OsStr, Vec<&PathBuf>> = HashMap::new();
    for path in missing {
        if let Some(name) = path.file_name() {
            wanted.entry(name).or_default().push(path);
        }
    }
    let mut found = Vec::new();
    let mut level = vec![folder.to_owned()];
    let mut seen = 0;
    for _ in 0..=depth {
        let mut next = Vec::new();
        for dir in level {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
            entries.sort();
            for path in entries {
                seen += 1;
                if seen > SEARCH_LIMIT || wanted.is_empty() {
                    return sorted(found, missing);
                }
                if path.is_dir() {
                    next.push(path);
                } else if let Some(paths) = path.file_name().and_then(|n| wanted.remove(n)) {
                    found.extend(paths.into_iter().map(|m| (m.clone(), path.clone())));
                }
            }
        }
        level = next;
    }
    sorted(found, missing)
}

fn sorted(
    mut found: Vec<(PathBuf, PathBuf)>,
    missing: &BTreeSet<PathBuf>,
) -> Vec<(PathBuf, PathBuf)> {
    let order: Vec<_> = missing.iter().collect();
    found.sort_by_key(|(m, _)| order.iter().position(|o| *o == m));
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{Rational, Time, TimeRange};
    use crate::timeline::{Clip, Composable};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nox-relink-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn relink_points_every_use_of_a_file_elsewhere() {
        let fps = Rational::new(24, 1);
        let range = TimeRange::new(Time::zero(fps.inverse()), Time::from_frames(10, fps));
        let mut timeline = Timeline::new("t", fps);
        let clip = |path: &str| {
            Composable::Clip(Clip::new(
                "c",
                MediaReference::from_path(Path::new(path), Some(range)),
                range,
            ))
        };
        timeline.track_mut(0).unwrap().children =
            vec![clip("/old/a.mov"), clip("/old/b.mov"), clip("/old/a.mov")];
        assert_eq!(
            timeline.relink(Path::new("/old/a.mov"), Path::new("/new/a 2.mov")),
            2
        );
        let files: Vec<_> = timeline.media_files().into_iter().collect();
        assert_eq!(files, [Path::new("/new/a 2.mov"), Path::new("/old/b.mov")]);
        assert_eq!(timeline.offline_files().len(), 2);
        // Relative references are matched by their resolved path.
        timeline.track_mut(0).unwrap().children = vec![Composable::Clip(Clip::new(
            "r",
            MediaReference::new(
                ReferenceKind::External {
                    target_url: "media/r.mov".into(),
                },
                Some(range),
            ),
            range,
        ))];
        timeline.media_dir = Some("/project".into());
        assert_eq!(
            timeline.relink(Path::new("/project/media/r.mov"), Path::new("/x/r.mov")),
            1
        );
        assert_eq!(
            timeline.media_files().into_iter().collect::<Vec<_>>(),
            [Path::new("/x/r.mov")]
        );
    }

    #[test]
    fn folder_search_finds_files_by_name() {
        let dir = scratch("search");
        touch(&dir.join("deep/er/a.mov"));
        touch(&dir.join("a.mov"));
        touch(&dir.join("sub/b.mov"));
        touch(&dir.join("sub/unrelated.mov"));
        touch(&dir.join("1/2/3/c.mov"));
        let missing: BTreeSet<PathBuf> =
            ["/gone/a.mov", "/gone/b.mov", "/gone/c.mov", "/gone/d.mov"]
                .into_iter()
                .map(PathBuf::from)
                .collect();
        let found = find_in_folder(&missing, &dir, SEARCH_DEPTH);
        assert_eq!(
            found,
            [
                (PathBuf::from("/gone/a.mov"), dir.join("a.mov")),
                (PathBuf::from("/gone/b.mov"), dir.join("sub/b.mov")),
                (PathBuf::from("/gone/c.mov"), dir.join("1/2/3/c.mov")),
            ]
        );
        // Depth 0 looks only in the folder itself.
        assert_eq!(find_in_folder(&missing, &dir, 0).len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
