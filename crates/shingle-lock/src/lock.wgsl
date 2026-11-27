// shingle-lock's passes, in linear light and 16-bit float until the
// last: a separable blur of the screenshot, done once; per frame the
// background (the blurred screenshot slowly drifting and flowing, its
// light slowly shifting, dimmed and vignetted) and the clock and input
// overlays, into a scene; and the present pass, which encodes the scene
// as sRGB bytes with a fine dither into the buffer the compositor shows.

struct VsOut {
  @builtin(position) position: vec4<f32>,
  @location(0) uv: vec2<f32>,
};

// A triangle that covers the target, uv 0 to 1 from the top left.
@vertex
fn vs_full(@builtin(vertex_index) index: u32) -> VsOut {
  let x = f32((index << 1u) & 2u);
  let y = f32(index & 2u);
  var out: VsOut;
  out.position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
  out.uv = vec2<f32>(x, y);
  return out;
}

fn to_linear(c: vec3<f32>) -> vec3<f32> {
  let low = c / 12.92;
  let high = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
  return select(high, low, c <= vec3<f32>(0.04045));
}

fn to_srgb(c: vec3<f32>) -> vec3<f32> {
  let v = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
  let low = v * 12.92;
  let high = 1.055 * pow(v, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
  return select(high, low, v <= vec3<f32>(0.0031308));
}

struct Blur {
  // The step between taps in texels, (1, 0) or (0, 1); radius in texels.
  direction: vec2<f32>,
  radius: f32,
  pad: f32,
};

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> blur: Blur;

// Read through an sRGB view the capture arrives linear; a radius of 0
// copies (and scales) it.
@fragment
fn fs_blur(in: VsOut) -> @location(0) vec4<f32> {
  let size = vec2<f32>(textureDimensions(source));
  let step = blur.direction / size;
  let taps = i32(ceil(blur.radius));
  let sigma = max(blur.radius / 2.0, 0.5);
  var sum = vec3<f32>(0.0);
  var weight = 0.0;
  for (var k = -taps; k <= taps; k = k + 1) {
    let distance = f32(k);
    let w = exp(-distance * distance / (2.0 * sigma * sigma));
    sum = sum + textureSampleLevel(source, linear_sampler,
      in.uv + step * distance, 0.0).rgb * w;
    weight = weight + w;
  }
  return vec4<f32>(sum / weight, 1.0);
}

struct Background {
  // Drift offset in uv, zoom, and whether there is an image.
  transform: vec4<f32>,
  // Vignette base and factor, whether it is on, the light's strength.
  vignette: vec4<f32>,
  // The background colour, linear, and the brightness of the image.
  color: vec4<f32>,
  // The time in seconds, the image's cover scale in yz, and the flow's
  // strength in uv.
  motion: vec4<f32>,
};

@group(0) @binding(0) var image: texture_2d<f32>;
@group(0) @binding(1) var image_sampler: sampler;
@group(0) @binding(2) var<uniform> background: Background;

// A slow, smooth flow: two low-frequency waves, each turning at its own
// pace, bend the image's coordinates a fraction of a percent.
fn flow(uv: vec2<f32>, t: f32) -> vec2<f32> {
  let a = sin(uv.y * 3.1 + t * 0.21 + sin(uv.x * 1.7 + t * 0.13) * 1.3);
  let b = sin(uv.x * 2.7 - t * 0.17 + sin(uv.y * 2.3 - t * 0.11) * 1.1);
  return vec2<f32>(a, b);
}

@fragment
fn fs_background(in: VsOut) -> @location(0) vec4<f32> {
  var rgb = background.color.rgb;
  let t = background.motion.x;
  if (background.transform.w > 0.5) {
    var uv = (in.uv - vec2<f32>(0.5)) * background.motion.yz
      / background.transform.z + vec2<f32>(0.5) + background.transform.xy;
    uv = uv + flow(in.uv, t) * background.motion.w;
    rgb = textureSampleLevel(image, image_sampler, uv, 0.0).rgb
      * background.color.w;
  }
  if (background.vignette.z > 0.5) {
    // swaylock-effects' effect_vignette, on the encoded value as it
    // computes it, applied in linear light.
    let x = in.uv.x;
    let y = in.uv.y;
    let factor = clamp(background.vignette.x
      + background.vignette.y * 16.0 * x * y * (1.0 - x) * (1.0 - y),
      0.0, 1.0);
    rgb = rgb * pow(factor, 2.2);
  }
  // A broad, soft light drifting slowly across.
  let centre = vec2<f32>(0.5 + 0.35 * sin(t * 0.047),
    0.5 + 0.3 * sin(t * 0.061 + 1.3));
  let d = distance(in.uv, centre);
  let light = 1.0 + background.vignette.w * (exp(-d * d * 3.0) * 2.0 - 1.0);
  return vec4<f32>(rgb * light, 1.0);
}

struct Overlay {
  // x0, y0, x1, y1 in clip space.
  rect: vec4<f32>,
  // The overlay's opacity in x.
  alpha: vec4<f32>,
};

@group(0) @binding(0) var overlay_texture: texture_2d<f32>;
@group(0) @binding(1) var overlay_sampler: sampler;
@group(0) @binding(2) var<uniform> overlay: Overlay;

@vertex
fn vs_overlay(@builtin(vertex_index) index: u32) -> VsOut {
  var corners = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
    vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0));
  let corner = corners[index];
  var out: VsOut;
  out.position = vec4<f32>(
    mix(overlay.rect.x, overlay.rect.z, corner.x),
    mix(overlay.rect.y, overlay.rect.w, corner.y), 0.0, 1.0);
  out.uv = corner;
  return out;
}

