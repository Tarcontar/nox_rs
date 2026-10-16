// Sums and maxima over a workgroup of GROUP invocations (the including
// shader defines GROUP, and includes this before its declarations):
// within each subgroup first, then across the subgroups through shared
// memory. Every invocation gets the result; call it from uniform control
// flow (it has barriers).

#extension GL_KHR_shader_subgroup_basic : require
#extension GL_KHR_shader_subgroup_arithmetic : require

shared float group_partial[GROUP];

float group_sum(float v) {
    float s = subgroupAdd(v);
    if (subgroupElect()) {
        group_partial[gl_SubgroupID] = s;
    }
    barrier();
    float total = 0.0;
    for (uint g = 0u; g < gl_NumSubgroups; g++) {
        total += group_partial[g];
    }
    // Before the partials are written again.
    barrier();
    return total;
}

float group_max(float v) {
    float m = subgroupMax(v);
    if (subgroupElect()) {
        group_partial[gl_SubgroupID] = m;
    }
    barrier();
    float total = -3.402823466e38;
    for (uint g = 0u; g < gl_NumSubgroups; g++) {
        total = max(total, group_partial[g]);
    }
    barrier();
    return total;
}
