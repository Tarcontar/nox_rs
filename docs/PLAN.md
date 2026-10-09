# Nox — state and plan

Nox is a video editor written in Rust: a GTK4 UI, FFmpeg for media I/O, and Vulkan for decoding, image processing and display, with hardware acceleration wherever possible. The UI follows common editors (DaVinci Resolve, Blender VSE): a media pool, a viewer and a timeline, with an inspector later.

*Last updated: 2026-10-09 (end of M5; UI templates and translations).*

## Status at a glance

| Milestone | Status |
|---|---|
| M0 – Workspace skeleton, GTK window | ✅ done |
| M1 – Media import, probing, thumbnails | ✅ done |
| M2 – Vulkan decode to viewer, zero copy | ✅ done |
| M3 – Project model (OTIO) and timeline editing | ✅ done |
| M4 – Playback engine, audio, A/V sync | ✅ done |
| M5 – Export | ✅ done |
| M6 – Color management | ⏭ next |

What works today:
- **Import:** drag files onto the media pool (or use the import button, or `nox file…`). Each file is probed in the background and gets a thumbnail, its duration as timecode, and a tooltip with stream and color details.
- **Timeline editing:** drag media from the pool onto a track, or double-click it to append. Files with audio become a linked video and audio clip. Drag clips to move them, also to another track of the same kind; drag their edges to trim. Clips snap to edit points and the playhead (N toggles snapping). Ctrl+B splits at the playhead, Delete removes the selected clip. Every edit can be undone (Ctrl+Z, Ctrl+Shift+Z).
- **Projects:** `.otio` files (OpenTimelineIO) open and save, from the header bar or with `nox edit.otio`. Opening a project imports its media into the pool. Unsaved changes are flagged in the title, and closing asks first.
- **Sequence settings** (main menu): frame rate (23.976–60), drop-frame timecode for 29.97/59.94, adding video and audio tracks.
- **Viewer:** shows the whole sequence at the playhead, composited on the GPU: every enabled video track, with per-clip opacity and blend mode (normal, add, multiply, screen), dissolves, and sources fitted to the sequence (letterboxed or pillarboxed). It renders at the viewer's size. Scrub by dragging the ruler or empty track space, step frames with ←/→, and jump with Home/End.
- **Playback:** Space plays and pauses, L plays and K pauses. Audio tracks are mixed (clip gain, equal-power crossfades under dissolves) and played on the default output device, which also serves as the clock. Frames are rendered ahead into a cache, and frames that can't be ready in time are skipped. Scrubbing back over cached frames is instant.
- **Clip menu** (right-click): opacity, blend mode, gain, add dissolve (also Ctrl+T, at the selected clip's edge nearest the playhead), split, delete. Dissolves can be selected and deleted.
- **Project settings** (Shift+9, Resolve-style: categories in a sidebar, Cancel/Save, one undoable edit): project name; timeline resolution (presets or custom), frame rate and drop-frame timecode; image scaling for mismatched sources (scale to fit, scale with crop, center crop 1:1, stretch). The first clip in an empty sequence sets frame rate and resolution (like Shotcut's "Automatic" video mode).
- **Export** (Ctrl+E): presets (H.264 master, YouTube/web, HEVC, ProRes 422 HQ, review copy) or custom format, quality, hardware encoding and audio; progress, cancel, Open Folder. With hardware encoding the compositor writes straight into the Vulkan Video encoder's images (zero copy): 1080p exports of the test footage run at about 190 fps, against 57 fps with x264. `nox-cli render project.otio out.mp4` does the same headless.
- **Translations:** every user-visible string goes through gettext; a German catalog (`po/de.po`) is the first. Run with `LANGUAGE=de` to see it.
- **Frame path on Linux/NVIDIA:** frames never leave the GPU between decoder and screen. Vulkan Video decodes, a compute shader converts to RGB directly on FFmpeg's images, and the result goes to GTK as a DMA-BUF texture.

## Architecture

### Crates

```
crates/
  nox-color    color science: H.273 color tags, Y'CbCr→R'G'B' matrices (pure Rust)
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

Dependency direction: `gui → {engine, otio}`, `engine → {media, gpu, project}`, `otio → project`, `media → {gpu, color, project}`, and `gpu → color`. `nox-color`, `nox-project` and `nox-otio` have no heavy dependencies. Nothing below `nox-gui` depends on GTK or GLib, so the core can be tested headless and used by the CLI.

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
9. **Edits overwrite, OTIO-style.** Tracks are sequences with explicit gaps. Placing an item splits whatever is under it at the edges and replaces it. Trimming in leaves a gap, and trimming out may only consume gaps and available media. Transitions touching an edit are dropped. Tracks are normalized after every edit (gaps merged, trailing gaps removed). Linked clips (`metadata.nox.link`) move, trim and delete together, and splitting gives each half its own link. Ripple insert exists in the model but not yet in the UI.
10. **Undo by snapshots.** `Document::edit` keeps the timeline from before each edit (up to 500 steps). Timelines are small, and this makes undo exact by construction with no inverse operations. Failed edits leave the timeline untouched, and edits that change nothing aren't recorded.
11. **Compositing in two steps.** Each layer is converted by the existing YUV→RGB shader into an RGBA8 image at its placed size (`Rect::fit` keeps the aspect ratio). `blend.comp` then blends the layers in order into an RGBA16F accumulator over black, and `output.comp` writes AB24 for DMA-BUF or readback. Blending works on display-referred R'G'B' (editors' usual "video" mode); M6 moves it to a linear working space. A dissolve resolves to both clips, with the incoming one's opacity scaled by progress, so it needs no separate code path. Per-clip settings live in `metadata.nox` (`opacity`, `blend`, `gain`).
    *Changed during M4:* the M3 viewer requested single source frames; the engine now renders whole sequence frames.
12. **The engine renders sequence frames into a cache.** A worker thread owns the GPU and a pool of open decoders (up to 8, two per file). A decoder is reused by seeking rather than reopened; reopening is expensive, e.g. FFV1 sets up FFmpeg's Vulkan compute decoder. A second decoder per file is opened only when the file is on two layers of one frame. Rendered frames are cached by timeline version and output size, up to `NOX_CACHE_MB` (768 MB by default), least recently used first. While paused, the newest request wins. While playing, the worker renders up to 0.5 s ahead of the clock. If it falls behind, it aims ahead by its measured render time, so finished frames are still shown and the ones in between are skipped.
    Scrubbing in long-GOP footage (keyframes 4–8 s apart, so stepping back can mean decoding hundreds of 4K frames) works in three ways:
    - While a requested frame renders, `request` returns the closest cached frame within half a second to show meanwhile.
    - If nothing is cached nearby and the frame is more than 8 decoded frames from where a decoder can start, a preview using the keyframe before it is rendered first.
    - After 50 ms, a newer request interrupts the render (`VideoDecoder::frame_at_until`). The decoder keeps its progress. Short renders always finish, otherwise a fast-moving mouse would never see a frame.

    Decoders decode on rather than seek whenever the target is in the GOP they are already in (from the demuxer's keyframe index), not only within 2 s.
    Stills (photos) are decoded once, and every position maps to their single frame (image files can't seek, so treating them as 25 fps video reopened and re-decoded a 30 MP JPEG on every frame after 2 s). The compositor keeps converted stills on the GPU (8 entries, LRU, keyed by file and placed size), so a photo isn't uploaded and converted on every frame. Rendering ahead during playback only moves forward. When the lead shrinks after a slow frame, going back behind frames already rendered made decoders seek back to a keyframe, which was slow and made the lead grow again. That runaway made `noemi.otio` lag after its first photo.
13. **Audio is the clock.** For each playback, a `cpal` stream on the default device (or `NOX_AUDIO_DEVICE`) pulls from a lock-free ring that a mixer thread keeps 250 ms full. The playback time is the frames the device has consumed, corrected by the output latency cpal reports. After an underrun the mixer skips the frames the device played as silence, so audio stays in step. If the audio clock drifts more than a second from wall time (ALSA's `null` device consumes samples as fast as it can), playback switches to wall time. Without a device it uses wall time from the start. Read raw, the audio clock moves in steps (per device callback) and its latency estimate jitters, which made 60 fps playback judder. So the playback clock is wall time plus an offset that slowly follows the audio clock (2% of the error per reading), snapping on errors over 50 ms. On every display refresh (a GTK tick callback), the GUI picks the frame for the moment that refresh will be shown, which is the frame clock's time plus one refresh interval.
14. **HDR sources are tone-mapped for SDR display.** PQ (HDR10) and HLG frames are decoded to display light, converted from BT.2020/P3 to BT.709, mapped with the BT.2390 EETF on the largest component (content peak from MaxCLL or the mastering display, else 1000 nits, onto 203-nit reference white per BT.2408), and encoded with BT.1886 gamma. The same math runs in the YUV→RGB shader and in `nox_color::hdr` (the CPU path for thumbnails, and the reference in tests). Real HDR output is M6.
15. **Export writes into the encoder's own images.** For Vulkan Video encoders (`h264_vulkan`, `hevc_vulkan`) on the shared device, `MediaWriter` creates an NV12 frames pool with storage usage (mutable and extended-usage images, so the compositor can view each plane). `output_yuv.comp` writes BT.709 limited-range Y'CbCr (left-sited 4:2:0 chroma, the matrix being the exact inverse of the decoding one) into a pool frame. It follows FFmpeg's protocol: lock, wait on the semaphore, signal the next value, record GENERAL layout and no access (the semaphore makes the writes available to the encode queue). The frame then goes to the encoder: no readback, no CPU conversion. Fallbacks, in order: NVENC (frames read back, converted by swscale), then software (x264, x265). ProRes is always software, with 24-bit PCM audio in MOV. Exports render at sequence resolution with their own renderer, mix audio frame by frame, tag BT.709, put keyframes every 2 s, and use `+faststart`; a cancelled export deletes its file.
16. **Dialogs follow other editors.** Project settings are laid out like Resolve's (sidebar of categories). Export leads with presets like Resolve's Deliver page and Shotcut's export panel, with Shotcut's single "Use hardware encoder" switch, and shows what will be rendered (as Kdenlive does).
17. **Layout in `.ui` templates, behavior in Rust.** The window, viewer, media pool and its cells, both dialogs and the menus are GTK composite templates in plain `.ui` XML (`crates/nox-gui/resources/ui/`; no Blueprint, so no extra compiler). They are compiled into a GResource by `build.rs` (`glib-build-tools`; `.ui` files compressed and with blanks stripped by `xml-stripblanks`, which needs `xmllint`; without it the build warns and embeds them unstripped) and embedded in the binary under `/io/github/nox/Nox/`, with `style.css`. Each template has a Rust subclass (`NoxWindow`, `NoxViewer`, `NoxMediaPool`, `NoxMediaCell`, `NoxProjectSettings`, `NoxExportDialog`) that binds its children by id (`TemplateChild`) and its signal handlers with `#[template_callbacks]`. Things computed at run time (dropdown models from the preset tables, the clip labels) stay in Rust. The timeline is drawn in code (`snapshot`), so it has no template; the window template places it, and `TimelineView::set_project` attaches the project.
18. **Translations with gettext.** The domain is `nox`; `i18n::init` binds it at startup. Rust uses `gettext`, `pgettext` (context, e.g. "blend mode"), `ngettext` (plurals) and `n_` (marks entries of constant tables, translated where shown). Placeholders are named, `{name}`, and filled by `i18n::fill`, so translators can reorder them; `format!` around translated text is avoided. `.ui` files mark text `translatable="yes"` with `<interface domain="nox">`. `build.rs` compiles `po/<lang>.po` for every language in `po/LINGUAS` into `.mo` catalogs itself (a small writer, no gettext tools needed to build). The binary looks for catalogs in `NOX_LOCALEDIR`, then `../share/locale` next to an installed `bin/nox`, then the build directory. Format names (H.264, ProRes, OpenTimelineIO) and units stay untranslated.

### Display formats

GTK's Vulkan renderer on the development machine imports these DMA-BUF formats: `AB24`, `XB24`, `AR24`, `XR24` (8-bit RGB), `AB4H` (16-bit float RGBA), `AYUV`/`XYUV`/`AVUY`/`XVUY` (packed 8-bit YUV) and `R8`. Nox outputs `AB24` today. M6 switches to `AB4H` with an explicit `GdkColorState`, for 10-bit and HDR sources. Handing GTK the decoder's YUV directly isn't useful for an editor, since it would bypass compositing and color management.

## Building and running

Linux (system libraries):

```sh
sudo apt install libgtk-4-dev libavcodec-dev libavformat-dev libavutil-dev \
                 libavfilter-dev libavdevice-dev libswscale-dev libswresample-dev \
                 libasound2-dev glslang-tools libxml2-utils gettext
cargo run -- clip.mp4 photo.jpg      # starts Nox and imports the files
cargo test --workspace               # needs the `ffmpeg` CLI for test clips
```

Windows/macOS: `vcpkg.json` lists GTK and FFmpeg. Point `PKG_CONFIG_PATH` at `vcpkg_installed/<triplet>/lib/pkgconfig` (the `cargo xtask` helper for this doesn't exist yet). macOS needs MoltenVK and has no Vulkan Video; decoding falls back to VideoToolbox or software.

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
| `NOX_VULKAN_DEBUG=1` | Vulkan validation layers (needs `vulkan-validationlayers`) |
| `NOX_PRESENT=readback` | turn off DMA-BUF presentation |
| `NOX_AUDIO_DEVICE` | output device by name substring; `none` plays without audio (wall clock). For tests, `Discard all samples` is ALSA's null device |
| `NOX_CACHE_MB` | frame cache size (default 768) |
| `NOX_LOCALEDIR` | where to find `<lang>/LC_MESSAGES/nox.mo` |
| `LANGUAGE` | UI language, e.g. `de` (with a UTF-8 `LANG`) |
| `NOX_LOG` for `nox-cli` | defaults to `warn`; `info` shows the encoder and speed |
| `NOX_TEST_ACTIONS=1` | adds `win.screenshot(path)`, which renders the window content to PNG (tests only) |
| `NOX_OTIO_PYTHON` | tests: a Python with `opentimelineio`, to compare `nox-otio` output with the reference implementation |

`--gapplication-app-id=…` runs a separate instance next to a running Nox. Window actions are exported over D-Bus (org.gtk.Actions), which tests use to script the app:
- transport: `seek(frame)`, `next-frame`, `previous-frame`, `go-to-start`, `go-to-end`
- editing: `undo`, `redo`, `split`, `delete-clip`, `add-video-track`, `add-audio-track`, `append-media(path)`
- files: `open-path(path)`, `save-path(path)`
- settings: `frame-rate` (state such as `'30000/1001'`), `drop-frame`, `snapping`

For GUI tests, run Nox on Xvfb (`GDK_BACKEND=x11 DISPLAY=:99`) so no windows appear on the desktop. GNOME doesn't allow screenshots over D-Bus, which is what `win.screenshot` is for. Mouse and keyboard input can be synthesized with XTEST. Two catches: drag-and-drop needs slow drags for XDND's round trips, and the vertical paned uses a wide handle because a thin handle's invisible grab area covered the top half of the ruler.

## Verification so far

- **93 automated tests:**
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
  - the engine end to end: tracks with opacity, a dissolve, the cache, mixer placement and gain, four layers from one file, playback on the wall clock, and on ALSA's null device (the drift guard)
- **M5 tests:**
  - media writer round trips (decode the file and compare frames, colors, rate, tags, duration and audio level): x264, NVENC, HEVC, ProRes with PCM, and zero-copy Vulkan encoding from the compositor
  - export end to end on a timeline with a dissolve and audio, with Vulkan zero copy, x264 and ProRes; the scaling mode reaching the file; cancelling deleting the file
  - RGB→Y'CbCr matrix round trips, placement rects for all scaling modes
  - the zero-copy encode path is clean under the validation layers. FFmpeg's own Vulkan encoder leaks 6 image views at shutdown and logs encode-command errors; the plain `ffmpeg` CLI does the same.
- **M5 app checks** (Xvfb, real clicks): both dialogs open and lay out as intended; exporting 789 frames of the 1080p/4K test project through the dialog took 4.1 s with `h264_vulkan`, zero copy.
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

## Known gaps

- Transparency is dropped; frames are shown opaque.
- Variable-frame-rate sources (GNOME screencasts) map frame indices approximately.
- Uncached backward jumps in long-GOP footage still take 20–100 ms (1 s when a 4K decoder first opens and seeks).
- FFmpeg's own log lines go to stderr instead of the tracing log.
- Dropping folders onto the media pool isn't supported.
- Not built or tested on Windows or macOS yet. The planned Vulkan→OpenGL interop path (for Windows) doesn't exist yet.
- Vulkan synchronization hasn't been checked with validation layers; they aren't installed on the development machine.

Translations:
- German is the only catalog, written as a first example; it hasn't been reviewed by a native speaker.
- There is no install step yet; an installed Nox finds catalogs under `../share/locale` next to `bin/nox` or via `NOX_LOCALEDIR`.
- GTK's own strings (file dialogs, entry menus) come from GTK's catalogs, so they are only translated if the system has them.

M5 limitations:
- Only the whole timeline can be exported from the dialog. Timeline In/Out marks (and with them render ranges, as in Kdenlive) don't exist yet; `nox-cli render --frames` takes a range.
- No custom presets, render queue, or advanced codec settings (bitrate modes, GOP, B-frames, 10-bit or HDR output).
- AV1: the RTX 3060 has no AV1 encoder and this FFmpeg build no software one, so it isn't offered.
- NVENC gets frames from memory (it can't read Vulkan images), so it only runs when Vulkan Video encoding isn't available.

M4 limitations:
- HDR tone mapping follows BT.2408 (reference white about SDR white). mpv (libplacebo) maps HDR about a stop darker in the mid-tones; the user reviewed both and kept Nox's look.
- Not yet verified by anyone: audio on real speakers (A/V sync and the latency correction on PipeWire), and pacing on high-refresh monitors (the virtual monitor's frame clock is irregular at 144 Hz).
- Audio was only tested on ALSA's null device, not heard on real speakers; output latency compensation is unverified on PipeWire/PulseAudio.
- Only `f32` output devices are supported (others play without audio). There is no audio while scrubbing, no reverse playback (J), and no audio meters or waveforms.
- Only audio tracks are heard. Audio inside nested compositions or on video tracks is ignored.
- Downscaling to the viewer is bilinear, so 4K footage in a small viewer aliases (mipmaps would fix it). Layers are 8-bit before blending.
- Only dissolves are rendered; other transition types play as dissolves.
- Frames are cached at viewer size; resizing the viewer drops the cache.

M3 limitations:
- Time-warp effects from imported files are respected when resolving frames, but splits and trims ignore them.
- Markers are kept but not shown. Nested compositions show as one block and can't be edited inside.
- No ripple or roll edits in the UI (the model has `insert`), no track deletion, renaming, muting or locking, and no multi-selection.
- No relinking of offline media (they show black and are logged once). Files can't be dropped straight onto the timeline, only via the media pool.
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
- Viewer and export output as `AB4H` with an explicit `GdkColorState`.
- ACEScct/ACEScg working space.
- Input transforms from color tags and camera log curves.
- CDL and `.cube`/CLF LUT nodes.
- A reader for a subset of `.ocio` configs.
- Display/view transforms, with ACES 2 output transforms as baked LUTs at first.
- Primary color wheels in the inspector.
- Tests against reference values produced by OCIO.

### Later
- In/Out marks on the timeline, and export of that range.
- More editing: ripple and roll edits, multi-selection, track controls (delete, rename, mute, lock), creating transitions, showing markers, relinking offline media.
- Inspector with transforms, transitions and keyframes.
- Scopes (waveform, parade, vectorscope).
- Proxies and audio waveform display.
- A native ACES 2 port, and real OCIO as an optional feature.
- An OpenFX host and CLAP audio plugins.
- Vulkan→OpenGL interop (Windows) and IOSurface (macOS) presentation paths.
- `cargo xtask vcpkg-setup`, and CI on Linux, Windows and macOS.
