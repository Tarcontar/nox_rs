// gemm.glsl on tensor cores (cooperative matrices; compiled with TENSOR,
// for F16 plans): out = A × B with A M×K and B K×N, the same hooks
// (load_a, load_b, store) and workgroups (x over N, y over M).
//
// 128 invocations, four 32-wide subgroups, compute a TILE_M×TILE_N tile
// (64×64 by default; 32×128 and 16×128 for few rows, e.g. convolutions'
// output channels, which would leave most of a 64-row tile empty): the
// subgroups SUBGROUP_ROWS × 4 / SUBGROUP_ROWS, each a block of 16×16
// matrices of 32-bit sums. K goes
// in steps of 32, staged in shared memory as 16-bit floats (zeros past the
// edges); each subgroup hands its sums to store() a 16×16 matrix at a
// time through 1 KB of shared memory of its own (a whole 64×64 tile of
// sums took 17 KB: 27 KB per workgroup, too much for 5 of them per SM).
// store() only sees elements inside the matrices. Needs 16 × 16 × 16 f16·f16 → f32 at
// subgroup scope and 32-wide subgroups (GpuDevice::tensor_f16). An
// invocation's B loads are always of one column, col0 + its index %
// TILE_N (conv_gemm.comp relies on it; TILE_N divides 128).


#extension GL_KHR_cooperative_matrix : require
#extension GL_KHR_memory_scope_semantics : require
#extension GL_KHR_shader_subgroup_basic : require
#extension GL_EXT_shader_explicit_arithmetic_types_float16 : require
#extension GL_EXT_control_flow_attributes : require

layout(local_size_x = 128) in;

#ifndef TILE_M
#define TILE_M 64
#define TILE_N 64
#define SUBGROUP_ROWS 2
#endif
const uint STEP = 32u;
// 16×16 matrices per subgroup, down and across.
const uint FM = TILE_M / (SUBGROUP_ROWS * 16u);
const uint FN = TILE_N / ((4u / SUBGROUP_ROWS) * 16u);
// Rows padded by 8 halves (16 bytes), against bank conflicts.
const uint A_STRIDE = STEP + 8u;
const uint B_STRIDE = TILE_N + 8u;

shared float16_t tile_a[TILE_M * A_STRIDE];
shared float16_t tile_b[STEP * B_STRIDE];
// A 16×16 matrix of sums per subgroup.
shared float tile_c[4u * 256u];

float load_a(uint row, uint k);
float load_b(uint k, uint col);
void store(uint row, uint col, float value);

#define SUMS coopmat<float, gl_ScopeSubgroup, 16, 16, gl_MatrixUseAccumulator>

void gemm(uint m, uint n, uint k) {
    uint t = gl_LocalInvocationID.x;
    uint row0 = gl_WorkGroupID.y * TILE_M;
    uint col0 = gl_WorkGroupID.x * TILE_N;
    // This subgroup's block.
    uint qr = (gl_SubgroupID / (4u / SUBGROUP_ROWS)) * FM * 16u;
    uint qc = (gl_SubgroupID % (4u / SUBGROUP_ROWS)) * FN * 16u;
    SUMS sums[FM][FN];
    [[unroll]] for (uint i = 0u; i < FM; i++) {
        [[unroll]] for (uint j = 0u; j < FN; j++) {
            sums[i][j] = SUMS(0.0);
        }
    }
    for (uint k0 = 0u; k0 < k; k0 += STEP) {
        // TILE_M×32 of A along K, 32×TILE_N of B along N, 4 at a time.
        // Unrolled fully, the loads took 156 registers per invocation (3
        // workgroups per SM, 24% occupancy); not at all, too few loads in
        // flight. 4 at a time: 90 registers and ~10% faster (RTX 5060,
        // shader_stats and Nsight GPU Trace).
        [[dont_unroll]] for (uint i0 = 0u; i0 < TILE_M * STEP / 128u; i0 += 4u) {
            [[unroll]] for (uint i = i0; i < i0 + 4u; i++) {
                uint e = t + 128u * i;
                uint ak = e % STEP;
                uint ar = e / STEP;
                tile_a[ar * A_STRIDE + ak] = float16_t(
                    (row0 + ar < m && k0 + ak < k) ? load_a(row0 + ar, k0 + ak) : 0.0);
            }
        }
        [[dont_unroll]] for (uint i0 = 0u; i0 < STEP * TILE_N / 128u; i0 += 4u) {
            [[unroll]] for (uint i = i0; i < i0 + 4u; i++) {
                uint e = t + 128u * i;
                uint bc = e % TILE_N;
                uint bk = e / TILE_N;
                tile_b[bk * B_STRIDE + bc] = float16_t(
                    (k0 + bk < k && col0 + bc < n) ? load_b(k0 + bk, col0 + bc) : 0.0);
            }
        }
        barrier();
        [[unroll]] for (uint s = 0u; s < STEP; s += 16u) {
            coopmat<float16_t, gl_ScopeSubgroup, 16, 16, gl_MatrixUseA> a[FM];
            coopmat<float16_t, gl_ScopeSubgroup, 16, 16, gl_MatrixUseB> b[FN];
            [[unroll]] for (uint i = 0u; i < FM; i++) {
                coopMatLoad(a[i], tile_a, (qr + 16u * i) * A_STRIDE + s, A_STRIDE,
                            gl_CooperativeMatrixLayoutRowMajor);
            }
            [[unroll]] for (uint j = 0u; j < FN; j++) {
                coopMatLoad(b[j], tile_b, s * B_STRIDE + qc + 16u * j, B_STRIDE,
                            gl_CooperativeMatrixLayoutRowMajor);
            }
            [[unroll]] for (uint i = 0u; i < FM; i++) {
                [[unroll]] for (uint j = 0u; j < FN; j++) {
                    sums[i][j] = coopMatMulAdd(a[i], b[j], sums[i][j]);
                }
            }
        }
        barrier();
    }
    uint scratch = gl_SubgroupID * 256u;
    uint lane = gl_SubgroupInvocationID;
    [[unroll]] for (uint i = 0u; i < FM; i++) {
        [[unroll]] for (uint j = 0u; j < FN; j++) {
            coopMatStore(sums[i][j], tile_c, scratch, 16u, gl_CooperativeMatrixLayoutRowMajor);
            subgroupMemoryBarrierShared();
            subgroupBarrier();
            // Out along N: 8 per invocation, two rows of 16 at a time.
            uint row = row0 + qr + 16u * i;
            uint col = col0 + qc + 16u * j;
            [[unroll]] for (uint e = lane; e < 256u; e += 32u) {
                uint r = e / 16u;
                uint c = e % 16u;
                if (row + r < m && col + c < n) {
                    store(row + r, col + c, tile_c[scratch + e]);
                }
            }
            // Before the next matrix goes in.
            subgroupBarrier();
        }
    }
}
