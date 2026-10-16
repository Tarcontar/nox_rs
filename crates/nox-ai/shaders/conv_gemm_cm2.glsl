// conv_gemm.comp on NVIDIA's cooperative matrices 2 (compiled with CM2,
// for F16 plans), for convolutions with stride 1: weights × input as
// workgroup matrices, TILE_M rows (output channels; 128 by default, 64 for
// fewer) by the columns below, loaded straight from their tensors (tensor
// addressing: the driver stages, pipelines and bounds them). K goes in
// steps of 32 input channels. The bias is added to the 32-bit sums.
//
// Pointwise convolutions (1×1, no padding) multiply the weights by the
// input itself: a workgroup's columns are 128 consecutive pixels. Others
// sum over the kernel's taps: for each, the weights at that tap (reordered
// taps first while planning) times the input shifted by it, a 3D slice
// (channels × rows × columns under a 4×16 tile of the output) that the
// clamped layout pads with zeros.
//
// Measured on an RTX 5060 (shader_stats, Nsight GPU Trace): with 3×3
// geometry, 8×16 output tiles took 255 registers (196 with 64 rows), one
// workgroup per SM; 4×16 tiles 128 (99) and a third less time. Weights
// read tap by tap from their ONNX order (a stride of the kernel's area)
// and as a 3D layout were slower too; steps of 64 or 16, and 128
// invocations, no faster. Gathering the patches through shared memory,
// and MatMul on these, were slower than gemm_tensor.glsl.
// Workgroups: x over the output's tiles, y over output channels, z batch ×
// group.

#extension GL_KHR_cooperative_matrix : require
#extension GL_NV_cooperative_matrix2 : require
#extension GL_KHR_memory_scope_semantics : require
#extension GL_EXT_shader_explicit_arithmetic_types_float16 : require
#extension GL_EXT_control_flow_attributes : require

// The tiles of KernelKind::ConvGemm2 (kernels.rs: CM2_*).
#ifndef TILE_M
#define TILE_M 128
#endif
// Pointwise convolutions' columns.
const uint TILE_N = 128u;
// Other convolutions' output tile.
const uint TILE_H = 4u;
const uint TILE_W = 16u;
const uint TILE_P = TILE_H * TILE_W;
const uint STEP = 32u;

layout(local_size_x = 256) in;

layout(set = 0, binding = 0, std430) readonly buffer X { float16_t x[]; };
layout(set = 0, binding = 1, std430) readonly buffer W { float16_t w[]; };
layout(set = 0, binding = 2, std430) readonly buffer Bias { float16_t bias[]; };
layout(set = 0, binding = 3, std430) writeonly buffer Out { float16_t o[]; };

layout(push_constant) uniform Params {
    uint n;
    uint in_channels;
    uint in_h;
    uint in_w;
    uint out_channels;
    uint out_h;
    uint out_w;
    uint kernel_h;
    uint kernel_w;
    uint stride_h;
    uint stride_w;
    uint pad_h;
    uint pad_w;
    uint dilation_h;
    uint dilation_w;
    uint group;
    uint has_bias;
} p;

#define CLAMP gl_CooperativeMatrixClampModeConstantNV
#define SUMS(N) coopmat<float, gl_ScopeWorkgroup, TILE_M, N, gl_MatrixUseAccumulator>
#define OUT(N) coopmat<float16_t, gl_ScopeWorkgroup, TILE_M, N, gl_MatrixUseAccumulator>
#define MAT_A coopmat<float16_t, gl_ScopeWorkgroup, TILE_M, STEP, gl_MatrixUseA>
#define MAT_B(N) coopmat<float16_t, gl_ScopeWorkgroup, STEP, N, gl_MatrixUseB>

uint first_out;

float add_bias(const in uint r, const in uint c, const in float sum, const in uint row0) {
    return sum + float(bias[first_out + row0 + r]);
}

