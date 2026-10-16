# AI in Nox: own ONNX engine on Vulkan (nox-ai), first feature: background removal

**Status (2026-10-15): steps 0–4 done.** Background removal works in the viewer and in exports. Stills use BiRefNet lite (fp32) instead of RVM, which keeps every person in a photo (docs/PLAN.md). What's left is in "Later" and the risks below (faster kernels). fp16 models run in a 16-bit pipeline, at half the memory (docs/PLAN.md).

## Context
You want AI-powered processing (speech to text, background removal, …) built on ONNX, the open and portable model format. You want inference in Vulkan compute shaders, so frames never leave the GPU. Nox already decodes, composites and encodes on one ash device without copies. Everything AI-related goes in its own crate.

**Crates checked:**
- **Burn + burn-onnx:** compiles models into the binary at build time, and runs Vulkan through wgpu, not our ash device.
- **ort (ONNX Runtime):** uses its own GPU devices: WebGPU through Dawn, plus CUDA and DirectML. Every frame would make a CPU round trip.
- **onnx-vulkan-core 0.0.1 / onnx-vulkan-rs:** ash 0.38 and gpu-allocator 0.28 like Nox, GLSL/WGSL kernels, op fusion, and a prost-based `.onnx` loader. `VkContext::new()` always creates its own instance and device, so it can't use ours.

