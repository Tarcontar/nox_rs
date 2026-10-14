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

// HDR outputs (nox_color::OutputEncoding): BT.2020 with SDR white at 203
// nits (BT.2408), PQ (SMPTE ST 2084) or HLG for a 1000-nit display.
float pq_encode(float nits) {
    const float m1 = 2610.0 / 16384.0;
    const float m2 = 2523.0 / 4096.0 * 128.0;
    const float c1 = 3424.0 / 4096.0;
    const float c2 = 2413.0 / 4096.0 * 32.0;
    const float c3 = 2392.0 / 4096.0 * 32.0;
    float y = pow(clamp(nits / 10000.0, 0.0, 1.0), m1);
    return pow((c1 + c2 * y) / (1.0 + c3 * y), m2);
}

float hlg_oetf(float e) {
    e = max(e, 0.0);
    return e <= 1.0 / 12.0 ? sqrt(3.0 * e)
                           : 0.17883277 * log(12.0 * e - 0.28466892) + 0.55991073;
}

// Linear working-space RGB to output R'G'B' in `encoding` (ENCODING_*).
vec3 encode_output(vec3 working, int encoding) {
    if (encoding == ENCODING_SDR) {
        return output_transform(working);
    }
    vec3 nits = max(WORKING_TO_BT2020 * working, 0.0) * 203.0;
    if (encoding == ENCODING_PQ) {
        return vec3(pq_encode(nits.r), pq_encode(nits.g), pq_encode(nits.b));
    }
    // HLG: the inverse BT.2100 OOTF (system gamma 1.2), then the OETF.
    vec3 display = clamp(nits / 1000.0, 0.0, 1.0);
    float y = dot(vec3(0.2627, 0.6780, 0.0593), display);
    float gain = y > 0.0 ? pow(y, (1.0 - 1.2) / 1.2) : 0.0;
    vec3 e = min(display * gain, 1.0);
    return vec3(hlg_oetf(e.r), hlg_oetf(e.g), hlg_oetf(e.b));
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
