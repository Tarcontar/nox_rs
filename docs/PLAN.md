# Nox — state and plan

Nox is a video editor written in Rust: a GTK4 UI, FFmpeg for media I/O, and Vulkan for decoding, image processing and display, with hardware acceleration wherever possible. The UI follows common editors (DaVinci Resolve, Blender VSE): a media pool, a viewer and a timeline, with an inspector later.

*Last updated: 2026-10-09 (end of M2).*

## Status at a glance

| Milestone | Status |
|---|---|
| M0 – Workspace skeleton, GTK window | ✅ done |
| M1 – Media import, probing, thumbnails | ✅ done |
| M2 – Vulkan decode to viewer, zero copy | ✅ done |
| M3 – Project model (OTIO) and timeline editing | ⏭ next |
| M4 – Playback engine, audio, A/V sync | planned |
| M5 – Export | planned |
| M6 – Color management | planned |

What works today:
- **Import:** drag files onto the media pool (or use the import button, or `nox file…`). Each file is probed in the background and gets a thumbnail, its duration as timecode, and a tooltip with stream and color details.
- **Viewer:** shows the selected clip at the playhead. Scrub by dragging the timeline, step frames with ←/→, and jump to the start with Home.
- **Frame path on Linux/NVIDIA:** frames never leave the GPU between decoder and screen. Vulkan Video decodes, a compute shader converts to RGB directly on FFmpeg's images, and the result goes to GTK as a DMA-BUF texture.

## Architecture

### Crates

```
crates/
  nox-color    color science: H.273 color tags, Y'CbCr→R'G'B' matrices (pure Rust)
  nox-project  project/timeline model; rational time and timecode (pure Rust)
  nox-gpu      Vulkan: shared device, YUV→RGB compute converter, DMA-BUF export
  nox-media    FFmpeg (ffmpeg-next): probe, thumbnails, frame-accurate decoding,
               Vulkan device creation, zero-copy access to decoded frames
  nox-engine   background preview worker (later: playback, compositing, export)
  nox-gui      GTK4 app, binary `nox`
  nox-cli      headless tools (`nox-cli probe`, `nox-cli version`)
```

Dependency direction: `gui → engine → {media, gpu, project}`, `media → {gpu, color, project}`, and `gpu → color`. `nox-color` and `nox-project` have no heavy dependencies. Nothing below `nox-gui` depends on GTK or GLib, so the core can be tested headless and used by the CLI.

### Key decisions

1. **One shared Vulkan device, created by FFmpeg.** FFmpeg creates the device (`av_hwdevice_ctx_create`), so it enables exactly the video queues and extensions its decoders need, including DMA-BUF export. `nox-gpu` loads the same device into `ash` (`GpuDevice::from_external`) and submits work through FFmpeg's queue locks. `NOX_GPU` selects the device.
   *Changed during M2:* the original plan was for Nox to create the device and hand it to FFmpeg.
2. **Zero copy from decoder to screen.** Decoded `AVVkFrame` images are read in place through per-plane image views, following FFmpeg's sync protocol (frame lock, timeline semaphores, recorded layouts). The converted frame is exported as DMA-BUF and wrapped in a `GdkDmabufTexture`. Its release callback returns the image to a pool. The fallbacks, each logged with its reason:
   - download + upload, if the decoder's images can't be viewed per plane
   - GPU readback to a `GdkMemoryTexture`, if GTK can't import DMA-BUF (`NOX_PRESENT=readback` forces this)
   - CPU conversion with swscale, if there is no Vulkan device
3. **One conversion shader for every layout.** A single compute shader handles planar, semi-planar and packed layouts. A 3×4 matrix built by `nox-color` folds in the H.273 coefficients, range, bit depth and sample packing (e.g. P010). The shader also handles chroma siting (untagged 4:2:0 is treated as left-sited, the H.264 convention), non-square pixels and padded decode images.
4. **Rational time everywhere:** integer ticks plus a timebase, never `f64` seconds.
5. **OpenTimelineIO for timelines.** `.otio` JSON is read and written natively with `serde`. Nox-specific data goes under `metadata.nox`. The internal model mirrors OTIO's types but keeps integer time.
6. **Color management without linking OpenColorIO.** `nox-color` implements the underlying standards: H.273, ACES, ASC CDL, `.cube`/CLF LUTs, camera log curves, and a subset of the `.ocio` config format. Where useful we port OCIO's BSD-licensed GPU code. OCIO itself is used only in tests, to produce reference values.
7. **Shaders:** GLSL compiled to SPIR-V at build time by `glslc` or `glslangValidator` (from `PATH` or `$VULKAN_SDK/bin`).
8. **GUI objects are thin wrappers.** Model types are plain Rust; the GUI wraps them in small GObject subclasses (`MediaObject`) so they can live in GTK list models.

