// A convolution's geometry as specialization constants, for every variant
// of conv_gemm.comp (KernelKind::constants; the push constants' copies
// aren't read): divisions by them compile to multiplications, and without
// padding no bounds are checked.
layout(constant_id = 0) const uint KERNEL_H = 1u;
layout(constant_id = 1) const uint KERNEL_W = 1u;
layout(constant_id = 2) const uint STRIDE_H = 1u;
layout(constant_id = 3) const uint STRIDE_W = 1u;
layout(constant_id = 4) const uint PAD_H = 0u;
layout(constant_id = 5) const uint PAD_W = 0u;
layout(constant_id = 6) const uint DILATION_H = 1u;
layout(constant_id = 7) const uint DILATION_W = 1u;
const uint TAPS = KERNEL_H * KERNEL_W;
// 1×1, stride 1, no padding: B is the input itself (channels × pixels).
const bool POINTWISE = TAPS == 1u && STRIDE_H == 1u && STRIDE_W == 1u && PAD_H == 0u
    && PAD_W == 0u;
const bool PADDED = PAD_H != 0u || PAD_W != 0u;
