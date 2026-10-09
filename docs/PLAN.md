# Nox — state and plan

Nox is a video editor written in Rust: a GTK4 UI, FFmpeg for media I/O, and Vulkan for decoding, image processing and display, with hardware acceleration wherever possible. The UI follows common editors (DaVinci Resolve, Blender VSE): a media pool, a viewer and a timeline, with an inspector later.

*Last updated: 2026-10-09 (end of M3).*

## Status at a glance

| Milestone | Status |
|---|---|
| M0 – Workspace skeleton, GTK window | ✅ done |
| M1 – Media import, probing, thumbnails | ✅ done |
| M2 – Vulkan decode to viewer, zero copy | ✅ done |
| M3 – Project model (OTIO) and timeline editing | ✅ done |
| M4 – Playback engine, audio, A/V sync | ⏭ next |
| M5 – Export | planned |
| M6 – Color management | planned |

What works today:
- **Import:** drag files onto the media pool (or use the import button, or `nox file…`). Each file is probed in the background and gets a thumbnail, its duration as timecode, and a tooltip with stream and color details.
- **Timeline editing:** drag media from the pool onto a track, or double-click it to append. Files with audio become a linked video and audio clip. Drag clips to move them, also to another track of the same kind; drag their edges to trim. Clips snap to edit points and the playhead (N toggles snapping). Ctrl+B splits at the playhead, Delete removes the selected clip. Every edit can be undone (Ctrl+Z, Ctrl+Shift+Z).
- **Projects:** `.otio` files (OpenTimelineIO) open and save, from the header bar or with `nox edit.otio`. Opening a project imports its media into the pool. Unsaved changes are flagged in the title, and closing asks first.
- **Sequence settings** (main menu): frame rate (23.976–60), drop-frame timecode for 29.97/59.94, adding video and audio tracks.
- **Viewer:** shows the topmost video clip at the playhead. Scrub by dragging the ruler or empty track space, step frames with ←/→, and jump with Home/End.
- **Frame path on Linux/NVIDIA:** frames never leave the GPU between decoder and screen. Vulkan Video decodes, a compute shader converts to RGB directly on FFmpeg's images, and the result goes to GTK as a DMA-BUF texture.

## Architecture

### Crates

```
crates/
  nox-color    color science: H.273 color tags, Y'CbCr→R'G'B' matrices (pure Rust)
  nox-project  timeline model shaped like OTIO, edit operations, undo; rational
               time, drop-frame timecode (pure Rust)
  nox-otio     .otio reading and writing (pure Rust, no OTIO library)
  nox-gpu      Vulkan: shared device, YUV→RGB compute converter, DMA-BUF export
  nox-media    FFmpeg (ffmpeg-next): probe, thumbnails, frame-accurate decoding,
               Vulkan device creation, zero-copy access to decoded frames
  nox-engine   background preview worker (later: playback, compositing, export)
  nox-gui      GTK4 app, binary `nox`
  nox-cli      headless tools (`nox-cli probe`, `nox-cli version`)
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
11. **The viewer follows the sequence.** The topmost enabled video clip at the playhead is decoded. Frames are requested by source time, and the decoder picks the frame at its own rate. Requests carry ids, so late frames for an old playhead position are dropped. Compositing all layers is M4.

### Display formats

GTK's Vulkan renderer on the development machine imports these DMA-BUF formats: `AB24`, `XB24`, `AR24`, `XR24` (8-bit RGB), `AB4H` (16-bit float RGBA), `AYUV`/`XYUV`/`AVUY`/`XVUY` (packed 8-bit YUV) and `R8`. Nox outputs `AB24` today. M6 switches to `AB4H` with an explicit `GdkColorState`, for 10-bit and HDR sources. Handing GTK the decoder's YUV directly isn't useful for an editor, since it would bypass compositing and color management.

## Building and running

Linux (system libraries):

```sh
sudo apt install libgtk-4-dev libavcodec-dev libavformat-dev libavutil-dev \
                 libavfilter-dev libavdevice-dev libswscale-dev libswresample-dev \
                 glslang-tools
