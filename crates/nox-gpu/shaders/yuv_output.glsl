// Shared by the 4:2:0 Y'CbCr outputs (output_yuv.comp, output_linear.comp).
// Expects `accum` (the accumulated picture) and push constants `p` with
// `size`, `encoding` and the R'G'B' → Y'CbCr rows `row1`, `row2`.

// Output R'G'B' at `pos`, clamped to the picture.
vec4 rgb_at(ivec2 pos) {
    pos = clamp(pos, ivec2(0), p.size - 1);
    return vec4(encode_output(imageLoad(accum, pos).rgb, p.encoding), 1.0);
}

// Cb, Cr (normalized) of the left-sited chroma sample at luma column `x`,
// rows `y` and `y + 1`: [1 2 1] across, averaged down.
vec2 chroma_at(int x, int y) {
    vec4 rgb = vec4(0.0);
    for (int dy = 0; dy < 2; dy++) {
        ivec2 row = ivec2(x, y + dy);
        rgb += 0.25 * rgb_at(row - ivec2(1, 0)) + 0.5 * rgb_at(row) + 0.25 * rgb_at(row + ivec2(1, 0));
    }
    rgb *= 0.5;
    return vec2(dot(p.row1, rgb), dot(p.row2, rgb));
}
