#version 330 core

// Subtitle images arrive as straight (non-premultiplied) RGBA, so the
// blend is the ordinary source-alpha one set up by the caller.
in vec2 vTexCoord;
out vec4 fragColor;

uniform sampler2D uImage;

void main() {
    fragColor = texture(uImage, vTexCoord);
}
