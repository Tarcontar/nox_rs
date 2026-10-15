# Nox — state and plan

Nox is a video editor written in Rust: a GTK4 UI, FFmpeg for media I/O, and Vulkan for decoding, image processing and display, with hardware acceleration wherever possible. The UI follows common editors (DaVinci Resolve, Blender VSE): a media pool, a viewer and a timeline, with an inspector later.

*Last updated: 2026-10-13 (M6 started: linear working space and half-float output; In/Out marks, ripple and roll edits, track controls; Vulkan validation).*

## Status at a glance

| Milestone | Status |
|---|---|
| M0 – Workspace skeleton, GTK window | ✅ done |
| M1 – Media import, probing, thumbnails | ✅ done |
| M2 – Vulkan decode to viewer, zero copy | ✅ done |
| M3 – Project model (OTIO) and timeline editing | ✅ done |
| M4 – Playback engine, audio, A/V sync | ✅ done |
| M5 – Export | ✅ done |
| M6 – Color management | ⏸ partly done; the rest is postponed (see M6) |
| M7 – Editing depth | ✅ done |
| M8 – Motion and keyframes | ✅ done |

What works today:
- **Import:** drag files onto the media pool (or use the import button, or `nox file…`). Each file is probed in the background and gets a thumbnail, its duration as timecode, and a tooltip with stream and color details.
- **Timeline editing:** drag media from the pool onto a track, or double-click it to append. Files with audio become a linked video and audio clip. Drag clips to move them, also to another track of the same kind; drag their edges to trim. Clips snap to edit points and the playhead (N toggles snapping). Ctrl+B splits at the playhead, Delete removes the selected clips and Shift+Delete ripple-deletes them (the gaps close on their tracks). Ctrl+dropping inserts instead of overwriting, pushing what follows. With media selected in the pool, F9 or comma inserts it at the playhead and F10 or period overwrites; the playhead moves past the new clips. Ctrl+dragging an edge ripple-trims (what follows moves along), and dragging the middle of a cut between two clips rolls it, within both clips' media. Every edit can be undone (Ctrl+Z, Ctrl+Shift+Z).
- **Multi-selection:**
  - Ctrl-click adds or removes a clip, and Shift-click adds one.
  - Dragging on empty track space draws a rubber band, which selects every clip it touches.
  - Ctrl+A selects all clips, and Escape clears the selection.
  - Dragging a selected clip moves the whole selection, with linked clips. It can change tracks when the selection is on one track.
  - Delete, ripple delete and inspector or wheel changes apply to every selected clip.
  - The inspector and the wheels show the clip clicked last.
- **Slip and slide:**
  - Alt+drag a clip to slip it: the same place and length, showing another part of its media. Dragging right shows earlier media.
  - Alt+Shift+drag slides it between its neighbors: the clip before ends later, the one after starts later, and gaps grow or shrink instead.
  - Both stay within the spare media, move linked clips along, and show the offset in frames while dragging.
