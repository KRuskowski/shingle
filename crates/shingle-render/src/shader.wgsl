struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) flags: u32,
    // The w the position was divided by, 1 unless the tile is projected.
    @location(4) w: f32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) flags: u32,
};

// Tile flags, as in dmabuf.rs (`tile_flags`).
// The texture's alpha is ignored: X formats.
const FLAG_OPAQUE: u32 = 1u;
// The texture holds sRGB values in a UNORM format.
const FLAG_DECODE_SRGB: u32 = 2u;

fn srgb_to_linear(value: vec3<f32>) -> vec3<f32> {
    let low = value / 12.92;
    let high = pow((value + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(high, low, value <= vec3<f32>(0.04045));
}

fn linear_to_srgb(value: vec3<f32>) -> vec3<f32> {
    let low = value * 12.92;
    let high = 1.055 * pow(value, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(high, low, value <= vec3<f32>(0.0031308));
}

@group(0) @binding(0) var tile_texture: texture_2d<f32>;
@group(0) @binding(1) var tile_sampler: sampler;
@group(1) @binding(0) var gamma_lut: texture_2d<f32>;

struct EffectUniform {
    inverted: u32,
    preserve_hue: u32,
};
@group(2) @binding(0) var<uniform> effects: EffectUniform;

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.position = vec4<f32>(input.position * input.w, 0.0, input.w);
    output.uv = input.uv;
    output.color = input.color;
    output.flags = input.flags;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    var texel = textureSample(tile_texture, tile_sampler, input.uv);
    if ((input.flags & FLAG_DECODE_SRGB) != 0u) {
        texel = vec4<f32>(srgb_to_linear(texel.rgb), texel.a);
    }
    if ((input.flags & FLAG_OPAQUE) != 0u) {
        texel.a = 1.0;
    }
    let color = texel * input.color;
    // A gamma ramp maps the bytes sent to the display, so the LUT is
    // indexed by the colour's sRGB byte, and its entry, a byte too, is
    // decoded again for the sRGB target. With the identity ramp every byte
    // comes out as it went in.
    let encoded =
        linear_to_srgb(clamp(color.rgb, vec3<f32>(0.0), vec3<f32>(1.0)));
    let index = vec3<i32>(round(encoded * 255.0));
    let red = textureLoad(gamma_lut, vec2<i32>(index.r, 0), 0).r;
    let green = textureLoad(gamma_lut, vec2<i32>(index.g, 0), 0).g;
    let blue = textureLoad(gamma_lut, vec2<i32>(index.b, 0), 0).b;
    let corrected =
        vec4<f32>(srgb_to_linear(vec3<f32>(red, green, blue)), color.a);
    if (effects.inverted == 1u) {
        if (effects.preserve_hue == 1u) {
            let hue = corrected.a - min(corrected.r, min(corrected.g, corrected.b))
                - max(corrected.r, max(corrected.g, corrected.b));
            return vec4<f32>(corrected.rgb + vec3<f32>(hue), corrected.a);
        }
        return vec4<f32>(1.0 - corrected.r, 1.0 - corrected.g, 1.0 - corrected.b, corrected.a);
    }
    return corrected;
}
