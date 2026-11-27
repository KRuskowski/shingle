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
    let texel = textureSample(tile_texture, tile_sampler, input.uv);
    return shade(texel, input.color, input.flags);
}

// A sampled texel times the tile colour, through the gamma ramp and the
// inversion effect.
fn shade(sampled: vec4<f32>, tint: vec4<f32>, flags: u32) -> vec4<f32> {
    var texel = sampled;
    if ((flags & FLAG_DECODE_SRGB) != 0u) {
        texel = vec4<f32>(srgb_to_linear(texel.rgb), texel.a);
    }
    if ((flags & FLAG_OPAQUE) != 0u) {
        texel.a = 1.0;
    }
    return post_process(texel * tint);
}

// The gamma ramp and the inversion effect, on a premultiplied colour.
fn post_process(color: vec4<f32>) -> vec4<f32> {
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

// Wayfire's squeezimize warp (wayfire:plugins/animate/squeezimize.hpp:68-174),
// in the tile's area: `box_uv` is Wayfire's `uv`, 0 to 1 with y up, and
// the boxes are fractions of the area with y down, as Wayfire passes them.
struct SqueezeInput {
    @location(0) position: vec2<f32>,
    @location(1) box_uv: vec2<f32>,
    @location(2) src_box: vec4<f32>,
    @location(3) dst_box: vec4<f32>,
    @location(4) params: vec4<f32>,
    @location(5) tex_origin: vec2<f32>,
    @location(6) tex_u: vec2<f32>,
    @location(7) tex_v: vec2<f32>,
    @location(8) color: vec4<f32>,
    @location(9) flags: u32,
};

struct SqueezeOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) box_uv: vec2<f32>,
    @location(1) @interpolate(flat) src_box: vec4<f32>,
    @location(2) @interpolate(flat) dst_box: vec4<f32>,
    @location(3) @interpolate(flat) params: vec4<f32>,
    @location(4) @interpolate(flat) tex_origin: vec2<f32>,
    @location(5) @interpolate(flat) tex_u: vec2<f32>,
    @location(6) @interpolate(flat) tex_v: vec2<f32>,
    @location(7) color: vec4<f32>,
    @location(8) @interpolate(flat) flags: u32,
};

@vertex
fn vs_squeeze(input: SqueezeInput) -> SqueezeOutput {
    var output: SqueezeOutput;
    output.position = vec4<f32>(input.position, 0.0, 1.0);
    output.box_uv = input.box_uv;
    output.src_box = input.src_box;
    output.dst_box = input.dst_box;
    output.params = input.params;
    output.tex_origin = input.tex_origin;
    output.tex_u = input.tex_u;
    output.tex_v = input.tex_v;
    output.color = input.color;
    output.flags = input.flags;
    return output;
}

// Where in the window, 0 to 1 with y up as Wayfire samples it, the warp
// shows at `uv`; outside 0..1 nothing is drawn.
fn squeeze_vertical(
    uv: vec2<f32>, src_box: vec4<f32>, dst_box: vec4<f32>, progress: f32,
    upward: bool,
) -> vec2<f32> {
    var squeeze: vec2<f32>;
    var sigmoid: f32;
    let inv_w = 1.0 / (src_box.z - src_box.x);
    let inv_h = 1.0 / (src_box.w - src_box.y);
    let one = clamp(progress, 0.0, 0.25) * 4.0;
    let progress_pt_one = one * one;
    let progress_pt_two = progress * progress;
    squeeze.x = inv_w * (uv.x - src_box.x);
    squeeze.y = inv_h * (uv.y - 1.0 + src_box.w);
    if (upward) {
        let y = uv.y;
        squeeze.y += -progress_pt_two * (inv_h - dst_box.w);
        sigmoid = 1.0 / (1.0 + pow(2.718,
            -(y * (1.0 / (src_box.w - dst_box.w)) * 15.0 - 10.0)));
    } else {
        let y = 1.0 - uv.y;
        squeeze.y -= -progress_pt_two * (inv_h - dst_box.y + dst_box.w);
        sigmoid = 1.0 / (1.0 + pow(2.718,
            -(y * (1.0 / (dst_box.w - src_box.y)) * 15.0 - 10.0)));
    }
    let t = sigmoid * progress_pt_one;
    let sx0 = mix(src_box.x, dst_box.x, t);
    let sx1 = mix(src_box.z, dst_box.z, t);
    squeeze.x = (uv.x - sx0) / (sx1 - sx0);
    return squeeze;
}

