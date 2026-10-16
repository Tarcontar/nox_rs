// Shared by the effects' kernels: one invocation per pixel of a picture
// in the working format (RGBA32F, linear ACEScg, straight alpha), in
// 16 × 16 workgroups.

layout(local_size_x = 16, local_size_y = 16) in;

// This invocation's pixel, or false past the picture's edges.
bool pixel(uvec2 size, out ivec2 pos) {
    pos = ivec2(gl_GlobalInvocationID.xy);
    return all(lessThan(gl_GlobalInvocationID.xy, size));
}