// The overlay images are premultiplied sRGB; blended premultiplied in
// linear light.
@fragment
fn fs_overlay(in: VsOut) -> @location(0) vec4<f32> {
  let texel = textureSampleLevel(overlay_texture, overlay_sampler, in.uv,
    0.0);
  let alpha = texel.a * overlay.alpha.x;
  if (texel.a <= 0.0) {
    return vec4<f32>(0.0);
  }
  return vec4<f32>(to_linear(texel.rgb / texel.a) * alpha, alpha);
}

struct Present {
  // The output's levels per channel less one (255 or 1023), the frame's
  // dither seed, whether to dither, and whether the output is an sRGB
  // view that encodes what it is given.
  levels: vec4<f32>,
};

@group(0) @binding(0) var scene: texture_2d<f32>;
@group(0) @binding(1) var scene_sampler: sampler;
@group(0) @binding(2) var<uniform> present: Present;

// Interleaved gradient noise (Jimenez), shifted each frame, from 0 to 1.
fn noise(p: vec2<f32>, seed: f32) -> f32 {
  let q = p + vec2<f32>(5.588238, 5.588238) * seed;
  return fract(52.9829189 * fract(dot(q, vec2<f32>(0.06711056,
    0.00583715))));
}

@fragment
fn fs_present(in: VsOut) -> @location(0) vec4<f32> {
  let texel = textureLoad(scene, vec2<i32>(in.position.xy), 0);
  let levels = present.levels.x;
  let encoded = to_srgb(texel.rgb);
  // About half a step either way, a different grain for each channel.
  let seed = present.levels.y;
  let dither = vec3<f32>(noise(in.position.xy, seed),
    noise(in.position.xy + vec2<f32>(17.0, 59.0), seed),
    noise(in.position.xy + vec2<f32>(83.0, 31.0), seed)) - vec3<f32>(0.5);
  let value = clamp(encoded + dither * present.levels.z / levels,
    vec3<f32>(0.0), vec3<f32>(1.0));
  if (present.levels.w > 0.5) {
    // The view encodes again what it is given: the dithered value comes
    // out as computed.
    return vec4<f32>(to_linear(value), 1.0);
  }
  return vec4<f32>(value, 1.0);
}