cargo run -- clip.mp4 photo.jpg      # starts Nox and imports the files
cargo test --workspace               # needs the `ffmpeg` CLI for test clips
```

Windows/macOS: `vcpkg.json` lists GTK and FFmpeg. Point `PKG_CONFIG_PATH` at `vcpkg_installed/<triplet>/lib/pkgconfig` (the `cargo xtask` helper for this doesn't exist yet). macOS needs MoltenVK and has no Vulkan Video; decoding falls back to VideoToolbox or software.

### Environment variables

| Variable | Effect |
|---|---|
| `NOX_LOG` | log filter, e.g. `info,nox_engine=trace` (trace shows per-frame times) |
| `NOX_GPU` | Vulkan device: index or name substring |
| `NOX_VULKAN_DEBUG=1` | Vulkan validation layers (needs `vulkan-validationlayers`) |
| `NOX_PRESENT=readback` | turn off DMA-BUF presentation |
| `NOX_TEST_ACTIONS=1` | adds `win.screenshot(path)`, which renders the window content to PNG (tests only) |
| `NOX_OTIO_PYTHON` | tests: a Python with `opentimelineio`, to compare `nox-otio` output with the reference implementation |

`--gapplication-app-id=…` runs a separate instance next to a running Nox. Window actions are exported over D-Bus (org.gtk.Actions), which tests use to script the app:
- transport: `seek(frame)`, `next-frame`, `previous-frame`, `go-to-start`, `go-to-end`
- editing: `undo`, `redo`, `split`, `delete-clip`, `add-video-track`, `add-audio-track`, `append-media(path)`
- files: `open-path(path)`, `save-path(path)`
- settings: `frame-rate` (state such as `'30000/1001'`), `drop-frame`, `snapping`

For GUI tests, run Nox on Xvfb (`GDK_BACKEND=x11 DISPLAY=:99`) so no windows appear on the desktop. GNOME doesn't allow screenshots over D-Bus, which is what `win.screenshot` is for. Mouse and keyboard input can be synthesized with XTEST. Two catches: drag-and-drop needs slow drags for XDND's round trips, and the vertical paned uses a wide handle because a thin handle's invisible grab area covered the top half of the ruler.

## Verification so far

- **78 automated tests:**
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
- Preview converts at full resolution rather than viewer size. Backward jumps in long-GOP footage take 20–100 ms (no frame cache yet).
- FFmpeg's own log lines go to stderr instead of the tracing log.
- Dropping folders onto the media pool isn't supported.
- Not built or tested on Windows or macOS yet. The planned Vulkan→OpenGL interop path (for Windows) doesn't exist yet.
- Vulkan synchronization hasn't been checked with validation layers; they aren't installed on the development machine.
- Play is disabled until M4.

M3 limitations:
- The viewer shows only the topmost clip; there is no compositing, and transitions are not rendered (M4).
- Time-warp effects from imported files are respected when resolving frames, but splits and trims ignore them.
- Transitions are shown but can't be created. Markers are kept but not shown. Nested compositions show as one block and can't be edited inside.
- No ripple or roll edits in the UI (the model has `insert`), no track deletion, renaming, muting or locking, and no multi-selection.
- No relinking of offline media (they show black and are logged once). Files can't be dropped straight onto the timeline, only via the media pool.
- Dragging an imported clip whose edges aren't on the sequence's frame grid snaps the moved clip to the grid.
- `Marker.3` colors are reduced to the nearest named color, since Nox writes `Marker.2`.

## Roadmap

### M3 – Project model and timeline (done)
All planned items are in, except that the viewer shows the topmost clip rather than a composite; compositing moved to M4. Added beyond the plan: linked video/audio clips, split at playhead, adding tracks, and pass-through of unknown OTIO fields.

### M4 – Playback engine (next)
- Decode-ahead per clip and a frame cache (also speeds up backward scrubbing).
- GPU compositor for multiple tracks with opacity and blend modes; render dissolves.
- Audio decoding, resampling and mixing, output through `cpal`, with audio as the master clock and A/V sync.
- Preview at viewer resolution.

### M5 – Export
- Render the timeline through the compositor, encode with FFmpeg (Vulkan, NVENC or VAAPI encoders, with software fallback) and mux.
- Export dialog, plus `nox-cli render` for headless rendering.

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
- More editing: ripple and roll edits, multi-selection, track controls (delete, rename, mute, lock), creating transitions, showing markers, relinking offline media.
- Inspector with transforms, transitions and keyframes.
- Scopes (waveform, parade, vectorscope).
- Proxies and audio waveform display.
- A native ACES 2 port, and real OCIO as an optional feature.
- An OpenFX host and CLAP audio plugins.
- Vulkan→OpenGL interop (Windows) and IOSurface (macOS) presentation paths.
- `cargo xtask vcpkg-setup`, and CI on Linux, Windows and macOS.
