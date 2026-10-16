// A tiled matrix multiplication shared by the kernels that are one (Conv,
// MatMul): out = A × B with A M×K and B K×N. Each workgroup computes a
// 64×64 tile of the output, its 256 invocations 4×4 values each, going
// through K in steps of 16 staged in shared memory.
//
// The shader including it defines how elements are read and written:
//   float load_a(uint row, uint k);
//   float load_b(uint k, uint col);
//   void store(uint row, uint col, float value);
// gemm() only asks for elements inside the matrices. Workgroups: x over
// N, y over M (64 each); z is the shader's to use (batches). An
// invocation's B loads are always of one column, col0 + its index % 64
// (conv_gemm.comp relies on it).

layout(local_size_x = 16, local_size_y = 16) in;

const uint TILE = 64u;
// gemm_tensor.glsl's name for the columns.
const uint TILE_N = TILE;
const uint STEP = 16u;

// Padded a column, so writing a tile (along K) hits different banks.
shared float tile_a[STEP][TILE + 1u];
shared float tile_b[STEP][TILE];

float load_a(uint row, uint k);
float load_b(uint k, uint col);
void store(uint row, uint col, float value);

void gemm(uint m, uint n, uint k) {
    uint tx = gl_LocalInvocationID.x;
    uint ty = gl_LocalInvocationID.y;
    uint t = ty * 16u + tx;
    uint row0 = gl_WorkGroupID.y * TILE;
    uint col0 = gl_WorkGroupID.x * TILE;
    float acc[4][4];
    for (uint i = 0u; i < 4u; i++) {
        for (uint j = 0u; j < 4u; j++) {
            acc[i][j] = 0.0;
        }
    }
    for (uint k0 = 0u; k0 < k; k0 += STEP) {
        for (uint i = 0u; i < 4u; i++) {
            uint e = t + 256u * i;
            // A along K (contiguous in a row), B along N.
            uint ak = e % STEP;
            uint ar = e / STEP;
            tile_a[ak][ar] =
                (row0 + ar < m && k0 + ak < k) ? load_a(row0 + ar, k0 + ak) : 0.0;
            uint bc = e % TILE;
            uint bk = e / TILE;
            tile_b[bk][bc] =
                (k0 + bk < k && col0 + bc < n) ? load_b(k0 + bk, col0 + bc) : 0.0;
        }
        barrier();
        for (uint s = 0u; s < STEP; s++) {
            float a[4];
            float b[4];
            for (uint i = 0u; i < 4u; i++) {
                a[i] = tile_a[s][ty + 16u * i];
                b[i] = tile_b[s][tx + 16u * i];
            }
            for (uint i = 0u; i < 4u; i++) {
                for (uint j = 0u; j < 4u; j++) {
                    acc[i][j] = fma(a[i], b[j], acc[i][j]);
                }
            }
        }
        barrier();
    }
    for (uint i = 0u; i < 4u; i++) {
        uint row = row0 + ty + 16u * i;
        for (uint j = 0u; j < 4u; j++) {
            uint col = col0 + tx + 16u * j;
            if (row < m && col < n) {
                store(row, col, acc[i][j]);
            }
        }
    }
}
