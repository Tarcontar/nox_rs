// Mattes: planes of 32-bit floats in 0–1, `size` wide and high, row by
// row, at their own resolution (a model's); sampled bilinearly for a
// picture of another size, edges clamped. The including shader declares
// the matte buffer as `float matte[]` before including this.

float matte_at(ivec2 pos, uvec2 size) {
    pos = clamp(pos, ivec2(0), ivec2(size) - 1);
    return matte[uint(pos.y) * size.x + uint(pos.x)];
}

// The matte at pixel `pos` of a picture `picture_size` large.
float sample_matte(ivec2 pos, uvec2 picture_size, uvec2 size) {
    vec2 xy = (vec2(pos) + 0.5) * vec2(size) / vec2(picture_size) - 0.5;
    ivec2 c = ivec2(floor(xy));
    vec2 f = xy - vec2(c);
    return mix(mix(matte_at(c, size), matte_at(c + ivec2(1, 0), size), f.x),
               mix(matte_at(c + ivec2(0, 1), size), matte_at(c + ivec2(1, 1), size), f.x),
               f.y);
}
