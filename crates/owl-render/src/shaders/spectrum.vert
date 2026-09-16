#version 330 core

// Full-screen triangle again. vUv.y is 0 at the bottom, which is the way
// up for something that grows out of a baseline.
out vec2 vUv;

void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    vUv = p;
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
