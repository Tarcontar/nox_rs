// HDR10 light levels (MaxCLL and MaxFALL, CTA-861.3), measured by the
// output shaders that write encoders' frames: every workgroup writes the
// brightest pixel and the sum over its pixels of max(R, G, B) in nits to
// `light.partials`, which the compositor sums up.
//
// Include after `layout(local_size_x …) in;` and color.glsl, with the
// buffer declared as
//   layout(…, std430) writeonly buffer Light { vec2 partials[]; } light;

shared vec2 shared_light[gl_WorkGroupSize.x * gl_WorkGroupSize.y];

// The largest of R, G, B of a working-space color in nits, as PQ
// represents it.
float brightest_nits(vec3 working) {
    vec3 nits = min(max(WORKING_TO_BT2020 * working, 0.0) * 203.0, 10000.0);
    return max(nits.r, max(nits.g, nits.b));
}

// Adds a pixel's light to an invocation's (peak, sum).
vec2 add_light(vec2 measured, float nits) {
    return vec2(max(measured.x, nits), measured.y + nits);
}

// Reduces the invocations' (peak, sum) and stores the workgroup's. Call
// from all invocations (uniform control flow).
void store_light(vec2 measured) {
    uint index = gl_LocalInvocationIndex;
    shared_light[index] = measured;
    barrier();
    for (uint stride = gl_WorkGroupSize.x * gl_WorkGroupSize.y / 2; stride > 0; stride /= 2) {
        if (index < stride) {
            vec2 other = shared_light[index + stride];
            shared_light[index] = vec2(max(shared_light[index].x, other.x), shared_light[index].y + other.y);
        }
        barrier();
    }
    if (index == 0) {
        light.partials[gl_WorkGroupID.y * gl_NumWorkGroups.x + gl_WorkGroupID.x] = shared_light[0];
    }
}
