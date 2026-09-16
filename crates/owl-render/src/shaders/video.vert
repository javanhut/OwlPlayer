#version 330 core

// A full-screen triangle, generated from gl_VertexID. No vertex buffer,
// no index buffer, no attribute plumbing: three vertices covering the
// viewport, which also avoids the diagonal seam a two-triangle quad puts
// through the middle of the picture on some drivers.
out vec2 vTexCoord;

uniform vec4 uViewport;  // xy = scale, zw = offset. Letterboxing lives here.

void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    vTexCoord = p;
    vec2 ndc = p * 2.0 - 1.0;
    gl_Position = vec4(ndc * uViewport.xy + uViewport.zw, 0.0, 1.0);
}
