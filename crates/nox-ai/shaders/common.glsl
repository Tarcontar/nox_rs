// Shared by the kernels: one invocation per output element, numbered
// across a 2D grid of workgroups (large tensors need more than 65535
// groups in x). Tensors are float arrays at element offsets.

layout(local_size_x = 256) in;

uint element() {
    return gl_GlobalInvocationID.x + gl_GlobalInvocationID.y * gl_NumWorkGroups.x * 256u;
}
