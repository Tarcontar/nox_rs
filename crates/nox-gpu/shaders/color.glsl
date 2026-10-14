// Color math shared by the shaders; mirrors nox_color::transfer and
// nox_color::working. Needs GL_GOOGLE_include_directive.

#include "color_constants.glsl"

// SDR signal (0–1) to display light relative to reference white.
vec3 eotf(vec3 v, int transfer) {
    v = clamp(v, 0.0, 1.0);
    if (transfer == TRANSFER_SRGB) {
        return mix(pow((v + 0.055) / 1.055, vec3(2.4)), v / 12.92, lessThanEqual(v, vec3(0.04045)));
    }
    if (transfer == TRANSFER_GAMMA22) {
        return pow(v, vec3(2.2));
    }
    if (transfer == TRANSFER_GAMMA28) {
        return pow(v, vec3(2.8));
    }
    if (transfer == TRANSFER_LINEAR) {
        return v;
    }
    return pow(v, vec3(2.4)); // BT.1886
}

// Linear working-space RGB to output R'G'B' (0–1): BT.709 primaries,
// clipped to the gamut, BT.1886 encoding.
vec3 output_transform(vec3 working) {
    vec3 rgb = clamp(WORKING_TO_OUTPUT * working, 0.0, 1.0);
    return pow(rgb, vec3(1.0 / 2.4));
}

// ACEScct (S-2016-001), the encoding grades work in; mirrors
// nox_color::grade.
vec3 acescct_from_linear(vec3 lin) {
    return mix((log2(max(lin, 1e-10)) + 9.72) / 17.52,
               10.5402377 * lin + 0.0729055341,
               lessThanEqual(lin, vec3(0.0078125)));
}

vec3 linear_from_acescct(vec3 cct) {
    return mix(min(exp2(cct * 17.52 - 9.72), 65504.0),
               (cct - 0.0729055341) / 10.5402377,
               lessThanEqual(cct, vec3(0.155251141)));
}