- **Markers** (Resolve's keys):
  - M adds a timeline marker at the playhead, and Shift+↑/↓ jump between markers.
  - Markers show as colored flags on the ruler and can be dragged along it.
  - Clicking one selects it. The inspector then edits its name, color and note, and Delete removes it.
  - Markers on clips from imported files show as dots on the clips.
  - They are OTIO markers (on the top-level stack), saved with the project.
- **In/Out marks** (DaVinci Resolve's keys): I and O mark In and Out at the playhead, Alt+I and Alt+O clear one, Alt+X both. The range shows on the ruler with everything outside it shaded. Marks are undoable edits and are saved with the project. Right-clicking the ruler moves the playhead there and offers Mark In, Mark Out, Add Marker and the Clear actions, as in Premiere.
- **Track controls:** each track header shows the track's name and toggles: mute (video tracks: show/hide), solo (audio tracks), lock and sync lock. They are real buttons, reached with Tab and toggled with Space. Right-click a header, or press Shift+F10 in it, for the track menu: rename, delete and the same toggles. While any audio track is soloed, only soloed tracks are heard. Sync-locked tracks move along with ripple edits on other tracks (gaps open or close on them), so their clips stay in step. Muted audio tracks aren't mixed and hidden video tracks aren't composited, in playback and export (OTIO's `enabled`). Locked tracks are striped and reject every edit, including edits of clips linked to their clips.
- **Projects:** `.otio` files (OpenTimelineIO) open and save, from the header bar or with `nox edit.otio`. Opening a project imports its media into the pool. Relative media paths are resolved against the `.otio` file's folder, as other OTIO tools do. Saving to another folder writes those as absolute `file://` URLs, so they keep working. Clips whose files are missing show red and are labelled "Offline". If a project opens with missing media, Nox offers to search a folder for them. Relinking works in two ways:
  - **Relink Offline Media…** (a button in the media pool's header, shown while media is offline) searches a folder and its subfolders, up to 6 levels deep, for files of the same name.
  - **Relink Media…** (clip menu) points one clip's file elsewhere. Every clip using that file follows, and other offline files found in the same folder are relinked too.

  Relinking is one undoable edit, and the new files are added to the media pool. Unsaved changes are flagged in the title, and closing asks first.
- **Window layout** (decision 26): the header bar has New, Open, Save, Save As, Undo and Redo, the Edit/Color switcher, the project's name, Project Settings and Export, and on the Edit page a toggle that hides the inspector (`win.show-inspector`), giving the viewer its room. There is no main menu. A toolbar above the timeline has Insert, Overwrite, Split, Ripple Delete, Add Dissolve, Snapping, and Add Video/Audio Track. The viewer has Mark In, Mark Out and Add Marker beside the transport controls. Tooltips name each button's shortcut. The media pool selects with click, Ctrl-click and Shift-click, without a rubber band.
- **Viewer:** shows the whole sequence at the playhead, composited on the GPU: every enabled video track, with per-clip opacity and blend mode (normal, add, multiply, screen), source alpha (RGBA images such as PNG titles), dissolves, and sources fitted to the sequence (letterboxed or pillarboxed). It renders at the viewer's size. Scrub by dragging the ruler, step frames with ←/→, jump to edit points with ↑/↓, and to the ends with Home/End.
- **Playback:** Space plays and pauses, L plays and K pauses. Audio clips show their waveform on the timeline. Audio tracks are mixed (clip volume, equal-power crossfades under dissolves) and played on the default output device, which also serves as the clock. Frames are rendered ahead into a cache, and frames that can't be ready in time are skipped. Scrubbing back over cached frames is instant.
- **Edit and Color pages** (header bar, Shift+4 and Shift+6, as in Resolve):
  - **Edit page:** has an inspector on the right for the selected clip (opacity, blend mode, volume) or dissolve. Dragging a slider makes one undo step.
  - **Color page:** shows the viewer large with live scopes beside it: waveform, RGB parade, vectorscope and histogram of the frame on screen. Below them is a strip of the timeline's video clips that selects a clip and jumps to its start, and the primary wheels (lift, gamma, gain, offset) that grade the selected clip: drag a wheel's puck to shift the color balance, and use its slider for the master value. Grades are undoable and saved with the project.
  - **Viewer:** one viewer moves to whichever page is showing.
- **Speed:** the inspector sets a clip's speed in percent (the clip gets shorter or longer and shows the same media, audio included) or freezes it on the frame under the playhead.
- **Keyframes:** opacity, volume and the transform values can change over a clip. Click the diamond beside a value to keyframe it at the playhead, move the playhead and change the value. The value moves between keyframes linearly or eased.
- **Transform:** the inspector moves, zooms, rotates and crops the selected video clips, as Resolve's Transform and Cropping. Each field changes only its value, also across a multi-selection.
- **Fades:** hovering or selecting a clip shows handles at its top corners; drag them inward to fade the clip in or out (video to the track below, audio to silence). The inspector has the same values in seconds.
- **Clip menu** (right-click): opacity, blend mode, volume, add dissolve (also Ctrl+T, at the selected clip's edge nearest the playhead), split, delete. Dissolves can be selected and deleted.
- **Project settings** (Shift+9, Resolve-style: categories in a sidebar, Cancel/Save, one undoable edit): project name; timeline resolution (presets or custom), frame rate and drop-frame timecode; image scaling for mismatched sources (scale to fit, scale with crop, center crop 1:1, stretch). The first clip in an empty sequence sets frame rate and resolution (like Shotcut's "Automatic" video mode). A later clip whose orientation differs (a portrait clip in a landscape timeline or the other way round) asks first, as Premiere's clip mismatch warning does (`framing.rs`). The choices are Fit (black bars), Fill (zoomed to cover the frame, as an ordinary transform), or Change Timeline (to the clip's size), and adding the clip is one undo step with the choice. When several clips are added at once (F9/F10), the answer holds for the rest of them. Only asked while the image scaling is "Scale entire image to fit".
- **Export** (Ctrl+E): presets (H.264 master, YouTube/web, HEVC, HDR10 master, HLG, ProRes 422 HQ, review copy) or custom format, quality, hardware encoding and audio; progress, cancel, Open Folder. With hardware encoding the compositor writes straight into the Vulkan Video encoder's images (zero copy): 1080p exports of the test footage run at about 190 fps, against 57 fps with x264. The range is the whole timeline or In to Out (the default when both marks are set, as on Resolve's Deliver page). Timelines without audio in the range export without an audio track (the option is greyed out). `nox-cli render project.otio out.mp4` does the same headless; `--in-out` renders only the marked range, `--frames` any range. **HDR:** the dialog's Dynamic range (or `nox-cli render --hdr pq|hlg [--peak NITS]`) exports HDR10 or HLG as 10-bit BT.2020 HEVC or ProRes, with HDR sources keeping their highlights and SDR clips at 203 nits (decision 27). There are presets for an HDR10 master and for HLG.
- **Translations:** every user-visible string goes through gettext; a German catalog (`po/de.po`) is the first. Run with `LANGUAGE=de` to see it.
- **Color management (M6, first part):** pictures are composited in linear ACEScg. Each source enters through its transfer function and primaries (BT.601, BT.709, BT.2020, P3 and others are converted rather than shown as is), so opacity, blend modes and dissolves work in linear light. The viewer gets half-float frames (`AB4H`) with an explicit color state, so 10-bit sources are no longer rounded to 8 bit before the screen. SDR BT.709 footage looks exactly as before.
- **Frame path on Linux/NVIDIA:** frames never leave the GPU between decoder and screen. Vulkan Video decodes, a compute shader converts to RGB directly on FFmpeg's images, and the result goes to GTK as a DMA-BUF texture.

## Architecture

### Crates

```
crates/
  nox-color    color science: H.273 color tags, Y'CbCr→R'G'B' matrices, primaries,
               transfer functions, the working space, ACEScct and grades (pure Rust)
  nox-project  timeline model shaped like OTIO, edit operations, undo; rational
               time, drop-frame timecode (pure Rust)
  nox-otio     .otio reading and writing (pure Rust, no OTIO library)
  nox-gpu      Vulkan: shared device, YUV→RGB compute converter, compositor,
               DMA-BUF export
  nox-media    FFmpeg (ffmpeg-next): probe, thumbnails, frame-accurate decoding,
               Vulkan device creation, zero-copy access to decoded frames
  nox-engine   render worker (decoder pool, compositing, frame cache), audio
               mixer and output (cpal), playback clock
  nox-gui      GTK4 app, binary `nox`; layouts in resources/ui/*.ui
  nox-cli      headless tools (`nox-cli probe`, `nox-cli render`, `nox-cli version`)
```

Dependency direction: `gui → {engine, otio}`, `engine → {media, gpu, project}`, `otio → project`, `project → color`, `media → {gpu, color, project}`, and `gpu → color`. `nox-color`, `nox-project` and `nox-otio` have no heavy dependencies. Nothing below `nox-gui` depends on GTK or GLib, so the core can be tested headless and used by the CLI.

### Key decisions

1. **One shared Vulkan device, created by FFmpeg.** FFmpeg creates the device (`av_hwdevice_ctx_create`), so it enables exactly the video queues and extensions its decoders need, including DMA-BUF export. `nox-gpu` loads the same device into `ash` (`GpuDevice::from_external`) and submits work through FFmpeg's queue locks. `NOX_GPU` selects the device.
   *Changed during M2:* the original plan was for Nox to create the device and hand it to FFmpeg.
2. **Zero copy from decoder to screen.** Decoded `AVVkFrame` images are read in place through per-plane image views, following FFmpeg's sync protocol (frame lock, timeline semaphores, recorded layouts). The converted frame is exported as DMA-BUF and wrapped in a `GdkDmabufTexture`. Its release callback returns the image to a pool. The fallbacks, each logged with its reason:
   - download + upload, if the decoder's images can't be viewed per plane
   - GPU readback to a `GdkMemoryTexture`, if GTK can't import DMA-BUF (`NOX_PRESENT=readback` forces this)
   - CPU conversion with swscale, if there is no Vulkan device
3. **One conversion shader for every layout.** A single compute shader handles planar, semi-planar and packed layouts. A 3×4 matrix built by `nox-color` folds in the H.273 coefficients, range, bit depth and sample packing (e.g. P010). The shader also handles chroma siting (untagged 4:2:0 is treated as left-sited, the H.264 convention), non-square pixels and padded decode images.
4. **Rational time everywhere:** integer ticks plus a timebase, never `f64` seconds.
5. **OpenTimelineIO for timelines.** The model in `nox-project` mirrors OTIO's types (Timeline, Stack, Track, Clip, Gap, Transition, media references, effects, markers) but keeps exact rational time. The format lives in its own crate, `nox-otio`. Details:
   - It has its own small JSON reader and writer instead of `serde_json`, because OTIO files may contain `NaN`/`Inf` and integers beyond 64 bits (OTIO's own `big_int.otio` sample does).
   - It reads old and new schema versions (`Sequence.1`, `Clip.1`/`.2`, `Marker.1`–`.3`). It writes the versions of the current OTIO release, 0.18, so tools in the wild can read the files.
   - OTIO's float `(value, rate)` pairs become exact fractions. NTSC rates are recognized from abbreviations (29.97 → 30000/1001), and non-integral values get a finer timebase instead of being rounded.
   - Fields a schema doesn't define, from other tools or OTIO versions, are kept in `extra` and written back, as OTIO itself does.
   - Nox's own data goes under `metadata.nox`: sequence settings on the timeline, and link ids on clips.
   - *Changed during M3:* `serde` turned out not to fit (see the JSON point above).
6. **Color management without linking OpenColorIO.** `nox-color` implements the underlying standards: H.273, ACES, ASC CDL, `.cube`/CLF LUTs, camera log curves, and a subset of the `.ocio` config format. Where useful we port OCIO's BSD-licensed GPU code. OCIO itself is used only in tests, to produce reference values.
7. **Shaders:** GLSL compiled to SPIR-V at build time by `glslc` or `glslangValidator` (from `PATH` or `$VULKAN_SDK/bin`).
8. **GUI objects are thin wrappers.** Model types are plain Rust; the GUI wraps them in small GObject subclasses (`MediaObject`) so they can live in GTK list models.
9. **Edits overwrite, OTIO-style.** Tracks are sequences with explicit gaps. Placing an item splits whatever is under it at the edges and replaces it. Trimming in leaves a gap, and trimming out may only consume gaps and available media. Transitions touching an edit are dropped. Tracks are normalized after every edit (gaps merged, trailing gaps removed). Linked clips (`metadata.nox.link`) move, trim and delete together, and splitting gives each half its own link. The ripple edits are the exception to overwriting: ripple delete and ripple trim move what follows, and so does ripple insert. They act on the tracks of the edited clips and their linked clips, plus every sync-locked track (`metadata.nox.sync_lock`): there the same range is opened as a gap or, when removing, closed if it holds only gap. If a sync-locked track has media in a range that would close, the edit is refused (`EditError::SyncLocked`) rather than cutting it. A roll changes two neighboring clips' source ranges, so nothing else moves. Linked clips with the same edge or cut are ripple-trimmed and rolled along, all or nothing, so they stay in sync. Locked tracks (`metadata.nox.locked`) reject every edit of their contents in the model itself (`EditError::Locked`), not just in the UI.
10. **Undo by snapshots.** `Document::edit` keeps the timeline from before each edit (up to 500 steps). Timelines are small, and this makes undo exact by construction with no inverse operations. Failed edits leave the timeline untouched, and edits that change nothing aren't recorded.
11. **Compositing in two steps.** Each layer is converted by the YUV→RGB shader (its `ycbcr_to_working` variant) into the linear working space, an RGBA32F image at its placed size (`Rect::fit` keeps the aspect ratio). `blend.comp` then blends the layers in order into an RGBA32F accumulator over black. `output.comp` applies the output transform and writes `AB4H` (or `AB24`) for DMA-BUF, or RGBA8 for readback. `output_yuv.comp` writes into an encoder's NV12 image instead. Blending happens in linear light (decision 20).
    *Changed during M6:* blending used to work on display-referred R'G'B' (editors' "video" mode), with RGBA8 layers and an RGBA16F accumulator. Half floats turned out too coarse for a linear working space: converting saturated colors back to BT.709 subtracts large terms, and the 1/2.4 encoding magnified the leftover rounding to as much as 9 code values (cyan `0,254,192` became `9,254,192`). A dissolve resolves to both clips, with the incoming one's opacity scaled by progress, so it needs no separate code path. Per-clip settings live in `metadata.nox` (`opacity`, `blend`, `volume`, `grade`).
    *Changed during M4:* the M3 viewer requested single source frames; the engine now renders whole sequence frames.
12. **The engine renders sequence frames into a cache.** A worker thread owns the GPU and a pool of open decoders (up to 8, two per file). A decoder is reused by seeking rather than reopened; reopening is expensive, e.g. FFV1 sets up FFmpeg's Vulkan compute decoder. A second decoder per file is opened only when the file is on two layers of one frame. Rendered frames are cached by timeline version and output size, up to `NOX_CACHE_MB` (768 MB by default), least recently used first. While paused, the newest request wins. While playing, the worker renders up to 0.5 s ahead of the clock. If it falls behind, it aims ahead by its measured render time, so finished frames are still shown and the ones in between are skipped.
    Scrubbing in long-GOP footage (keyframes 4–8 s apart, so stepping back can mean decoding hundreds of 4K frames) works in three ways:
    - While a requested frame renders, `request` returns the closest cached frame within half a second to show meanwhile.
    - If nothing is cached nearby and the frame is more than 8 decoded frames from where a decoder can start, a preview using the keyframe before it is rendered first.
    - After 50 ms, a newer request interrupts the render (`VideoDecoder::frame_at_until`). The decoder keeps its progress. Short renders always finish, otherwise a fast-moving mouse would never see a frame.

    Decoders decode on rather than seek whenever the target is in the GOP they are already in (from the demuxer's keyframe index), not only within 2 s.
    Stills (photos) are decoded once, and every position maps to their single frame (image files can't seek, so treating them as 25 fps video reopened and re-decoded a 30 MP JPEG on every frame after 2 s). The compositor keeps converted stills on the GPU (8 entries, LRU, keyed by file and placed size), so a photo isn't uploaded and converted on every frame. Rendering ahead during playback only moves forward. When the lead shrinks after a slow frame, going back behind frames already rendered made decoders seek back to a keyframe, which was slow and made the lead grow again. That runaway made `noemi.otio` lag after its first photo.
13. **Audio is the clock.** For each playback, a `cpal` stream on the default device (or `NOX_AUDIO_DEVICE`) pulls from a lock-free ring that a mixer thread keeps 250 ms full. The playback time is the frames the device has consumed, corrected by the output latency cpal reports. After an underrun the mixer skips the frames the device played as silence, so audio stays in step. If the audio clock drifts more than a second from wall time (ALSA's `null` device consumes samples as fast as it can), playback switches to wall time. Without a device it uses wall time from the start. Read raw, the audio clock moves in steps (per device callback) and its latency estimate jitters, which made 60 fps playback judder. So the playback clock is wall time plus an offset that slowly follows the audio clock (2% of the error per reading), snapping on errors over 50 ms. On every display refresh (a GTK tick callback), the GUI picks the frame for the moment that refresh will be shown, which is the frame clock's time plus one refresh interval.
14. **HDR sources are tone-mapped for SDR display.** PQ (HDR10) and HLG frames are decoded to display light, converted from BT.2020/P3 to BT.709, mapped with the BT.2390 EETF on the largest component (content peak from MaxCLL or the mastering display, else 1000 nits, onto 203-nit reference white per BT.2408), and encoded with BT.1886 gamma. The same math runs in the YUV→RGB shader and in `nox_color::hdr` (the CPU path for thumbnails, and the reference in tests). In the working space the same mapping is applied on the way in, without the final encoding. For HDR exports the converter keeps the HDR light instead (decision 27).
15. **Export writes into the encoder's own images.** For Vulkan Video encoders (`h264_vulkan`, `hevc_vulkan`) on the shared device, `MediaWriter` creates an NV12 frames pool with storage usage (mutable and extended-usage images, so the compositor can view each plane). `output_yuv.comp` writes BT.709 limited-range Y'CbCr (left-sited 4:2:0 chroma, the matrix being the exact inverse of the decoding one) into a pool frame. It follows FFmpeg's protocol: lock, wait on the semaphore, signal the next value, record GENERAL layout and no access (the semaphore makes the writes available to the encode queue). The frame then goes to the encoder: no readback, no CPU conversion. Fallbacks, in order: NVENC (frames shared through CUDA, decision 28), then software (x264, x265). ProRes is always software, with 24-bit PCM audio in MOV. Exports render at sequence resolution with their own renderer, mix audio frame by frame, tag BT.709, put keyframes every 2 s, and use `+faststart`; a cancelled export deletes its file.
16. **Dialogs follow other editors.** Project settings are laid out like Resolve's (sidebar of categories). Export leads with presets like Resolve's Deliver page and Shotcut's export panel, with Shotcut's single "Use hardware encoder" switch, and shows what will be rendered (as Kdenlive does).
17. **Layout in `.ui` templates, behavior in Rust.** The window, viewer, media pool and its cells, both dialogs and the menus are GTK composite templates in plain `.ui` XML (`crates/nox-gui/resources/ui/`; no Blueprint, so no extra compiler). They are compiled into a GResource by `build.rs` (`glib-build-tools`; `.ui` files compressed and with blanks stripped by `xml-stripblanks`, which needs `xmllint`; without it the build warns and embeds them unstripped) and embedded in the binary under `/io/github/feliwir/Nox/`, with `style.css`. Each template has a Rust subclass (`NoxWindow`, `NoxViewer`, `NoxMediaPool`, `NoxMediaCell`, `NoxProjectSettings`, `NoxExportDialog`) that binds its children by id (`TemplateChild`) and its signal handlers with `#[template_callbacks]`. Things computed at run time (dropdown models from the preset tables, the clip labels) stay in Rust. List rows and their data are declared too:
    - **Row templates:** the media pool and the clip strip use `GtkBuilderListItemFactory` with row templates (`media-pool-item.ui`, `clip-strip-item.ui`).
    - **Data binding:** cells read their item through `<binding>`/`<lookup>` expressions, and `<closure>` calls template callbacks (`#[template_callback]`) where a value needs computing.
    - **What Rust still does:** fill the list store and react to selection. GObject properties (`MediaObject:name`, `NoxMediaCell:media`, `NoxClipRow`) are the interface between the two. The timeline is drawn in code (`snapshot`), so it has no template; the window template places it, and `TimelineView::set_project` attaches the project.
18. **Translations with gettext.** The domain is `nox`; `i18n::init` binds it at startup. Rust uses `gettext`, `pgettext` (context, e.g. "blend mode"), `ngettext` (plurals) and `n_` (marks entries of constant tables, translated where shown). Placeholders are named, `{name}`, and filled by `i18n::fill`, so translators can reorder them; `format!` around translated text is avoided. `.ui` files mark text `translatable="yes"` with `<interface domain="nox">`. `build.rs` compiles `po/<lang>.po` for every language in `po/LINGUAS` into `.mo` catalogs itself (a small writer, no gettext tools needed to build). The binary looks for catalogs in `NOX_LOCALEDIR`, then `../share/locale` next to an installed `bin/nox`, then the build directory. Format names (H.264, ProRes, OpenTimelineIO) and units stay untranslated.
19. **Ripple and roll follow Premiere's and Resolve's selection tool.** There is no separate trim mode: Ctrl while dragging an edge makes it a ripple trim (Premiere's Ctrl-drag), and the middle 6 px of a cut between two clips rolls it (Resolve's selection mode); either side of that zone trims the clip on that side. A ripple-trimmed start stays where it is and the clip's media start moves, as in both editors. In/Out marks are stored as `RationalTime`s in the timeline's `metadata.nox` (`mark_in`, `mark_out`; Out is exclusive, so O includes the frame under the playhead), track mute/hide is OTIO's `enabled`, and the track lock is `metadata.nox.locked`. Track renaming opens a small window of its own (`rename-track.ui`) rather than an inline entry, because the main window's single-key accelerators (Space, I, O, …) would otherwise eat the typing.

20. **Color management in a linear working space** (`nox_color::working`).
    - **Working space:** linear ACEScg (AP1 primaries, ACES white), with SDR reference white at 1.0.
    - **Input:** the R'G'B' signal is decoded with its transfer function. That is BT.1886 (a pure 2.4 power) for BT.709/601/2020 video and for untagged sources, so untagged JPEGs look as before, and the sRGB EOTF for sRGB-tagged ones. The result goes to AP1 from its tagged primaries (unknown ones count as BT.709), with Bradford adaptation from D65 to the ACES white, as OCIO's ACES configs do. HDR stays tone-mapped per source on the way in (decision 14), except for HDR exports (decision 27).
    - **Output:** AP1 to BT.709, clipped to the gamut, BT.1886-encoded. That is the inverse of the SDR input, so BT.709 footage comes out with the code values it went in with.
    - **Viewer color state:** frames are tagged sRGB (`GdkColorState`). GTK then passes the code values to an SDR monitor unchanged, which treats the desktop monitor as the BT.1886 reference display, as video players do. Tagging them honestly as BT.709 (CICP 1/1) would have made GTK apply the BT.709 inverse OETF: dark grays would lift by about 6 code values at 10% (`0.1 → 0.16`), unlike every player. On an HDR monitor GTK maps sRGB to its SDR white.
    - **One source of truth:** the matrices are computed in `nox-color` (from xy chromaticities, checked against published ACES and BT.2087 matrices) and written into `color_constants.glsl` by `nox-gpu`'s build script, which the shaders include.
    - *Found during M6:* indexing a constant `mat3[]` array with a push constant returned wrong matrices (glslang 16, NVIDIA 580; colors came out hue-shifted while gray was right). The generated code uses a function with an `if` per primaries instead.
    - Blend modes keep their formulas but now act on linear values. Multiply and screen differ from their gamma-space look, and screen is only meaningful for values up to 1. A working-space choice (ACEScct, closer to the "video" look for blend modes) may follow.

21. **Edit and Color pages, as in DaVinci Resolve.**
    - **Switching:** a switcher in the header bar swaps between two layouts of the same project, in a `GtkStack`. Each page is laid out for its job.
    - **Edit:** media pool, viewer, a new inspector on the right (clip properties such as opacity, blend mode and volume, which only the clip menu has today, and later transforms and transitions), and the timeline.
    - **Color:**
      - a large viewer with the scopes beside it (2×2, or one enlarged)
      - a clip strip of the timeline's clips
      - the color wheels (lift, gamma, gain, offset) across the bottom, next to CDL/LUT controls
    - **Shared viewer:** there is one viewer and one engine. The viewer widget moves to whichever page is visible, so nothing renders twice.
    - **Cost:** scopes run only while the Color page is visible.
    - **Rejected options:**
      - A single page with a color tab in the inspector (Premiere's Lumetri): the wheels get cramped, and the scopes compete with the media pool.
      - Dockable panels (Kdenlive, Blender; libpanel in GTK): the most flexible, but a new dependency and much UI work before any color feature ships. Saved workspaces can come later on top of the pages.

    - *Found while building it:* the Color page's vertical splitter made GTK warn on every resize ("Trying to measure GtkBox for height of 168, but it needs at least 169"). When GTK computed the window's minimum size, the paned handed its non-shrinkable end child one pixel less than that child's minimum. The bottom area is now shrinkable and scrolls instead. A fixed splitter position hid the clip strip and wheels on small screens, so the initial position leaves the bottom 380 px (clip strip and wheels), at most half the page, and the user's position is kept after that. The page stack sizes only to the visible page (`hhomogeneous`/`vhomogeneous` off).

22. **Shortcuts: application accelerators only with Ctrl or Alt.**
    - **The problem:** GTK runs application accelerators (`set_accels_for_action`) before the focused widget. As accelerators, Space, the arrow keys, Home/End and the single letters took keys from focused controls: Space from buttons, arrows from the inspector's spin buttons and from the media pool.
    - **The rule:** application-wide, only shortcuts with Ctrl or Alt, in `main.rs`. Keys that type a character or navigate live in a window-level `GtkShortcutController` declared in `window.ui`, in the bubble phase, so the focused widget gets them first. That covers Space, L, K, I, O, the arrows, Home/End, Up/Down, Shift+V and the Shift+digit page keys.
    - **Buttons** name their keys in the tooltip ("Add Marker (M)") and in the accessible `key-shortcuts` property.
    - **Shift+4, Shift+6, Shift+9** (pages, project settings) are matched by the physical key in `window.rs`, not by `<Shift>6` triggers. On most layouts Shift plus a digit key types another character (`^` on US English, `&` on German), and the triggers never matched. Real key input in the headless session showed this; until then they had only been tried through D-Bus.
    - **Dialogs** are separate windows without the application accelerators, so they can use mnemonics (`_Preset`, `_Save`). In the main window, labels are linked with `labelled-by` instead, because a mnemonic such as Alt+O would collide with Clear Out.
23. **Accessibility basics.**
    - Icon-only buttons have accessible names.
    - Form labels are linked to their controls.
    - Dialogs have default buttons. Escape during an export cancels it first, and the outcome is announced.
    - The timeline, drawn in code, is one accessible element (role Group) with a name, a description of its keys and a focus ring. It announces the selected clip ("name, start to end").
    - Keyboard: Up/Down jump to edit points, Shift+V selects the clip at the playhead (Resolve), and Shift+F10 or the Menu key opens its menu.
    - Track headers are widgets (`track_header.rs`, children of the timeline), with toggle buttons whose accessible names include the track ("Mute A1") and which announce their pressed state. Shift+F10 in a header opens its track menu.
    - *Not done yet:*
      - Clips can't be moved or trimmed by keyboard.

24. **Scopes are counted on the GPU, then drawn** (`nox_gpu::scopes`, after Aras Pranckevičius's Blender GPU scopes: "This many points is surely out of scope", https://aras-p.info/blog/2025/08/24/This-many-points-is-surely-out-of-scope/).
    - **Why not draw points:** drawing one blended point per pixel stalls where points pile up, and most pixels sit near the vectorscope's center.
    - **Count pass** (`scope_count.comp`):
      - runs on the accumulator the compositor just blended, in the same submission
      - measures after the output transform, which is what an export contains
      - adds one per pixel to a bin with atomics; a picture narrower than a scope fills every column its pixels cover
      - tallies the histogram's few bins in shared memory per workgroup first, because they would be contended
    - **Resolve pass** (`scope_resolve.comp`): writes four small RGBA8 images, with brightness the logarithm of the count relative to a picture-size reference. They are read back with the frame.
    - **Layout:** the sizes and the count buffer layout are in `src/scope_layout.rs`, which `build.rs` also includes and writes into `scope_layout.glsl`.
    - **GUI:** the GUI draws the graticules: levels in percent, the 75% bar targets, and the skin-tone line at about 123° from +Cb.
    - **When:** scopes are measured only while the Color page is visible (`Engine::set_scopes`). Turning them on renders cached frames again.
    - **Cost:** at 1280×720 about 0.1–0.3 ms per frame, readback included (RTX 5060, release build).

25. **Primary grades per clip, in ACEScct** (`nox_color::grade`).
    - **Storage:** a clip's grade lives in `metadata.nox.grade` as `lift`, `gamma`, `gain` and `offset`, each `[r, g, b, master]`, and is left out when neutral.
    - **Where it runs:** in the blend pass (`blend.comp`), on the layer's linear ACEScg values. They are converted to ACEScct, graded, and converted back before blending. So the grade comes before compositing, as in Resolve, and cached stills are graded without a copy.
    - **The formula:** per channel, `v' = max(v·(gain−lift) + lift + offset, 0)^(1/gamma)`.
      - Lift pivots at 1.0, gain at 0, and gamma bends the middle.
      - The RGB and master values fold into one slope, add and power per channel, which the shader gets as push constants.
      - Neutral is an exact identity.
    - **Why ACEScct:** in linear light, lift and gamma would act almost only on the darkest shadows. ACEScct is the encoding ACES defines for grading.
    - **The wheels** (`color_wheels.rs`):
      - A puck position is a Cb/Cr direction, turned into luma-neutral (BT.709) R, G and B changes, so the ring shows the hue a push goes toward.
      - Lift and offset add, up to ±0.1 in ACEScct; gamma and gain multiply, up to 2^±0.5.
      - The master sliders cover −1…1, mapped per wheel.
      - Each drag is one undo step (`edit_merging`). Double-click or Home resets a wheel.
    - **Naming:** "Gain" means only the wheel. The audio level of a clip is its **volume** (`metadata.nox.volume`), as in Resolve's and Premiere's inspectors. *Changed during M6:* it was called gain, in the UI and in the project format.
    - **Caveat:** the channels graded are AP1's. Pushing a wheel toward red therefore also lowers BT.709 green and blue a little, as in Resolve's ACES modes. The ring's hues are a BT.709 approximation.

26. **No main menu; tools are buttons where they are used** (asked for in review).
    - **Comparison:**
      - Resolve: a menu bar, page buttons at the bottom, a toolbar above the timeline (edit modes; insert, overwrite and replace; blade; snapping; flags and markers). Mark In/Out sit under the viewer.
      - Premiere: a menu bar and an Export button at the top. The Program Monitor has a button row (marker, In/Out, step, play, insert/overwrite). The timeline panel has snap, linked selection and add marker at its top left, beside the Tools panel.
      - Shotcut and Kdenlive: a main toolbar (open, save, undo, redo, export) and a toolbar directly above the timeline.
      - Blender's video editor: header menus (View, Select, Marker, Add, Strip) and a tool shelf.
      - None of them hides timeline actions in a menu. The menu bars exist for the long tail, which Nox doesn't have yet.
    - **Nox:**
      - The header bar has file actions and undo on the left, the page switcher, the project's name, then Project Settings and a highlighted Export (as Premiere's).
      - The timeline toolbar is laid out like Resolve's: edit actions on the left, track actions on the right.
      - Marks and markers are on the viewer, as in Premiere's monitor.
      - The remaining clip and track actions are in their context menus (right-click or Shift+F10).
      - Without a primary menu, F10 is free, so it overwrites as in Resolve.
      - The tool icons are the app's own symbolic SVGs (`resources/icons`), because Adwaita has none for editing.
    - **Media pool:** no rubber band (asked for). Selection is by click, Ctrl-click and Shift-click.

27. **HDR export: SDR white at 203 nits, HDR sources kept, 10-bit BT.2020** (`nox_color::OutputEncoding`).
    - **Levels:** working 1.0 is SDR reference white, 203 nits (BT.2408), so SDR clips sit where HDR deliveries expect them: PQ 0.58, and HLG 0.75 on its nominal 1000-nit display. Highlights from HDR sources go above 1.0, up to 49 (10000 nits).
    - **Sources:** for an HDR export, HDR clips enter the working space with their light (`InputTransform::apply_hdr`, the converter's `keep_hdr`) rather than tone-mapped. The viewer and SDR exports still tone-map per source (decision 14).
    - **Output:** working → BT.2020 → PQ, or for HLG the inverse BT.2100 OOTF (system gamma 1.2) and OETF. This runs in `color.glsl`'s `encode_output` and on the CPU as the tests' reference. Frames go to NVENC as P010 on the GPU (decision 28). Software encoders get RGBA16 read back from the GPU, converted by swscale with BT.2020 coefficients.
    - **Files:** HEVC Main 10 (NVENC with `main10`, or x265) or ProRes 422 10-bit, tagged BT.2020 / PQ or HLG / BT.2020 NCL. HDR10 files carry the mastering display: P3 primaries, D65, the chosen peak (1000 nits by default), 0.0001 nits black. It goes in the encoder (SEI) and in the container (`mdcv`). MaxCLL and MaxFALL are measured from every rendered frame: the brightest pixel and the brightest frame average of max(R, G, B). Frames on the GPU are measured by the output shader that writes them (`light.glsl`: per-workgroup partials, summed on the CPU); read-back frames on the CPU, with a 16-bit PQ lookup table and rows in parallel. Both give the same numbers. They're written into the container (`clli`, which MP4 and MOV write last) but not into the bitstream, which is encoded before they're known. On a 4K clip whose metadata claims 1000/200 nits, the measurement found 2808/220. H.264 stays SDR.
    - **Not zero copy:** writing P010 straight into Vulkan Video encoder images gave wrong pictures on NVIDIA 580 (chroma near zero, striped luma). Spec-correct 16-bit plane views, the planes' own 10-bit formats and staging copies all came out the same, while only an invalid 8-bit view happened to look right. The driver and the spec disagree about those planes, so HDR goes to NVENC through CUDA instead (decision 28). `NOX_HDR_VULKAN_ENCODE=1` tries Vulkan Video anyway (16-bit views, `output_yuv_deep`).
    - **Tested:** gray clips exported as PQ and HLG with NVENC and x265 land within 3 code values of the levels the SDR export's light implies. A PQ source passes through an HDR10 export unchanged. Tags and mastering data were checked with ffprobe and under the validation layer, and an export was made from the dialog with real input.

28. **Hardware decoders and encoders keep frames on the GPU, whatever their API** (`nox_media::hwaccel`, `nox_gpu::interop`).
    - **Shape:** each API wants frames in its own memory. A backend moves them between that API and the device Nox renders with, both ways. Decoding turns a decoded hardware frame into a `nox_gpu::Input` for the converter (`with_gpu_input`). Encoding uses a sink (`GpuSink`), which owns the encoder's FFmpeg frames pool and lends each frame to the renderer as a `nox_gpu::GpuTarget`. The compositor writes Y'CbCr 4:2:0 into it (`Compositor::finish_to_gpu`), and the frame goes to FFmpeg as a hardware frame. `MediaWriter::write_gpu` and `Renderer::render_to_gpu` don't know which API it is.
    - **One place for devices:** `Accel` holds FFmpeg's Vulkan device (shared with nox-gpu), and opens other APIs' devices on the same GPU when first needed and keeps them (CUDA, matched by UUID). Decoders, encoders, exports and encoder probing all take their devices from it; before, every export opened a CUDA device of its own. FFmpeg's `AVVkFrame` protocol (lock, semaphores, state write-back, the 64-bit access flags of FFmpeg 9) is one helper (`LockedFrame`) for both directions, and so are the plane views.
    - **Targets:** `Images` are another user's Vulkan images, synchronized by FFmpeg's semaphore protocol (Vulkan Video, decision 15). `Buffer` is a `SharedBuffer`, a dedicated device-local buffer whose memory is exported (`MemoryHandle::OpaqueFd`). `output_linear.comp` writes NV12 or P010 into it, pitch-linear (rows aligned to 256 bytes, chroma after luma), as whole 32-bit words. Ownership moves from and to `VK_QUEUE_FAMILY_EXTERNAL`, and the compositor waits for its work before returning.
    - **CUDA (NVENC):** libcuda is loaded at run time (libloading), so Nox runs without it. The CUDA device is the one whose UUID matches the Vulkan device's. FFmpeg creates the CUDA device and frames context, and the buffers are imported into its context (`cuImportExternalMemory`, dedicated) and mapped as device pointers. Each frame is an `AV_PIX_FMT_CUDA` frame over a buffer, and the `AVBufferRef` free callback returns the buffer to the pool once NVENC is done with it. NVENC registers each pointer once, so the pool is stable: it grows to as many frames as the encoder holds (32 at most). Without a CUDA driver or a matching device, NVENC falls back to frames from memory. `NOX_NVENC_MEMORY=1` forces that path.
    - **Decoding** (`HwDecode`, chosen per file when it opens):
      - Vulkan Video, if the device's Vulkan decoder handles the codec (`HwDevice::decode_operations`) and the format is 4:2:0. Frames are used in place.
      - Otherwise NVDEC, if FFmpeg has it for the codec and a CUDA device matches. NVDEC fills Vulkan Video's gaps: 4:2:2 from cameras on recent GPUs, and codecs Vulkan Video lacks. Each decoded CUDA frame is copied once on the GPU (`cuMemcpy2DAsync` on FFmpeg's CUDA stream) into a pooled `SharedBuffer`. The converter reads it as `Input::Buffer`: it acquires the buffer from the external queue family, copies both planes into its own plane images, and releases the buffer again. NV12, P010, NV16 and P210 (4:2:2) are supported.
      - Otherwise VAAPI (Linux), if FFmpeg has it for the codec and the Vulkan device's DRM render node (`VK_EXT_physical_device_drm`, `GpuDevice::drm_render_node`) has a VA driver. Decoded frames are DMA-BUFs, and FFmpeg maps them to Vulkan images on Nox's device (VAAPI → DRM PRIME → `AVVkFrame`, imported with their modifiers; `av_hwframe_ctx_create_derived` and `av_hwframe_map`). The converter reads them in place, like Vulkan Video's frames, with no copies. The derived frames contexts are cached per decoder. Mapped images have one image per plane, so per-plane views need no mutable format.
      - Otherwise FFmpeg's choice, which falls back to software.
      - `NOX_DECODE=auto|software|vulkan|cuda|vaapi` forces a choice, and `VideoDecoder::open_with` takes one.
    - **Decoding speed:** on 4K60 H.264, decoding, conversion and compositing take 1.8 ms per frame with NVDEC, the same as Vulkan Video and less than software decoding with a CPU upload (2.7 ms). The copy into the shared buffer takes 0.2 ms.
    - **4:2:2:** NVDEC on the RTX 5060 with driver 580 decodes 4:2:2 10-bit HEVC with FFmpeg 9 (P210 frames), which the packages ship; the system's FFmpeg 8.0 refuses it. Tested against FFmpeg 9: the picture matches software decoding within 1 code value.
    - **VAAPI, tested on AMD:** the development machine's Ryzen 9 9950X3D has a Radeon iGPU (RDNA 2, RADV and radeonsi). With Nox's Vulkan device there (`NOX_GPU=RADV`):
      - VAAPI output is bit-exact with software decoding, and mapped frames convert exactly like downloaded ones, with no validation messages.
      - The whole nox-media and nox-engine test suites pass on RADV, so Nox's GPU code works on AMD.
      - On 4K60 H.264, Vulkan Video, VAAPI and software decoding all take 4.3–4.6 ms per frame including conversion and compositing. The iGPU's compositing is the bottleneck there; VAAPI's decode is 0.1 ms.
      - Intel's VA driver remains untested.
    - **VAAPI encoding** (`h264_vaapi`, `hevc_vaapi`, after NVENC in the lists): `VaapiSink` allocates the encoder's VAAPI surfaces (NV12, or P010 for HDR, in a growing pool). Each surface is mapped for writing to Vulkan images through a derived Vulkan frames context. The compositor writes into them like into Vulkan Video's images (one shared `render_into`), and the surface then goes to the encoder: no copies.
      - On the Radeon iGPU at 4K60: H.264 at 40 fps (Vulkan Video 39), HEVC at 53, and HDR10 at 41, with MaxCLL/MaxFALL.
      - In the export dialog it's "VAAPI (GPU)". Encoders the dialog doesn't know show FFmpeg's name instead of an empty entry.
    - **Zero copy first:** encoders that take frames on the GPU Nox renders with are tried before those fed from memory, each group in order of preference. Rendering on the iGPU, HDR goes to VAAPI rather than to NVENC on the other GPU through memory. On the NVIDIA card nothing changes.
    - **Light levels for every GPU target:** `output_yuv.comp` (Vulkan Video and VAAPI images) measures MaxCLL/MaxFALL like `output_linear.comp`, through a shared `light.glsl`. HDR10 files from those encoders now carry `clli` too; before, only NVENC's did.
    - **Next APIs** fit the same shape, each with a `MemoryHandle` kind, a backend, its device in `Accel`, and its encoders in `open_sink`:
      - D3D11 via shared NT handles (D3D11VA decoding; AMF, QSV and NVENC on Windows).
      - VideoToolbox via IOSurface.
    - **Speed:** 4K60 HDR10 to `hevc_nvenc` went from 17 fps (readback and swscale) to 108 fps (RTX 5060, release). The output matches the memory path (PSNR 56 dB, the same MaxCLL/MaxFALL). It's also faster than Vulkan Video's SDR zero-copy path at 57 fps.
    - **Choosing an encoder** (asked for): the export dialog's Encoder list offers "Automatic" (the first that works, best first) and every encoder that works on this system for the chosen format and dynamic range. A short note under the list says what the chosen encoder is good at. Encoders can be compiled into FFmpeg without the hardware or driver they need, so `nox_media::probe_encoders` opens each one at 1080p. `nox_engine::available_encoders` probes every codec × SDR/HDR once (about 1.7 s, on a thread while the dialog says it's checking) and caches the result. The choice goes to `OutputSettings::encoder`. `nox-cli encoders` lists them, and `nox-cli render --encoder NAME` uses one. On the development machine: H.264 Vulkan Video, NVENC and x264; HEVC Vulkan Video, NVENC and x265 (HDR: NVENC and x265); ProRes ffmpeg's `prores_ks`.
    - **AV1 and VP9 exports** (asked for): AV1 and VP9 in the export dialog, and `--codec av1|vp9` in the CLI (VP9 is the default for `.webm`). There's also a preset, "Web, smallest files (AV1)". Both codecs can be HDR (10-bit).
      - AV1 tries `av1_vulkan` (off by default, see below), `av1_nvenc`, `av1_vaapi`, then SVT-AV1; the hardware ones use the existing GPU sinks, so they're zero-copy. On the RTX 5060, 4K60 AV1 through NVENC runs at 116 fps, and HDR10 at 114 fps with MaxCLL. SVT-AV1 runs at 19 fps, with files less than half the size.
      - `av1_vulkan` is off unless `NOX_AV1_VULKAN_ENCODE=1`. On NVIDIA 580, FFmpeg 8's loses the device (`VK_ERROR_DEVICE_LOST`). FFmpeg 9's writes undecodable streams at any quantizer but its default (18, which makes far too large files). Even with the override, Nox skips it before libavcodec 63. Its `qp` is AV1's quantizer index (0–255).
      - VP9 tries `vp9_vaapi`, then libvpx (12 fps at 4K). WebM gets Opus audio (192 kb/s, interleaved floats).
      - Encoders now use FFmpeg's threads on all cores, as the `ffmpeg` tool does; libvpx was ten times slower without them, and ProRes gains too.
      - Opus drops DC, as lossy audio codecs do, so the export tests' constant-level "tone" isn't checked for WebM.
    - **Container, video codec and audio codec chosen separately** (asked for): the export dialog has a Container list (MP4, QuickTime, Matroska, WebM, AVI), then the video codecs and audio codecs that container holds (`nox_media::Container`, `AudioCodec`):

      | Container | Video | Audio (usual first) |
      |---|---|---|
      | MP4 | H.264, HEVC, AV1, VP9 | AAC, Opus, MP3, FLAC |
      | QuickTime | ProRes, H.264, HEVC | AAC, PCM 24, PCM 16 (PCM 24 with ProRes) |
      | Matroska | all five | all six (Opus with AV1/VP9, PCM 24 with ProRes) |
      | WebM | VP9, AV1 | Opus |
      | AVI | H.264 | PCM 16 |

      - The container and the file's extension follow each other. Picking a container sets the extension (`.m4v` stays, it's MP4 already), and typing or browsing to a known extension picks the container. The container's muxer is used whatever the extension.
      - Changing the container keeps the chosen codecs if it holds them. Otherwise it switches to the container's usual ones; for HDR, that's the first codec that carries HDR. Choosing HDR in AVI, which holds H.264 only, switches to MP4. Presets set all three.
      - AVI keeps no presentation timestamps, so its encoders run without B-frames. It doesn't offer MP3 either: LAME's encoder delay makes the muxer shift every stream, and AVI fills the shifted video with an empty frame, so frames come out one late.
      - FLAC is 24 bit; AAC and MP3 are 320 kb/s, Opus 192 kb/s.
      - In the CLI the container comes from the extension (`.avi` and `.webm` too). `--audio aac|opus|mp3|flac|pcm16|pcm24` picks the audio codec.
      - The audio decoder now discards the file's other streams. Otherwise, seeks in Matroska files stopped at video blocks ("keyframes not correctly marked").
      - Tested: every container with every audio codec it holds (`audio_codecs_in_every_container`), and exports to MKV (HEVC) and AVI.
    - **Tiled photos** (HEIC from phones, AVIF): FFmpeg demuxes each tile as a stream and describes the layout in a tile grid stream group. Nox's decoder, probe and thumbnails decode the tiles once in software, assemble them in their own planar format (any depth and subsampling), and crop to the grid's picture (`nox_media::tiles`). `tests/data/tiled.heic` (640×480 in 3×2 tiles of 256, made with libheif) checks the result against the pattern it was made from.
    - **Tested:**
      - Encoding: NV12 through CUDA (`nvenc_encode_from_gpu_memory`), and HDR10/HLG through CUDA (`hdr_exports_encode_pq_and_hlg`: levels within 3 codes, MaxCLL/MaxFALL within 2 nits).
      - Decoding:
        - `nvdec_decode_matches_software`: NVDEC output is bit-exact.
        - `nvdec_frames_convert_on_the_gpu`: frames through the shared buffer convert exactly like downloaded ones.
        - `automatic_decoding_picks_the_backend`: Vulkan Video for 4:2:0, never for 4:2:2.
        - `linear_buffers_round_trip`: compositor → NV12/P010 buffer → converter, on the GPU.
      - None of these produce validation layer messages beyond the allowlisted FFmpeg ones.

29. **Packages: Flatpak, self-contained ZIPs, a Windows installer** (asked for; `xtask`, `packaging/`, `.github/workflows`).
    - **App identity:** the app ID is `io.github.feliwir.Nox` (a GitHub ID the project owns, as Flathub requires). `data/` has the desktop entry, AppStream metainfo with a screenshot, a MIME type for `.otio` projects, and the icon: a crescent moon over a timeline, as SVG, as a symbolic SVG and as a Windows `.ico`. The GResource bundle carries the icon too, for running uninstalled.
    - **One tool:** `cargo xtask install --prefix DIR` installs the programs, translations (compiled by the same code as `build.rs`, `crates/nox-gui/po.rs`), desktop files, the license, and the license texts of all built-in Rust crates. `cargo xtask dist` builds the self-contained package for the platform it runs on. Each platform's bundler compiles everywhere, so all of them are type-checked everywhere.
    - **One FFmpeg:** `packaging/ffmpeg/build.sh` builds FFmpeg 9.0.2 with each platform's hardware APIs, the same everywhere (the Flatpak manifest mirrors it). Besides FFmpeg's own codecs (288 video and 220 audio decoders, 366 container formats: ProRes and ProRes RAW, DNxHR, CineForm, MXF, VVC and more), it has x264, x265, dav1d (AV1 decoding), SVT-AV1, libvpx, libjxl (JPEG XL), Opus and LAME. It also installs the `ffmpeg` tool, which tests use to make their clips (the packages don't include it). The Linux image adds Vulkan headers 1.4.341 over Debian's 1.4.309, because FFmpeg's AV1 Vulkan encoder and VP9 Vulkan decoder need 1.4.317. It uses nv-codec-headers 13.0, so NVENC works with NVIDIA drivers from 570. The runtime's and the distributions' FFmpeg builds use 13.1, which needs driver 610 and was unusable on the development machine's driver 580. FFmpeg 9 made `AVVkFrame::access` 64-bit (`VkAccessFlags2`); `ImageSync::access` is `AccessFlags2` now, converted for FFmpeg 8 and 9 alike, so high bits such as video decode writes aren't lost.
    - **Flatpak** (`packaging/flatpak`): GNOME 51 runtime with x264, x265, nv-codec-headers and FFmpeg as modules. Crates are vendored from `cargo-sources.json`, which `cargo xtask flatpak-sources` writes from `Cargo.lock` (no Python generator). The build links `/app`'s FFmpeg explicitly: `alsa-sys` puts the SDK's library directory on the link path, where the SDK's FFmpeg 8 shadowed ours, and FFmpeg 9 bindings against FFmpeg 8 libraries broke Vulkan device creation. A build step now fails if the linked and built sonames differ. Permissions are Wayland or X11, PulseAudio, all devices (Vulkan, and `/dev/nvidia-uvm` for CUDA) and the host file system (projects refer to media by path). In the sandbox, Vulkan Video decoding, DMA-BUF presentation, Vulkan Video encoding (58 fps 4K HEVC) and NVENC through CUDA (113 fps 4K HDR10) all work.
    - **Linux ZIP:** built in Debian 13 (`packaging/linux/Dockerfile`, glibc 2.41 baseline). It bundles the shared-library closure except what every desktop has (glibc, the C++ runtime, graphics drivers and loaders, X11 and Wayland, ALSA, fontconfig, freetype, harfbuzz, D-Bus), as AppImage's exclude list does. `patchelf` sets `$ORIGIN` rpaths, so nothing leaks into child processes through `LD_LIBRARY_PATH`. Launchers set GTK's data paths, its own module directories (so the system's GTK modules aren't loaded) and the SVG pixbuf loader's cache. 40 MB zipped, 71 libraries; with Debian's FFmpeg it was 106 MB and 192. It ran on Debian 13, Ubuntu 25.10 and Fedora 42 with a minimal desktop base, and on the development machine with its own GTK 4.18 and FFmpeg.
    - **Windows** (x64 and ARM64): MSYS2's CLANG64 and CLANGARM64, with Rust's `*-pc-windows-gnullvm` targets, rather than vcpkg. MSYS2 has prebuilt GTK, librsvg and codecs, which takes CI from hours to minutes, and vcpkg's FFmpeg had no Vulkan. The ZIP has GTK's standard layout (`bin/` with the DLLs, `share/`, `lib/`), so GTK finds its data with no launcher. DLLs are found by reading PE import tables (`xtask/src/windows.rs`). The Inno Setup installer (`packaging/windows/nox.iss`) installs per user without administrator rights, or for all users. It adds a Start menu entry, an optional desktop icon and an optional `.otio` association, in English and German. Release builds have no console window and carry the icon (`winresource`). Shared buffers export NT handles (`OPAQUE_WIN32`) for CUDA from `nvcuda.dll`, so NVENC can take frames from the GPU on Windows too.
    - **macOS** (ARM64): `Nox.app` built from Homebrew's libraries, copied into `Contents/Frameworks` and relinked to `@rpath`. It includes the Vulkan loader and MoltenVK with its ICD file, and is re-signed ad hoc. The launchers are shell scripts, because ash and FFmpeg open the Vulkan loader by name, which only `DYLD_LIBRARY_PATH` can point into the bundle. The ZIP is made with `ditto`, which keeps the signatures. Without a Developer ID, Gatekeeper asks before the first start (unsigned for now, as decided).
    - **Porting:** DMA-BUF (`DmabufFd`, GTK's DMA-BUF textures) is Linux-only; elsewhere `ExportPool::new` reports it unsupported and the viewer reads frames back. The workspace type-checks for Windows (MinGW headers for bindgen, in a container) and macOS ARM64 from Linux. Building, testing and packaging on those systems happens only in CI and hasn't run yet.
    - **CI:** `.github/workflows/ci.yml` runs on every push. Linux checks formatting, clippy and the tests in the Debian image; GPU tests run on Mesa's lavapipe, with Vulkan Video and DMA-BUF parts skipping themselves. Windows x64 and macOS run clippy and the tests. The Flatpak is built. `release.yml` (on `v*` tags, or by hand) builds all packages, and on tags writes them with `SHA256SUMS.txt` into a draft release.

### Display formats

GTK's Vulkan renderer on the development machine imports these DMA-BUF formats: `AB24`, `XB24`, `AR24`, `XR24` (8-bit RGB), `AB4H` (16-bit float RGBA), `AYUV`/`XYUV`/`AVUY`/`XVUY` (packed 8-bit YUV) and `R8`. Nox outputs `AB4H` (half float) when GTK offers it, else `AB24`, in both cases tagged sRGB (decision 20). A headless GNOME Shell session on the development machine imported `AB4H` with NVIDIA's tiled modifiers, zero copy. Handing GTK the decoder's YUV directly isn't useful for an editor, since it would bypass compositing and color management.

## Building and running

Linux (system libraries):

```sh
sudo apt install libgtk-4-dev libavcodec-dev libavformat-dev libavutil-dev \
                 libavfilter-dev libavdevice-dev libswscale-dev libswresample-dev \
                 libasound2-dev glslang-tools libxml2-utils gettext
cargo run -- clip.mp4 photo.jpg      # starts Nox and imports the files
cargo test --workspace               # needs the `ffmpeg` CLI for test clips
tools/vk-validate.sh cargo test --workspace -- --test-threads=1   # under validation
```

Windows: in an MSYS2 CLANG64 (or CLANGARM64) shell, install the packages `.github/actions/windows-env/action.yml` lists and FFmpeg with `packaging/ffmpeg/build.sh "$MSYSTEM_PREFIX"`, then use Rust's `x86_64-pc-windows-gnullvm` toolchain (`CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER=clang`). macOS: `brew install` the packages `.github/actions/macos-env/action.yml` lists, build FFmpeg with `packaging/ffmpeg/build.sh ~/ffmpeg`, and set `PKG_CONFIG_PATH`, `GETTEXT_DIR` and `RUSTFLAGS` as that file does. macOS has no Vulkan Video (MoltenVK); decoding falls back to VideoToolbox or software.

### Packages

`cargo xtask` is short for `cargo run --package xtask --`; the alias lives in `.cargo/config.toml`, which is git-ignored, so add it locally if you want it (CI spells the command out):

```toml
[alias]
xtask = "run --package xtask --"
```

```sh
cargo xtask install --prefix ~/.local     # programs, translations, desktop files
cargo xtask dist                          # self-contained ZIP for this platform, in target/dist

# Linux ZIP as released (Debian 13, glibc 2.41 baseline):
docker build -t nox-build-linux -f packaging/linux/Dockerfile packaging
.github/scripts/in-linux-env.sh cargo xtask dist        # → target/debian13/dist

# Flatpak (needs org.flatpak.Builder; regenerate the crate list after dependency changes):
cargo xtask flatpak-sources
flatpak run org.flatpak.Builder --user --install --force-clean build/flatpak \
    packaging/flatpak/io.github.feliwir.Nox.yml
flatpak run io.github.feliwir.Nox
```

Windows installers and the macOS app are built by `.github/workflows/release.yml` (decision 29).

### Translations

`po/update-pot.sh` regenerates `po/nox.pot` with `xgettext` from the files in `po/POTFILES` and merges it into each `po/<lang>.po` (needs `sudo apt install gettext`). To add a language, copy `nox.pot` to `po/<lang>.po`, set `Language` and `Plural-Forms`, translate, and add the code to `po/LINGUAS`; the next build compiles it. `cargo test -p nox-gui` checks that:
- string literals passed to user-visible setters in Rust (`.label("…")`, `.set_text("…")`, `.title("…")`, edit labels, …) are wrapped in `gettext`; a line that deliberately shows a fixed string carries `// no-i18n`
- `label`, `title`, `tooltip-text` and similar properties in `.ui` files are `translatable="yes"`
- every marked string is in `po/nox.pot` and every file with marked strings is in `po/POTFILES`
- every template builds (with a display: the test turns GTK warnings and criticals into failures, so a renamed id or missing callback fails it)

### Environment variables

| Variable | Effect |
|---|---|
| `NOX_LOG` | log filter, e.g. `info,nox_engine=trace` (trace shows per-frame times) |
| `NOX_GPU` | Vulkan device: index or name substring |
| `NOX_PRESENT=readback` | turn off DMA-BUF presentation |
| `NOX_NVENC_MEMORY=1` | NVENC takes frames from memory instead of through CUDA |
| `NOX_DECODE` | `auto` (default), `software`, `vulkan`, `cuda` (NVDEC) or `vaapi`: which hardware decodes video |
| `NOX_HDR_VULKAN_ENCODE=1` | allow Vulkan Video encoders for HDR (wrong output on NVIDIA 580) |
| `NOX_AV1_VULKAN_ENCODE=1` | allow the AV1 Vulkan Video encoder (undecodable output on NVIDIA 580) |
| `NOX_AUDIO_DEVICE` | output device by name substring; `none` plays without audio (wall clock). For tests, `Discard all samples` is ALSA's null device |
| `NOX_CACHE_MB` | frame cache size (default 768) |
| `NOX_LOCALEDIR` | where to find `<lang>/LC_MESSAGES/nox.mo` |
| `LANGUAGE` | UI language, e.g. `de` (with a UTF-8 `LANG`) |
| `NOX_LOG` for `nox-cli` | defaults to `warn`; `info` shows the encoder and speed |
| `NOX_TEST_ACTIONS=1` | adds `win.screenshot(path)`, which renders the window content to PNG, and `win.close`, which closes the window like its close button (tests only) |
| `NOX_OTIO_PYTHON` | tests: a Python with `opentimelineio`, to compare `nox-otio` output with the reference implementation |

`--gapplication-app-id=…` runs a separate instance next to a running Nox. Window actions are exported over D-Bus (org.gtk.Actions), which tests use to script the app:
- transport: `seek(frame)`, `next-frame`, `previous-frame`, `go-to-start`, `go-to-end`
- editing: `undo`, `redo`, `split`, `delete-clip`, `ripple-delete`, `add-video-track`, `add-audio-track`, `append-media(path)`, `insert-media` and `overwrite-media` (the pool's selection at the playhead), `relink-folder(path)`, `relink-offline` and `relink-clip` (dialogs), `select-clip((track, index))` (`(-1, -1)` clears), `select-clips([(track, index)])` (the first is primary), `clip-opacity(d)`, `clip-blend(s)`, `clip-volume(d)`
- pages: `page` (state `'edit'` or `'color'`)
- timeline: `previous-edit`, `next-edit`, `select-clip-at-playhead`, `add-marker`, `previous-marker`, `next-marker`, `delete-marker`, `select-marker(x)` (-1 clears), `slip-clip(x)` and `slide-clip(x)` (frames, the selected clip)
- marks: `set-in`, `set-out` (at the playhead), `clear-in`, `clear-out`, `clear-in-out`
- tracks, by stack index: `delete-track(x)`, `set-track-enabled((xb))`, `set-track-locked((xb))`, `rename-track((xs))`
- files: `open-path(path)`, `save-path(path)`
- settings: `frame-rate` (state such as `'30000/1001'`), `drop-frame`, `snapping`

For GUI tests, run Nox in a headless GNOME Shell on a private session bus (`dbus-run-session`, `gnome-shell --headless --wayland --no-x11 --virtual-monitor 1400x860 --wayland-display nox-test`, then `WAYLAND_DISPLAY=nox-test GDK_BACKEND=wayland NOX_TEST_ACTIONS=1 nox …`). That is GPU-backed, so DMA-BUF presentation works, and no windows appear on the desktop. `tools/gnome-input.py` injects real pointer and keyboard input through Mutter's RemoteDesktop API. Some quirks:
- The window is maximized below the top bar, so screenshot (window content) coordinates are monitor coordinates plus about 80 px in y.
- The first click only focuses the window, and the first key of a session is dropped; the script spends a Shift tap on it.
- Special keys go as evdev keycodes, because some keysyms never arrive.

GNOME doesn't allow screenshots over D-Bus, which is what `win.screenshot` is for. Xvfb (`GDK_BACKEND=x11 DISPLAY=:99`) also works, with software rendering. Two catches: drag-and-drop needs slow drags for XDND's round trips, and the vertical paned uses a wide handle because a thin handle's invisible grab area covered the top half of the ruler.

## Verification so far

- **101 automated tests:**
  - time and timecode math, Y'CbCr matrices
  - exact time arithmetic across timebases, drop-frame timecode formatting and parsing (round trips at 23.976/25/29.97/59.94)
  - edit operations: overwrite, insert, lift, trim limits and trims, linked moves, removal and splits, normalization, transitions, and keeping the sequence timebase
  - undo/redo and the modified state
  - JSON with OTIO's extensions, and all 13 upstream OTIO sample files: read, write, reread and compare. With `NOX_OTIO_PYTHON` set, OTIO 0.18.1 also loads Nox's output and compares it with its own reading of the original. All 11 files 0.18.1 can read are equivalent, and the two `Marker.3` files become loadable by 0.18.1.
  - probing (video, audio-only, stills, invalid files) and thumbnails for JPEG, PNG, WebP, TIFF and BMP
  - frame-accurate random access with B-frames
  - GPU conversion against swscale for 9 pixel formats, plus a chroma-siting test
  - Vulkan Video decoding bit-exact with software decoding
  - zero-copy input against the download path
  - DMA-BUF content against readback
  - a 200-step random-access stress test with zero-copy DMA-BUF output
- **Real media:** all 2,084 files in `~/Videos` and `~/Pictures` (H.264 and VP8 screencasts, large JPEGs, PNGs) pass probe, thumbnail, decode and GPU conversion, and match the CPU path.
- **App checks:** run through D-Bus actions with screenshots. The DMA-BUF and readback paths produce pixel-identical windows.
- **M4 tests:**
  - dissolves resolving to two layers, audio segments with equal-power crossfades
  - `add_dissolve` limited by media handles, clip properties
  - the compositor against a CPU reference: every blend mode, stacking, pillarboxing, empty frames, and Vulkan-decoded input and DMA-BUF output against plain conversion
  - audio decoding: sample-accurate seeks with resampling (WAV 44.1 → 48 kHz, AAC)
  - the engine end to end: tracks with opacity, a dissolve, the cache, mixer placement and volume, four layers from one file, playback on the wall clock, and on ALSA's null device (the drift guard)
- **M5 tests:**
  - media writer round trips (decode the file and compare frames, colors, rate, tags, duration and audio level): x264, NVENC, HEVC, ProRes with PCM, and zero-copy Vulkan encoding from the compositor
  - export end to end on a timeline with a dissolve and audio, with Vulkan zero copy, x264 and ProRes; the scaling mode reaching the file; cancelling deleting the file
  - RGB→Y'CbCr matrix round trips, placement rects for all scaling modes
  - the zero-copy encode path is clean under the validation layers. FFmpeg's own Vulkan encoder leaks 6 image views at shutdown and logs encode-command errors; the plain `ffmpeg` CLI does the same.
- **M6 tests:**
  - matrices against published ones (BT.709→XYZ, BT.2087, ACEScg→BT.709 with Bradford, AP0→AP1), round trips to and from AP1 for all primaries, transfer function inverses
  - SDR BT.709 round-tripping through the working space, wide-gamut sources being converted, HDR via the working space matching the display path
  - the compositor against a CPU reference that blends in ACEScg with `nox_color`'s transforms; a single layer matching plain conversion within 1 code value
  - engine and export dissolves mixing in linear light
- **M6 app check** (headless GNOME Shell, GPU-backed): frames reached GTK as `AB4H` DMA-BUF, zero copy. Picked colors of SMPTE bars in the viewer were within 1–3 code values of FFmpeg's own decode (downscaling and chroma interpolation account for the rest).
- **M5 app checks** (Xvfb, real clicks): both dialogs open and lay out as intended; exporting 789 frames of the 1080p/4K test project through the dialog took 4.1 s with `h264_vulkan`, zero copy.
- **Editing gaps closed (In/Out, ripple, roll, track controls):**
  - model tests: ripple delete (track ranges, linked clips), ripple trims of both edges with their limits and linked followers, roll limits from both clips' handles and linked rolls, transitions dropped at edited edges, locked tracks rejecting moves, placement, ripple delete and track deletion, splits skipping locked tracks, mark setters and the marked range
  - `.otio` round trip of marks, track `enabled`, lock and name; marks written without the other settings when those are defaults
  - engine: a hidden video track isn't composited, a muted audio track isn't mixed; `marked_range` in frames, also for marks off the frame grid
  - app checks on Xvfb with real XTEST input: I/O keys and the drawn range; Shift+Delete refused while the linked audio track was locked, then ripple-deleting after unlocking with the header toggle; Ctrl-drag ripple trim of a clip start (linked audio followed); a roll of a cut into the incoming clip's handle (saved file checked: 188 + 137 frames, source start 13); hiding V1 (viewer black) and the header menu; renaming through the rename window, typing an "i" without setting a mark; exporting In to Out from the dialog (50 frames, `h264_vulkan`, zero copy) and `nox-cli render --in-out` (50 frames)
- **UI templates and translations:** the lint and template tests above. On Xvfb, the window and both dialogs look as before, and with `LANGUAGE=de` all of them show German text, including the plural forms ("1145 Bilder"). Renaming a template id or leaving a label untranslated makes the tests fail.
- **Scrubbing** the 4K 60 fps clip (keyframes every 250 frames) with real mouse drags, forward and then back:
  - before: one step back froze the viewer for 963 ms, and only 1 frame was shown on the way back
  - after: 33 updates on the way back, each within 3 frames of the playhead, at most 140 ms apart
- **Playback pacing** (headless GNOME Shell with a GPU-backed 60 Hz virtual monitor, on a private session bus; default PipeWire device as the clock):
  - the 4K 60 fps HDR clip played 718 of 718 frames
  - after the clock fixes, frames were held for exactly one refresh, apart from 2 (before: 77 of 715 held for 0 or 2)
  - Xvfb is no good for this: GTK draws with software OpenGL there and manages only about 41 Hz
- **M4 app checks** (Xvfb, with ALSA's null device or no audio, so nothing was heard):
  - composited 50% opacity of a 4K clip over a 1080p clip, zero-copy DMA-BUF at viewer size
  - playback of that composite in a debug build: 137 frames shown and none skipped over 4.6 s at 30 fps, median render 7 ms
  - split + Ctrl+T dissolve (15 + 15 frames at the cut), selecting and deleting it
  - Space/L/K
- **M3 app checks** (Xvfb, XTEST input, screenshots, with two 1080p/4K H.264 clips; D-Bus actions for setup only, the edits themselves are real input):
  - append, then drag a clip to a new V2 track
  - trim a clip whose linked audio follows
  - snap a drag onto the trimmed edge
  - Ctrl+Z, Ctrl+Shift+Z, Delete and Ctrl+B
  - drag from the media pool to a track
  - frame rate change with drop-frame timecode
  - the close prompt with unsaved changes

  Saved files were inspected with OTIO. Opening a saved project and saving it again gives a byte-identical file. The viewer showed the right layer at each position, and black where nothing is under the playhead.

### Vulkan validation

`tools/vk-validate.sh <command>` runs a command under the Khronos validation layer with synchronization validation, and fails on any message not in its allowlist. It replaces `NOX_VULKAN_DEBUG=1` (FFmpeg's `debug` option), for two reasons found in October 2026 (layer 1.4.341, NVIDIA 580):
- The layer's handle wrapping crashes the NVIDIA driver inside FFmpeg's Vulkan Video decode (SIGSEGV in the first `vkCmdDecodeVideoKHR`). Every test binary that decodes on the GPU crashed. The script turns handle wrapping off.
- FFmpeg's debug callback drops some messages; the script injects the layer through the loader and has it print everything.

Results: the whole test suite passes under validation, and every message left also appears with the plain `ffmpeg` CLI, so they come from FFmpeg 8.0 and are allowlisted:
- `VUID-VkImageCreateInfo-pNext-06811`, `…07142`, `…07240`, `…08206`: decode and encode images created `VIDEO_PROFILE_INDEPENDENT` with usage no video format property lists.
- `VUID-VkImageViewCreateInfo-usage-02275`: whole-image views of NV12 inherit `STORAGE` usage.
- `VUID-vkDestroyDevice-device-05137`: the Vulkan encoder leaks 6 image views at teardown.

Positive controls confirmed the layer sees Nox's own commands (a wrong clear layout in the compositor is reported) and that synchronization validation works for transfers. It does **not** report hazards on storage images, storage buffers or sampled images accessed by shaders. A two-dispatch write-after-write on one storage buffer goes unreported on both NVIDIA and lavapipe, as does a barrier removed from the compositor's blend loop. So compute barriers are reviewed by hand.

### Closing the window

Closing the window during playback crashed: a SIGSEGV in the NVIDIA driver on the render thread. The window's controller (and with it the engine) was meant to be released on `destroy`. But GTK 4 emits `destroy` only once the last reference is gone, which the window ↔ controller cycle prevented, so the render thread kept decoding while the driver's libraries were unloaded at exit. The controller is now released when the window leaves the application (`window-removed`), and `Engine::shutdown` stops and joins the worker explicitly. Reproduced and checked in headless GNOME Shell: all five clips from `~/Videos/Footage` on the timeline, playing, then `win.close`. Before the fix it crashed every time (3 of 3), after it the process exited normally every time (3 of 3).

## Known gaps

- Transparency is dropped; frames are shown opaque.
- Variable-frame-rate sources (GNOME screencasts) map frame indices approximately.
- Uncached backward jumps in long-GOP footage still take 20–100 ms (1 s when a 4K decoder first opens and seeks).
- FFmpeg's own log lines go to stderr instead of the tracing log.
- GoPro MAX `.360` files hold two video streams (the halves of the sphere); Nox shows the first only.
- Camera RAW video other than ProRes RAW (Blackmagic RAW, RED, ARRIRAW, Canon Cinema RAW) needs the vendors' SDKs; FFmpeg doesn't read it.
- Without a Vulkan device, the CPU fallback renders only the top layer of a frame: it doesn't do dissolves, dips, grades or transforms. HDR exports refuse to run. Engine tests that need GPU compositing skip without a GPU, unless `NOX_REQUIRE_GPU` is set (Linux CI sets it, so lavapipe has to work there). GitHub's macOS runners are VMs without Metal, so MoltenVK has no device there, and macOS GPU paths can only be checked on a real Mac.
- `scrubbing_previews_and_interrupts` (nox-engine) fails about one run in seven. It requests frame 150 and then frame 3 at once, and sometimes 150 is still delivered exactly instead of being abandoned. The interruption isn't checked somewhere on the way to the first decoded frame.
- Dropping folders onto the media pool isn't supported.
- Not built or tested on Windows or macOS yet. The planned Vulkan→OpenGL interop path (for Windows) doesn't exist yet.
- Thumbnails (`Converter::convert`) stay display-referred. They tone-map HDR but don't convert the primaries of wide-gamut SDR sources, so a BT.2020 SDR clip's thumbnail is more saturated than the viewer shows it.
- Synchronization validation can't see shader accesses through descriptors (see "Vulkan validation"), so compute barriers are only reviewed by hand.

Translations:
- German is the only catalog, written as a first example; it hasn't been reviewed by a native speaker.
- There is no install step yet; an installed Nox finds catalogs under `../share/locale` next to `bin/nox` or via `NOX_LOCALEDIR`.
- GTK's own strings (file dialogs, entry menus) come from GTK's catalogs, so they are only translated if the system has them.

M5 limitations:
- No custom presets, render queue, or advanced codec settings (bitrate modes, GOP, B-frames). HDR exports use NVENC or x265 rather than the zero-copy Vulkan Video encoder (decision 27).
- AV1: the RTX 3060 has no AV1 encoder and this FFmpeg build no software one, so it isn't offered.
- NVENC gets frames from memory (it can't read Vulkan images), so it only runs when Vulkan Video encoding isn't available.

M4 limitations:
- HDR tone mapping follows BT.2408 (reference white about SDR white). mpv (libplacebo) maps HDR about a stop darker in the mid-tones; the user reviewed both and kept Nox's look.
- Not yet verified by anyone: audio on real speakers (A/V sync and the latency correction on PipeWire), and pacing on high-refresh monitors (the virtual monitor's frame clock is irregular at 144 Hz).
- Audio was only tested on ALSA's null device, not heard on real speakers; output latency compensation is unverified on PipeWire/PulseAudio.
- Only `f32` output devices are supported (others play without audio). There is no audio while scrubbing, no reverse playback (J), and no audio meters or waveforms.
- Only audio tracks are heard. Audio inside nested compositions or on video tracks is ignored.
- Downscaling to the viewer is bilinear, so 4K footage in a small viewer aliases (mipmaps would fix it).
- Transition types from other tools (other than `SMPTE_Dissolve`) play as dissolves.
- Frames are cached at viewer size; resizing the viewer drops the cache.

M3 limitations:
- Time-warp effects from imported files are respected when resolving frames, but splits and trims ignore them.
- Markers are kept but not shown. Nested compositions show as one block and can't be edited inside.
- Sync lock is off by default.
- Ripple-trim and roll previews show only the edited clip or cut, not the clips that will move.
- Mark and track-state edits replace the engine's timeline like any edit, so they drop the frame cache.
- Offline files are checked when the project changes, so files that appear later show as offline until the next edit.
- Image sequences (OTIO's `ImageSequenceReference`, e.g. EXR or TIFF frames) are kept in the project but not played; their clips show black. Several of the OTIO open-content examples (ASC StEM2, Sparks, Sol Levante 16-bit) use them.
- Alpha is read from packed RGBA sources only, not from planar formats with alpha (ProRes 4444, VP9 with alpha). Files can't be dropped straight onto the timeline, only via the media pool.
- Dragging an imported clip whose edges aren't on the sequence's frame grid snaps the moved clip to the grid.
- `Marker.3` colors are reduced to the nearest named color, since Nox writes `Marker.2`.

## Roadmap

### M3 – Project model and timeline (done)
All planned items are in, except that the viewer shows the topmost clip rather than a composite; compositing moved to M4. Added beyond the plan: linked video/audio clips, split at playhead, adding tracks, and pass-through of unknown OTIO fields.

### M4 – Playback engine (done)
Everything planned is in: render-ahead into a frame cache, the GPU compositor with opacity, blend modes and dissolves, audio mixing and output with audio as the clock, and preview at viewer size. Added beyond the plan: a sequence resolution setting, a clip context menu, creating and deleting dissolves, and the clock drift guard.

### M5 – Export (done)
Rendering through the compositor, hardware encoding (Vulkan Video with zero copy, NVENC) with software fallback, muxing, the export dialog and `nox-cli render` are all in. VAAPI was dropped: Vulkan Video covers hardware encoding on AMD and Intel too. Added beyond the plan: the Project Settings dialog (asked for in review, modeled on Resolve and Shotcut) and the image scaling modes.

### M6 – Color management
*Postponed (2026-10-14):* the open items below wait until after M7 and M8, which make Nox usable for everyday projects first.
- ✅ Viewer output as `AB4H` with an explicit `GdkColorState`.
- ✅ HDR export: HDR10 (PQ) and HLG, 10-bit BT.2020, HEVC Main 10 or ProRes (decision 27). The viewer stays SDR.
- ✅ ACEScg working space (linear; RGBA32F layers and accumulator). ACEScct as a choice for blending is open.
- Input transforms: ✅ from color tags (transfer functions, primaries); camera log curves still to do.
- CDL and `.cube`/CLF LUT nodes.
- A reader for a subset of `.ocio` configs.
- Display/view transforms, with ACES 2 output transforms as baked LUTs at first.
- ✅ Edit and Color pages (decision 21): the header-bar switcher, the shared viewer moving between pages, the Edit page's inspector, and the Color page's layout with its clip strip. Consecutive edits of one property merge into one undo step (`Document::edit_merging`).
- ✅ Primary color wheels (lift, gamma, gain, offset) on the Color page (decision 25). Still open: power windows and qualifiers (secondaries), curves, and a node graph for several corrections per clip.
- ✅ Scopes, on the Color page: waveform, RGB parade, vectorscope (75% bar targets, skin-tone line) and histogram (decision 24). Still open:
  - measuring the working space, with HDR headroom above 100%
  - a choice of which scopes to show
  - benchmarks on AMD, Intel and Apple GPUs
- Tests against reference values produced by OCIO.

### M7 – Editing depth
- ✅ Multi-selection: Ctrl/Shift-click and rubber band on the timeline; delete, ripple delete, move and grade/property edits act on all selected clips (`Timeline::move_items`, `remove_items`, `ripple_remove_items`). Tested with real pointer and keyboard input (`tools/gnome-input.py`).
- ✅ Markers: shown on the ruler and on clips (the model keeps OTIO markers); add with M, jump between them, drag them, name, color and note in the inspector. Adding markers to clips is open.
- ✅ Slip (move a clip's media under it) and slide (move a clip between its neighbours, trimming them): Alt+drag and Alt+Shift+drag (`Timeline::slip`, `slide`), with linked clips. Tested with real input. There are no tool modes (Resolve's trim mode, Premiere's slip and slide tools); modifiers stand in for them.
- ✅ Ripple insert from the UI: Ctrl+drop, F9/comma (insert) and period (overwrite) of the pool's selection at the playhead. Sync lock per track (header toggle or menu), followed by inserts, ripple delete and ripple trim (`Timeline::insert_clips`, `sync_shift`). Tested with real input.
- ✅ Relinking offline media: offline clips are red, opening a project with missing media offers a folder search, and clips can be relinked one file at a time from the clip menu (`Timeline::relink`, `relink::find_in_folder`). The folder search and undo were tested in the app. The file chooser for single clips wasn't, because the portal's dialog can't be driven headless.
- ✅ Audio waveforms on audio clips: the peak level per pixel column, scaled by the clip's volume. They're computed in the background at 100 peaks per second (`nox_media::waveform`) and cached under `~/.cache/nox/waveforms`, keyed by path, size and modification time. Tested in the app with real trims and volume changes.
- ✅ Track headers as real widgets: name, mute/hide, solo (audio, `metadata.nox.solo`), lock and sync lock. They're reachable by Tab, toggle with Space, and Shift+F10 opens the track menu. Tested with real pointer and keyboard input. Renaming is still done through the menu, not in place.

### M8 – Motion and keyframes
- ✅ Per-clip fades: fade-in and fade-out lengths in seconds (`metadata.nox.fade_in`, `fade_out`), so they keep their length when the frame rate changes. Handles at the clip's top corners appear on hover or selection, as in Resolve, and each drag is one undo step. The inspector also has fields for them. Video fades linearly to the track below, and audio fades with equal-power curves (a crossfade, if longer, wins). Tested with real drags.
- ✅ Transform per clip in the inspector: position (in timeline pixels), zoom, rotation (clockwise) and crop per edge, with a reset button (`metadata.nox.transform`, position stored as a fraction of the frame). In the compositor (`nox_gpu::Transform`), a transformed layer is blended over the pixels it covers. Each pixel is mapped back into the layer and sampled bilinearly by hand (linear filtering of RGBA32F is optional in Vulkan), with a one-pixel soft edge. Layers scaled up are converted at up to 4× their placed size, so they stay sharp. The parameters fill exactly the 128 bytes of push constants every device offers. Tested on the GPU (move, scale, rotate, crop, off-screen), through the engine, under the validation layer, and in the app with typed values.
  - On-screen controls in the viewer (`transform_overlay.rs`), as in Resolve: the selected video clip's outline (from `ScalingMode::place` and `Transform::corners`, the compositor's geometry). Drag inside it to move, a corner to zoom around the center, and the knob above it to rotate (Shift snaps to 15°). Each drag is one undo step, keyframed values are set at the playhead, and the controls are hidden on the Color page. Tested with real drags.
- ✅ Keyframes for opacity, volume and every transform value (`nox_project::keyframes`, `metadata.nox.keyframes`), with linear or eased (smoothstep) interpolation:
  - Keyframe times are in the clip's media time, as in Resolve, so trims and splits keep them on the same picture.
  - In the inspector, each property has a diamond toggle that adds or removes a keyframe at the playhead. Once a property has keyframes, changing its value at the playhead adds or updates one there.
  - The inspector shows values at the playhead. Its Keyframes row jumps to the previous or next keyframe and sets the easing.
  - Clips show their keyframes as diamonds along the bottom edge.
  - Volume keyframes reach the mixer in timeline time.
  - Dragging a diamond moves the keyframes there (of every property), in whole frames and between its neighbors, so none are lost (`Item::move_keyframes`). Diamonds and the inspector's jumps follow clip speed.
  - Grade keyframes: on the Color page the wheels' title has a keyframe toggle and previous/next buttons. A keyframe holds the whole grade, as Resolve's dynamic keyframes do (`Item::grade_keyframes`, `grade_at`), each value interpolated, and the engine grades at each frame's media time.
  - Still open: a curve editor.
- ✅ Transitions: Ctrl+T (or the toolbar) adds a one-second dissolve at the selected clip's edge nearest the playhead. The inspector changes its type and length (as long as the clips' media beyond the cut allows; `Timeline::set_transition_length`). Types: cross dissolve (OTIO's `SMPTE_Dissolve`), wipes from each side and dips to black or white (`Custom_Transition` with `metadata.nox.transition`). A dip draws a solid color layer (a tiny frame scaled by the compositor) over the outgoing picture, then the incoming picture fades in over the color, so the dip goes through the color whatever is on the tracks below. Wipes reveal the incoming clip by cropping it in the compositor, so they get the transform's soft edge. Audio crossfades under every type. Dragging a transition's edge on the timeline changes its length, centered on the cut (one undo step per drag). Still open: other dip colors (a color picker), and soft or angled wipes.
- ✅ Speed changes and freeze frames, as OTIO time warps (`LinearTimeWarp`, `FreezeFrame`):
  - Speed is a percentage in the inspector (`Timeline::set_speed`). The clip and its linked clips keep showing the same media, so their length changes. With "Move what follows" (on by default, Resolve's Ripple Sequence) the clips after them move, along with sync-locked tracks; otherwise a gap is left or the following clips are covered.
  - Audio plays at the new speed by varispeed resampling, so the pitch changes, as on tape.
  - Freeze Frame holds the frame under the playhead for the whole video clip (`set_freeze_frame`), and the linked audio plays on.
  - Clips show "2×" or "Freeze" in their label, and waveforms follow the speed.
  - Still open: pitch-preserving time stretch, speed ramps (keyframed speed), and a reverse.

### Later
- Grading II: secondaries (qualifiers, power windows), curves, a node graph for several corrections per clip.
- Proxies for heavy footage.
- A native ACES 2 port, and real OCIO as an optional feature.
- An OpenFX host and CLAP audio plugins.
- Vulkan→OpenGL interop (Windows) and IOSurface (macOS) presentation paths.
- `cargo xtask vcpkg-setup`, and CI on Linux, Windows and macOS.