### Display formats

GTK's Vulkan renderer on the development machine imports these DMA-BUF formats: `AB24`, `XB24`, `AR24`, `XR24` (8-bit RGB), `AB4H` (16-bit float RGBA), `AYUV`/`XYUV`/`AVUY`/`XVUY` (packed 8-bit YUV) and `R8`. Nox outputs `AB24` today. M6 switches to `AB4H` with an explicit `GdkColorState`, for 10-bit and HDR sources. Handing GTK the decoder's YUV directly isn't useful for an editor, since it would bypass compositing and color management.

## Building and running

Linux (system libraries):

```sh
sudo apt install libgtk-4-dev libavcodec-dev libavformat-dev libavutil-dev \
                 libswscale-dev libswresample-dev glslang-tools
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

`--gapplication-app-id=…` runs a separate instance next to a running Nox. Window actions (`win.seek`, `win.next-frame`, …) are exported over D-Bus, which tests use to script the app.

## Verification so far

- **33 automated tests:**
  - time and timecode math, Y'CbCr matrices
  - probing (video, audio-only, stills, invalid files) and thumbnails for JPEG, PNG, WebP, TIFF and BMP
  - frame-accurate random access with B-frames
  - GPU conversion against swscale for 9 pixel formats, plus a chroma-siting test
  - Vulkan Video decoding bit-exact with software decoding
  - zero-copy input against the download path
  - DMA-BUF content against readback
  - a 200-step random-access stress test with zero-copy DMA-BUF output
- **Real media:** all 2,084 files in `~/Videos` and `~/Pictures` (H.264 and VP8 screencasts, large JPEGs, PNGs) pass probe, thumbnail, decode and GPU conversion, and match the CPU path.
- **App checks:** run through D-Bus actions with screenshots. The DMA-BUF and readback paths produce pixel-identical windows.

## Known gaps

- Transparency is dropped; frames are shown opaque.
- Variable-frame-rate sources (GNOME screencasts) map frame indices approximately.
- Preview converts at full resolution rather than viewer size. Backward jumps in long-GOP footage take 20–100 ms (no frame cache yet).
- FFmpeg's own log lines go to stderr instead of the tracing log.
- Dropping folders onto the media pool isn't supported.
- Not built or tested on Windows or macOS yet. The planned Vulkan→OpenGL interop path (for Windows) doesn't exist yet.
- Vulkan synchronization hasn't been checked with validation layers; they aren't installed on the development machine.
- Play and Go-to-End are disabled until M4.

## Roadmap

### M3 – Project model and timeline (next)
- `nox-project` types shaped like OTIO (Timeline, Stack, Track, Clip, Gap, Transition, ExternalReference), with an undo/redo command stack.
- Save and load `.otio`, round-trip tested against upstream sample files.
- Timeline widget: drag clips from the media pool onto tracks, move and trim, snapping, a sequence frame-rate setting.
- The viewer shows the composited sequence at the playhead instead of the selected clip.
- Drop-frame timecode (29.97/59.94).

### M4 – Playback engine
- Decode-ahead per clip and a frame cache (also speeds up backward scrubbing).
- GPU compositor for multiple tracks with opacity and blend modes.
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
- Inspector with transforms, transitions and keyframes.
- Scopes (waveform, parade, vectorscope).
- Proxies and audio waveform display.
- A native ACES 2 port, and real OCIO as an optional feature.
- An OpenFX host and CLAP audio plugins.
- Vulkan→OpenGL interop (Windows) and IOSurface (macOS) presentation paths.
- `cargo xtask vcpkg-setup`, and CI on Linux, Windows and macOS.