void main() {
    uint g = gl_WorkGroupID.z % p.group;
    uint b = gl_WorkGroupID.z / p.group;
    uint per_group_in = p.in_channels / p.group;
    uint per_group_out = p.out_channels / p.group;
    uint pixels = p.out_h * p.out_w;
    uint k = per_group_in * TAPS;
    uint x_base = (b * p.in_channels + g * per_group_in) * p.in_h * p.in_w;
    first_out = g * per_group_out;
    uint w_base = first_out * k;
    uint out_base = (b * p.out_channels + first_out) * pixels;
    uint row0 = gl_WorkGroupID.y * TILE_M;

    if (POINTWISE) {
        SUMS(TILE_N) sums = SUMS(TILE_N)(0.0);
        uint col0 = gl_WorkGroupID.x * TILE_N;
        tensorLayoutNV<2, CLAMP> lw = createTensorLayoutNV(2, CLAMP);
        lw = setTensorLayoutDimensionNV(lw, per_group_out, k);
        tensorLayoutNV<2, CLAMP> lx = createTensorLayoutNV(2, CLAMP);
        lx = setTensorLayoutDimensionNV(lx, k, pixels);
        for (uint k0 = 0u; k0 < k; k0 += STEP) {
            MAT_A mw;
            MAT_B(TILE_N) mx;
            coopMatLoadTensorNV(mw, w, w_base, sliceTensorLayoutNV(lw, row0, TILE_M, k0, STEP));
            coopMatLoadTensorNV(mx, x, x_base, sliceTensorLayoutNV(lx, k0, STEP, col0, TILE_N));
            sums = coopMatMulAdd(mw, mx, sums);
        }
        if (p.has_bias != 0u) {
            coopMatPerElementNV(sums, sums, add_bias, row0);
        }
        tensorLayoutNV<2, CLAMP> lo = createTensorLayoutNV(2, CLAMP);
        lo = setTensorLayoutDimensionNV(lo, per_group_out, pixels);
        OUT(TILE_N) out16 = OUT(TILE_N)(sums);
        coopMatStoreTensorNV(out16, o, out_base, sliceTensorLayoutNV(lo, row0, TILE_M, col0, TILE_N));
        return;
    }

    uint tiles_w = (p.out_w + TILE_W - 1u) / TILE_W;
    uint oy0 = (gl_WorkGroupID.x / tiles_w) * TILE_H;
    uint ox0 = (gl_WorkGroupID.x % tiles_w) * TILE_W;
    // Weights as out channels × (taps × in channels), reordered so while
    // planning: a tap's step is a slice of columns. (Past the input
    // channels it reaches into the next tap, harmlessly: those rows of the
    // input read as zeros.) The input as channels × rows × columns: a slice
    // is a matrix with its rows and columns flattened.
    tensorLayoutNV<2, CLAMP> lw = createTensorLayoutNV(2, CLAMP);
    lw = setTensorLayoutDimensionNV(lw, per_group_out, k);
    tensorLayoutNV<3, CLAMP> lx = createTensorLayoutNV(3, CLAMP);
    lx = setTensorLayoutDimensionNV(lx, per_group_in, p.in_h, p.in_w);
    SUMS(TILE_P) sums = SUMS(TILE_P)(0.0);
    [[dont_unroll]] for (uint tap = 0u; tap < TAPS; tap++) {
        uint ky = tap / KERNEL_W;
        uint kx = tap - ky * KERNEL_W;
        // Above or left of the input (padding) wraps around to huge
        // coordinates, which the clamped layout reads as zeros.
        uint iy0 = oy0 + ky * DILATION_H - PAD_H;
        uint ix0 = ox0 + kx * DILATION_W - PAD_W;
        for (uint c0 = 0u; c0 < per_group_in; c0 += STEP) {
            MAT_A mw;
            MAT_B(TILE_P) mx;
            coopMatLoadTensorNV(mw, w, w_base,
                                sliceTensorLayoutNV(lw, row0, TILE_M, tap * per_group_in + c0, STEP));
            coopMatLoadTensorNV(mx, x, x_base,
                                sliceTensorLayoutNV(lx, c0, STEP, iy0, TILE_H, ix0, TILE_W));
            sums = coopMatMulAdd(mw, mx, sums);
        }
    }
    if (p.has_bias != 0u) {
        coopMatPerElementNV(sums, sums, add_bias, row0);
    }
    tensorLayoutNV<3, CLAMP> lo = createTensorLayoutNV(3, CLAMP);
    lo = setTensorLayoutDimensionNV(lo, per_group_out, p.out_h, p.out_w);
    OUT(TILE_P) out16 = OUT(TILE_P)(sums);
    coopMatStoreTensorNV(out16, o, out_base,
                         sliceTensorLayoutNV(lo, row0, TILE_M, oy0, TILE_H, ox0, TILE_W));
}
