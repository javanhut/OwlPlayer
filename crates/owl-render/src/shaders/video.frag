#version 330 core

// One shader for every pixel format the decoder can hand us. The branches
// are on uniforms, not on data, so every pixel in a frame takes the same
// path and the driver's constant folding removes the rest.

in vec2 vTexCoord;
out vec4 fragColor;

uniform sampler2D uPlane0;
uniform sampler2D uPlane1;
uniform sampler2D uPlane2;

uniform int   uPlanes;      // 1 = packed RGB, 2 = Y + interleaved UV, 3 = Y/U/V
uniform int   uSwapUV;      // NV21 stores Cr before Cb
uniform int   uIsRgb;
uniform int   uBgra;
uniform float uDepthScale;  // lifts a 10/12-bit value out of its 16-bit container
uniform mat3  uYuvToRgb;
uniform vec3  uYuvOffset;   // limited-range footroom, removed before the matrix
uniform int   uTransfer;    // 0 = SDR, 1 = PQ (HDR10), 2 = HLG
uniform mat3  uPrimaries;   // BT.2020 -> BT.709 when tone mapping, else identity
uniform float uPeakNits;

const float PQ_M1 = 0.1593017578125;
const float PQ_M2 = 78.84375;
const float PQ_C1 = 0.8359375;
const float PQ_C2 = 18.8515625;
const float PQ_C3 = 18.6875;

// SMPTE ST 2084 inverse EOTF: coded value -> absolute luminance, scaled so
// 1.0 is 10000 nits.
vec3 pqToLinear(vec3 v) {
    vec3 p = pow(max(v, 0.0), vec3(1.0 / PQ_M2));
    vec3 num = max(p - PQ_C1, 0.0);
    vec3 den = PQ_C2 - PQ_C3 * p;
    return pow(num / max(den, 1e-6), vec3(1.0 / PQ_M1));
}

// ARIB STD-B67. The OETF is piecewise; this is its inverse, followed by
// the system gamma that HLG leaves to the display.
vec3 hlgToLinear(vec3 v) {
    const float a = 0.17883277;
    const float b = 0.28466892;
    const float c = 0.55991073;
    vec3 lo = (v * v) / 3.0;
    vec3 hi = (exp((v - c) / a) + b) / 12.0;
    vec3 linear = mix(lo, hi, step(0.5, v));
    float luma = dot(linear, vec3(0.2627, 0.6780, 0.0593));
    return linear * pow(max(luma, 1e-6), 0.2);
}

// Tone map by luminance rather than per channel: scaling R, G and B
// independently desaturates every bright object, which is why naive HDR
// playback makes skies go white instead of staying blue.
vec3 toneMap(vec3 linear) {
    float luma = dot(linear, vec3(0.2627, 0.6780, 0.0593));
    if (luma < 1e-6) return linear;
    float scaled = luma * uPeakNits;
    // Reinhard with a white point, which keeps the toe linear so mid tones
    // are not crushed on the way down to SDR.
    float mapped = scaled * (1.0 + scaled / (uPeakNits * uPeakNits)) / (1.0 + scaled);
    return linear * (mapped / luma);
}

vec3 linearToSrgb(vec3 c) {
    c = clamp(c, 0.0, 1.0);
    return mix(12.92 * c, 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055, step(0.0031308, c));
}

void main() {
    vec3 rgb;

    if (uIsRgb == 1) {
        vec4 texel = texture(uPlane0, vTexCoord);
        rgb = (uBgra == 1) ? texel.bgr : texel.rgb;
    } else {
        float y;
        vec2 uv;
        if (uPlanes == 3) {
            y  = texture(uPlane0, vTexCoord).r;
            uv = vec2(texture(uPlane1, vTexCoord).r, texture(uPlane2, vTexCoord).r);
        } else {
            y  = texture(uPlane0, vTexCoord).r;
            uv = texture(uPlane1, vTexCoord).rg;
            if (uSwapUV == 1) uv = uv.gr;
        }
        vec3 yuv = vec3(y, uv) * uDepthScale - uYuvOffset;
        rgb = uYuvToRgb * yuv;
    }

    if (uTransfer != 0) {
        vec3 linear = (uTransfer == 1) ? pqToLinear(rgb) : hlgToLinear(rgb);
        linear = uPrimaries * linear;
        rgb = linearToSrgb(toneMap(linear));
    }

    fragColor = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