**Decision:** a new `nox-ai` crate with its own small ONNX engine on nox-gpu (Nox's device, allocator and queue lock), using onnx-vulkan-rs only as inspiration. The first milestone is per-clip background removal with Robust Video Matting (RVM). Models are downloaded on first use.

## Architecture

```
nox-gpu   ── new public `compute` module (kernels, buffers, recording) + a per-layer hook in Compositor
nox-ai    ── ONNX loader → graph → plan (shapes, constants, memory) → GLSL kernels → session
             models registry + downloader; matting (RVM) on top
nox-engine── Renderer: per-layer matting via the compositor hook, recurrent state per clip
nox-project── Effect "nox.remove_background" on Item.effects (OTIO round trip)
nox-gui   ── Inspector "Effects" section, download prompt/progress
```

### 1. nox-gpu: public compute API and layer hook
- Promote the internal helpers to a public `compute` module:
  - `create_pipeline` and `push` (`compositor.rs:1675`, `:1723`)
  - `image_barrier` (`converter.rs:1366`)
  - `create_buffer` and `destroy_buffer` (`converter.rs:1124`, `:1196`)
  - `GpuImage` and `GpuBuffer` (`converter.rs:286`)
- On top of those, a `Kernel` (SPIR-V, storage-buffer and image bindings, push constants) and a reusable `Recording` (command buffer and fence, submitted through `GpuDevice::submit`, so FFmpeg's queue lock is respected).
- `Compositor::add_layer_with(input, rect, style, hook: Option<&mut dyn LayerHook>)`:
  - It runs after `converter.convert_into` and before `layers.push` (`compositor.rs:807-818`; the same in `add_cached_layer` at `:717`).
  - `LayerHook::process(&mut self, gpu, image: &GpuImage, w, h)` can rewrite the layer's RGBA32F image, linear ACEScg with straight alpha.
  - A matte is written into alpha. `blend.comp:87` already multiplies by `src.a`, so blending needs no change.
- Barriers: the hook starts and ends with explicit memory barriers. Host fence waits order the submissions but don't make writes visible on the device.
- Check the layer image's layout after `convert_into` and keep it the same.
- Reuse nox-gpu's `build.rs` shader compilation (glslc or glslangValidator, with variants) by moving it into a small shared build helper, or by copying it into nox-ai's `build.rs`.

### 2. nox-ai: the engine
- **Loader** (`onnx.rs`):
  - Read `.onnx` files with `prost`. Vendor `onnx.proto` (Apache-2.0) and compile it with `protox` in `build.rs`, so no `protoc` is needed.
  - Initializers become f32 tensors. Int64 initializers are kept host-side for shape math.
- **Graph and planning** (`graph.rs`, `plan.rs`), for concrete input shapes:
  - Fold constants and shape subgraphs (Shape, Gather, Concat, Unsqueeze, Cast, and RVM's `downsample_ratio`) on the CPU.
  - Fold BatchNormalization into Conv. Reshape, Squeeze, Unsqueeze, Flatten and Identity become views.
  - Assign buffers by lifetime, reusing memory.
  - Record all dispatches into a command buffer that is recorded once and resubmitted per frame.
  - Plans are cached per input size (a few, LRU).
- **Kernels** (`shaders/*.comp`, fp32 first):
  - Conv: direct, 1×1 as GEMM, and depthwise.
  - MatMul and Gemm (tiled).
  - Elementwise binary with broadcast: Add, Sub, Mul, Div, Pow.
  - Unary: Relu, Sigmoid, Tanh, HardSigmoid, HardSwish, Clip, Exp, Sqrt, Erf, Neg.
  - Resize: nearest and bilinear, with ONNX coordinate modes.
  - Concat, Split, Slice, Transpose, Pad.
  - AveragePool, MaxPool, GlobalAveragePool, ReduceMean, Softmax.
  - The final list comes from step 0's op histogram. Unsupported ops are refused when the model loads, with their names.
- **`Session`:**
  - `Session::new(gpu: Arc<GpuDevice>, model)`, then `plan(input_shapes)`.
  - `run(&mut self, inputs: &[(name, BufferRef)], outputs) -> Result<()>`.
  - Inputs and outputs are GPU buffers, so nothing goes through the host.
- **Bridging shaders** (in nox-ai):
  - `layer_to_tensor.comp`: the RGBA32F ACEScg layer goes to sRGB/BT.709 [0,1], then into an NCHW f32 buffer at the model's size.
  - `matte_to_alpha.comp`: the `pha` tensor goes into layer alpha. It resamples bilinearly and multiplies, so the clip's own alpha is kept.
- **Models** (`models.rs`):
  - A registry of known models: id, pinned URL, SHA-256, size and license. The first entry is `rvm_mobilenetv3_fp32.onnx`: ~15 MB, GPL-3.0, which suits Nox's GPL-3.0-or-later.
  - `ensure(id, dir, progress, cancel)` downloads with `ureq` (rustls) to a temporary file, checks the hash, and renames the file into place.
  - The directory comes from the caller: the GUI passes `glib::user_data_dir()/nox/models`.
- **Matting** (`matting.rs`):
  - `Matting::new(gpu, model_path)`, then `apply(clip_key, frame_index, layer_image, w, h)`.
  - RVM's recurrent states r1–r4 stay on the GPU per clip.
  - They reset (zeros of the right shape) when the frame isn't the one after the last, which happens on seeks and scrubbing.
  - `downsample_ratio` is chosen so the short side the model sees is about 512 px.
  - For stills, the model runs 3 times on the same frame so the matte settles.

### 3. Project model
- An effect on the clip, in OTIO's own place for it, so it round-trips:
  - `Effect { effect_name: "nox.remove_background", kind: EffectKind::Generic, enabled, metadata: { model: "rvm_mobilenetv3" } }` on `Item.effects` (`timeline.rs:1709`).
- `Item::background_removal()` and `set_background_removal(Option<…>)` sit next to the existing `speed()` and `set_time_effects` (~700-720), with an undoable edit through `edit.rs`.

### 4. Engine
- `nox-engine` depends on `nox-ai`.
- `Renderer::layers()`: turn the `Layer` tuple (`render.rs:530`) into a struct with an `effects` field and a stable clip key (media path plus the clip's source start).
- `compose()` (`render.rs:351-416`) passes the `Matting` hook for layers that have the effect.
- Cached stills: add the effect to `still_key`.
- Exports get the same path for free (`render_to_gpu`).
- If the model is missing, the layer renders unmatted and Nox warns once.
- The renderer holds `Option<Matting>`, created lazily when the first clip with the effect renders.

### 5. GUI
- Inspector: a new "Effects" section for video clips with a "Remove background" switch.
- If the model isn't downloaded, a dialog asks first ("Download Robust Video Matting, 15 MB?"). It shows progress, runs the download in a background job (the pattern of `waveforms.rs`), and turns the effect on when done.
- Preferences: a "Models" row showing the folder and a "Delete downloaded models" button.
- Flatpak: add `--share=network` to `packaging/flatpak/io.github.feliwir.Nox.yml`.
- Translations: add the new strings to `po/nox.pot` and `po/de.po`.

## Steps
0. **Spike:** `cargo run -p nox-ai --example inspect -- model.onnx` prints the opset, inputs, outputs and an op histogram for RVM, which fixes the kernel list.
1. nox-gpu `compute` module and `LayerHook`. Refactor nothing else.
2. nox-ai loader, planner, kernels and `Session`, with per-op tests.
3. nox-ai models registry and downloader. Matting with RVM.
4. Project effect, engine wiring, GUI, Flatpak permission, docs (`docs/PLAN.md`: decision and roadmap) and translations.

## Verification
- **Per-op tests** (nox-ai): random tensors, GPU kernel against a naive CPU reference in the test code. They cover broadcasting and edge shapes.
- **Graph tests:** small ONNX models built in the tests through the prost types (no Python), run on the GPU and on a CPU reference interpreter.
- **RVM test:** runs when the model is cached or downloadable, and skips otherwise.
  - The whole graph runs on the GPU and on the CPU reference with the same random input, and the outputs must agree within tolerance.
  - Recurrent states carry over between frames and reset after a seek.
- **Engine test** (`nox-engine/tests/render.rs`): a clip with the effect renders with alpha below 1 somewhere and blends over the track below. A synthetic person isn't needed: the check is that the hook runs and alpha changes.
- **Gating:** GPU tests run under `have_gpu()` and `NOX_REQUIRE_GPU` (lavapipe in CI). Run once under `tools/vk-validate.sh` (Vulkan validation).
- **Headless GNOME session:** a real clip with a person, effect on, over a colored V1 clip. Screenshot the viewer, and measure fps during playback (`NOX_LOG=nox_engine=trace`).
- **Export:** an exported file shows the matted result.

## Risks
- Kernel performance: fp32 direct convolution is slow at large sizes. RVM at ~512 px is small. fp16 storage is done; cooperative-matrix (tensor-core) kernels come later.
- Whether FFmpeg's device enables shaderFloat16 or cooperative matrix decides what later optimizations can use. Step 0 logs the enabled features.
- RVM's dynamic-shape subgraphs need host folding. A model outside the supported subset is refused clearly at load time.

## Later features (roadmap, in docs/PLAN.md)
- **Speech to text:** Whisper, Moonshine or Parakeet as ONNX. A transcript gives ranged markers and an SRT export first. A subtitle track needs text rendering. Silero VAD can drive "remove silences".
- **Voice cleanup:** DeepFilterNet3 noise suppression as a clip audio effect, near real time.
- **Depth:** Depth Anything V2 for depth-of-field blur, fog and depth keys.
- **Quality:** Real-ESRGAN upscaling and denoising at export. RIFE frame interpolation for smooth slow motion, which works with the existing speed effect.
- **Faces:** YuNet or SCRFD to blur faces for privacy, and auto-reframing to vertical by tracking the subject.
- **Editing aids:** scene-cut detection (TransNetV2) to split imported footage, and prompted masks (SAM 2) for the planned secondaries and power windows.