fn squeeze_horizontal(
    uv: vec2<f32>, src_box: vec4<f32>, dst_box: vec4<f32>, progress: f32,
    upward: bool,
) -> vec2<f32> {
    var squeeze: vec2<f32>;
    var sigmoid: f32;
    let inv_w = 1.0 / (src_box.z - src_box.x);
    let inv_h = 1.0 / (src_box.w - src_box.y);
    let one = clamp(progress, 0.0, 0.25) * 4.0;
    let progress_pt_one = one * one;
    let progress_pt_two = progress * progress;
    squeeze.x = inv_w * (uv.x - src_box.x);
    squeeze.y = inv_h * (1.0 - uv.y - src_box.y);
    if (upward) {
        let y = 1.0 - uv.x;
        squeeze.x += progress_pt_two * (inv_w - dst_box.z);
        sigmoid = 1.0 / (1.0 + pow(2.718,
            -(y * (1.0 / (src_box.z - dst_box.z)) * 15.0 - 10.0)));
    } else {
        let y = uv.x;
        squeeze.x -= progress_pt_two * (inv_w - dst_box.x + dst_box.z);
        sigmoid = 1.0 / (1.0 + pow(2.718,
            -(y * (1.0 / (dst_box.z - src_box.x)) * 15.0 - 10.0)));
    }
    let t = sigmoid * progress_pt_one;
    let sy0 = mix(src_box.y, 1.0 - dst_box.w, t);
    let sy1 = mix(src_box.w, 1.0 - dst_box.y, t);
    squeeze.y = (uv.y - sy1) / (sy0 - sy1);
    squeeze.y = 1.0 - squeeze.y;
    return squeeze;
}

@fragment
fn fs_squeeze(input: SqueezeOutput) -> @location(0) vec4<f32> {
    let progress = input.params.x;
    let upward = input.params.y > 0.5;
    var squeeze: vec2<f32>;
    if (input.params.z > 0.5) {
        squeeze = squeeze_horizontal(
            input.box_uv, input.src_box, input.dst_box, progress, upward);
    } else {
        squeeze = squeeze_vertical(
            input.box_uv, input.src_box, input.dst_box, progress, upward);
    }
    // Wayfire's textures have y up; the window's top row is at 1.
    let window = vec2<f32>(squeeze.x, 1.0 - squeeze.y);
    let uv = input.tex_origin + window.x * input.tex_u + window.y * input.tex_v;
    // Sampled before the discard, which makes the rest non-uniform.
    let texel = textureSample(tile_texture, tile_sampler, uv);
    if (squeeze.x < 0.0 || squeeze.y < 0.0 || squeeze.x > 1.0 || squeeze.y > 1.0) {
        discard;
    }
    return shade(texel, input.color, input.flags);
}

// Wayfire's fire particles (wayfire:plugins/animate/fire/shaders.hpp):
// a quad of 1.5 by 2 radii around the centre, faded from the middle out.
struct ParticleInput {
    @builtin(vertex_index) index: u32,
    @location(0) center: vec2<f32>,
    @location(1) axis_x: vec2<f32>,
    @location(2) axis_y: vec2<f32>,
    @location(3) radius: f32,
    @location(4) color: vec4<f32>,
};

struct ParticleOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) radius: f32,
};

@vertex
fn vs_particle(input: ParticleInput) -> ParticleOutput {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0));
    let uv = corners[input.index] * input.radius;
    var output: ParticleOutput;
    let position =
        input.center + input.axis_x * (uv.x * 0.75) + input.axis_y * uv.y;
    output.position = vec4<f32>(position, 0.0, 1.0);
    output.uv = uv;
    output.color = input.color;
    output.radius = input.radius;
    return output;
}

fn particle(input: ParticleOutput, smoothing: f32, color: vec4<f32>) -> vec4<f32> {
    let len = length(input.uv);
    if (len >= input.radius) {
        return vec4<f32>(0.0);
    }
    let factor = pow(1.0 - len / input.radius, smoothing);
    return factor * color;
}

// The first pass darkens what is below by half the particle's alpha.
@fragment
fn fs_particle_darken(input: ParticleOutput) -> @location(0) vec4<f32> {
    return particle(input, 0.7, input.color * 0.5);
}

// The second adds its colour.
@fragment
fn fs_particle_glow(input: ParticleOutput) -> @location(0) vec4<f32> {
    let color = particle(input, 0.5, input.color);
    let shown = post_process(vec4<f32>(min(color.rgb, vec3<f32>(1.0)), 1.0));
    return vec4<f32>(shown.rgb, color.a);
}
