struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

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
    output.position = vec4<f32>(input.position, 0.0, 1.0);
    output.uv = input.uv;
    output.color = input.color;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(tile_texture, tile_sampler, input.uv) * input.color;
    let red = textureLoad(gamma_lut, vec2<i32>(i32(color.r * 255.0), 0), 0).r;
    let green = textureLoad(gamma_lut, vec2<i32>(i32(color.g * 255.0), 0), 0).g;
    let blue = textureLoad(gamma_lut, vec2<i32>(i32(color.b * 255.0), 0), 0).b;
    let corrected = vec4<f32>(red, green, blue, color.a);
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
