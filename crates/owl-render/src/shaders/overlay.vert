#version 330 core

// A quad for one subtitle image, positioned directly in clip space. The
// corners arrive as a rectangle rather than a transform because subtitle
// rectangles are axis-aligned by definition and a matrix would be four
// numbers pretending to be nine.
out vec2 vTexCoord;

uniform vec4 uRect;  // x0, y0 (top), x1, y1 (bottom), in NDC

void main() {
    vec2 corner = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
    // Texture row 0 is the top of the image, which is also y0.
    vTexCoord = corner;
    vec2 p = mix(uRect.xy, uRect.zw, corner);
    gl_Position = vec4(p, 0.0, 1.0);
}
