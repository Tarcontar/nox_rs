// Tensors' element type, `real`: 32-bit floats, or 16-bit ones when
// compiled with F16 (an fp16 plan: half the memory and bandwidth). The
// arithmetic is 32-bit either way: values are read with float() and
// written with real().
#ifdef F16
#extension GL_EXT_shader_16bit_storage : require
#define real float16_t
#else
#define real float
#endif
