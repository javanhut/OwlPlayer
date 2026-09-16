#version 330 core

// The music visualiser. Bars are drawn procedurally from a one-pixel-tall
// texture of band levels rather than from geometry: one draw call, no
// vertex buffer, and the bar count can change without touching anything
// but a uniform.

in vec2 vUv;
out vec4 fragColor;

uniform sampler2D uSpectrum;
uniform vec3  uAccent;
uniform float uBands;

// Where the bars stand, as a fraction of the height. Low enough that tall
// bars have somewhere to go, high enough to leave room for the reflection.
const float BASELINE = 0.34;
const float RISE     = 0.50;
const float REFLECT  = 0.20;

void main() {
    float cell = 1.0 / uBands;
    float index = floor(vUv.x / cell);
    float centre = (index + 0.5) * cell;
    float level = texture(uSpectrum, vec2(centre, 0.5)).r;

    // A gap between bars, as a fraction of the cell.
    float halfWidth = cell * 0.30;
    float dx = abs(vUv.x - centre);
    float across = 1.0 - smoothstep(halfWidth - cell * 0.05, halfWidth, dx);

    float top = BASELINE + level * RISE;
    float bar = across * step(BASELINE, vUv.y) * (1.0 - smoothstep(top - 0.004, top, vUv.y));

    // A dimmer, shorter image below the line — the same trick a glass
    // shelf plays, and it stops the bars looking like they are falling off
    // the bottom of the window.
    float floorY = BASELINE - level * REFLECT;
    float reflection = across
        * step(vUv.y, BASELINE)
        * smoothstep(floorY, floorY + 0.01, vUv.y)
        * 0.24 * smoothstep(floorY, BASELINE, vUv.y);

    // Bloom around the tip, so loud bands read as bright rather than tall.
    float bloom = across * exp(-abs(vUv.y - top) * 26.0) * level * 0.5;

    // Pale at the top, accent at the base: the gradient is what stops a
    // wall of one colour looking flat.
    vec3 tint = mix(uAccent, vec3(1.0), 0.45 * smoothstep(BASELINE, top + 0.02, vUv.y));

    float alpha = clamp(bar + reflection + bloom, 0.0, 1.0);
    fragColor = vec4(tint, alpha);
}
