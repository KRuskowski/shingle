// shingle-lock's passes: a separable blur of the screenshot, done once,
// the background of every frame (the blurred screenshot drifting, the
// vignette and a faint shimmer), and textured overlays for the clock
// and the password input. Colours are the bytes swaylock-effects works
// on, without sRGB conversion.

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

struct Blur {
  // The step between taps in texels, (1, 0) or (0, 1); radius in texels.
  direction: vec2<f32>,
  radius: f32,
  pad: f32,
};

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var linear: sampler;
@group(0) @binding(2) var<uniform> blur: Blur;

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
    sum = sum + textureSampleLevel(source, linear, in.uv + step * distance,
      0.0).rgb * w;
    weight = weight + w;
  }
  return vec4<f32>(sum / weight, 1.0);
}

struct Background {
  // Drift offset in uv, zoom, and whether there is an image.
  transform: vec4<f32>,
  // Vignette base and factor, whether it is on, shimmer strength.
  vignette: vec4<f32>,
  // The background colour.
  color: vec4<f32>,
  // The shimmer's phase, and the image's cover scale in yz.
  shimmer: vec4<f32>,
};

@group(0) @binding(0) var image: texture_2d<f32>;
@group(0) @binding(1) var image_sampler: sampler;
@group(0) @binding(2) var<uniform> background: Background;

@fragment
fn fs_background(in: VsOut) -> @location(0) vec4<f32> {
  var rgb = background.color.rgb;
  if (background.transform.w > 0.5) {
    let uv = (in.uv - vec2<f32>(0.5)) * background.shimmer.yz
      / background.transform.z + vec2<f32>(0.5) + background.transform.xy;
    rgb = textureSampleLevel(image, image_sampler, uv, 0.0).rgb;
  }
  if (background.vignette.z > 0.5) {
    // swaylock-effects' effect_vignette.
    let x = in.uv.x;
    let y = in.uv.y;
    rgb = rgb * (background.vignette.x
      + background.vignette.y * 16.0 * x * y * (1.0 - x) * (1.0 - y));
  }
  let phase = background.shimmer.x;
  let wave = sin(phase + in.uv.x * 2.3 + in.uv.y * 1.7)
    * sin(phase * 0.61 + in.uv.y * 2.9 - in.uv.x * 0.8);
  rgb = rgb * (1.0 + background.vignette.w * wave);
  return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
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

@fragment
fn fs_overlay(in: VsOut) -> @location(0) vec4<f32> {
  return textureSampleLevel(overlay_texture, overlay_sampler, in.uv, 0.0)
    * overlay.alpha.x;
}
