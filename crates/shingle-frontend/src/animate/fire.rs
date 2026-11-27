//! The particle system of Wayfire's fire animation
//! (wayfire:plugins/animate/fire/particle.cpp, fire.cpp), on the CPU as
//! Wayfire runs it, with the particles drawn by the renderer
//! ([`shingle_render::TileEffect::Particles`]).

use shingle_render::Particle as DrawnParticle;

/// The `[animate]` options the particles take.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FireOptions {
  /// `fire_particles`: particles of a 400 pixel wide view.
  pub particles: u32,
  /// `fire_particle_size`: the mean radius.
  pub particle_size: f64,
  /// `random_fire_color`.
  pub random_color: bool,
  /// `fire_color`, sRGB components 0 to 1.
  pub color: [f64; 4],
}

impl FireOptions {
  /// The options of `config`.
  pub fn of(config: &shingle_config::AnimateConfig) -> Self {
    Self {
      particles: config.fire_particles,
      particle_size: config.fire_particle_size,
      random_color: config.random_fire_color,
      color: config.fire_color,
    }
  }
}

/// A small deterministic generator in place of `std::rand`, so that a
/// test can fix the particles.
#[derive(Clone, Debug)]
pub(crate) struct Random(u64);

impl Random {
  /// A generator started from `seed`.
  pub(crate) fn new(seed: u64) -> Self {
    Self(seed.max(1))
  }

  /// Wayfire's `random(s, e)`: `s * r + (1 - r) * e` for a uniform `r` in
  /// 0..=1 (wayfire:plugins/animate/fire/fire.cpp:50-55).
  pub(crate) fn between(&mut self, start: f32, end: f32) -> f32 {
    // xorshift64*.
    self.0 ^= self.0 >> 12;
    self.0 ^= self.0 << 25;
    self.0 ^= self.0 >> 27;
    let value = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 40;
    let r = value as f64 / ((1u64 << 24) - 1) as f64;
    (f64::from(start) * r + (1.0 - r) * f64::from(end)) as f32
  }
}

/// One particle (wayfire:plugins/animate/fire/particle.hpp:20-34).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Particle {
  pub(crate) life: f32,
  pub(crate) fade: f32,
  pub(crate) radius: f32,
  pub(crate) base_radius: f32,
  pub(crate) pos: [f32; 2],
  pub(crate) speed: [f32; 2],
  pub(crate) g: [f32; 2],
  pub(crate) start_pos: [f32; 2],
  pub(crate) color: [f32; 4],
}

impl Default for Particle {
  fn default() -> Self {
    Self {
      life: -1.0,
      fade: 0.0,
      radius: 0.0,
      base_radius: 0.0,
      pos: [0.0; 2],
      speed: [0.0; 2],
      g: [0.0; 2],
      start_pos: [0.0; 2],
      color: [1.0; 4],
    }
  }
}

impl Particle {
  /// One frame of the particle's life, which Wayfire runs once per frame
  /// whatever the time between them
  /// (wayfire:plugins/animate/fire/particle.cpp:5-39).
  pub(crate) fn update(&mut self) {
    if self.life <= 0.0 {
      return;
    }
    let slowdown = 0.8f32;
    for axis in 0..2 {
      self.pos[axis] += self.speed[axis] * 0.2 * slowdown;
      self.speed[axis] += self.g[axis] * 0.3 * slowdown;
    }
    if self.life != 0.0 {
      self.color[3] /= self.life;
    }
    self.life -= (f64::from(self.fade) * 0.3 * f64::from(slowdown)) as f32;
    self.radius = self.base_radius * self.life.powf(0.5);
    self.color[3] *= self.life;
    self.g[0] = if self.start_pos[0] < self.pos[0] {
      -1.0
    } else {
      1.0
    };
    if self.life <= 0.0 {
      self.pos = [-10000.0, -10000.0];
    }
  }

  /// A new particle on the progress line, `line` of the way down a view
  /// of `size` (wayfire:plugins/animate/fire/fire.cpp:78-127).
  pub(crate) fn spawn(
    random: &mut Random,
    options: &FireOptions,
    size: (f64, f64),
    line: f64,
  ) -> Self {
    let fade = random.between(0.1, 0.6);
    let [r, g, b] = if options.random_color {
      let mut pick = || 2.0 * random.between(0.0, 1.0).powi(16);
      [pick(), pick(), pick()]
    } else {
      // The variation is lower at darker values.
      let mut pick = |component: f64| {
        let amount = (component * 0.857) / 2.0;
        random.between(
          (component - amount) as f32,
          (component + amount).min(1.0) as f32,
        )
      };
      [
        pick(options.color[0]),
        pick(options.color[1]),
        pick(options.color[2]),
      ]
    };
    let current = size.1 * line;
    let pos = [
      random.between(0.0, size.0 as f32),
      random.between((current - 10.0) as f32, (current + 10.0) as f32),
    ];
    let speed = [random.between(-10.0, 10.0), random.between(-25.0, 5.0)];
    let radius = random.between(
      (options.particle_size * 0.8) as f32,
      (options.particle_size * 1.2) as f32,
    );
    Self {
      life: 1.0,
      fade,
      radius,
      base_radius: radius,
      pos,
      speed,
      g: [-1.0, -3.0],
      start_pos: pos,
      color: [r, g, b, 1.0],
    }
  }
}

/// Wayfire's `ParticleSystem` (wayfire:plugins/animate/fire/particle.cpp).
#[derive(Clone, Debug)]
pub(crate) struct ParticleSystem {
  pub(crate) particles: Vec<Particle>,
}

impl ParticleSystem {
  /// A system of `count` dead particles.
  pub(crate) fn new(count: usize) -> Self {
    Self {
      particles: vec![Particle::default(); count],
    }
  }

  /// Brings up to `count` dead particles to life with `init`.
  pub(crate) fn spawn(
    &mut self,
    count: usize,
    mut init: impl FnMut() -> Particle,
  ) {
    let mut spawned = 0;
    for particle in &mut self.particles {
      if spawned >= count {
        break;
      }
      if particle.life <= 0.0 {
        *particle = init();
        spawned += 1;
      }
    }
  }

  /// Keeps `count` particles, killing the ones beyond it.
  pub(crate) fn resize(&mut self, count: usize) {
    self.particles.resize(count, Particle::default());
  }

  /// Runs one frame of every particle.
  pub(crate) fn update(&mut self) {
    for particle in &mut self.particles {
      particle.update();
    }
  }

  /// The particles alive.
  pub(crate) fn alive(&self) -> usize {
    self
      .particles
      .iter()
      .filter(|particle| particle.life > 0.0)
      .count()
  }

  /// The live particles to draw, with their colour decoded to linear for
  /// the renderer, and the darkening of their colour left to its shader.
  pub(crate) fn drawn(&self) -> Vec<DrawnParticle> {
    self
      .particles
      .iter()
      .filter(|particle| particle.life > 0.0)
      .map(|particle| DrawnParticle {
        center: particle.pos,
        radius: particle.radius,
        color: [
          srgb_to_linear(particle.color[0]),
          srgb_to_linear(particle.color[1]),
          srgb_to_linear(particle.color[2]),
          particle.color[3],
        ],
      })
      .collect()
  }
}

/// Particles of a view `width` wide: `fire_particles` per 400 pixels, at
/// most 3.5 times it (wayfire:plugins/animate/fire/fire.cpp:57-62).
pub(crate) fn particle_count_for_width(
  options: &FireOptions,
  width: f64,
) -> usize {
  (f64::from(options.particles) * (width / 400.0).min(3.5)).max(0.0) as usize
}

/// The factor of the fire's length for a view `height` tall
/// (wayfire:plugins/animate/fire/fire.cpp:268-271).
pub(crate) fn duration_factor_for_height(height: f64) -> f64 {
  (height / 400.0).min(3.0)
}

/// sRGB to linear, continued above 1 for random colours, which reach 2.
fn srgb_to_linear(value: f32) -> f32 {
  if value <= 0.04045 {
    value / 12.92
  } else {
    ((value + 0.055) / 1.055).powf(2.4)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn options() -> FireOptions {
    FireOptions::of(&shingle_config::AnimateConfig::default())
  }

  #[test]
  fn a_particle_moves_fades_and_dies_as_wayfires() {
    let mut particle = Particle {
      life: 1.0,
      fade: 0.5,
      radius: 10.0,
      base_radius: 10.0,
      pos: [0.0, 0.0],
      speed: [5.0, -10.0],
      g: [-1.0, -3.0],
      start_pos: [0.0, 0.0],
      color: [1.0, 0.5, 0.0, 1.0],
    };
    particle.update();
    // pos += speed * 0.16, then speed += g * 0.24.
    assert!((particle.pos[0] - 0.8).abs() < 1e-6);
    assert!((particle.pos[1] + 1.6).abs() < 1e-6);
    assert!((particle.speed[0] - 4.76).abs() < 1e-5);
    assert!((particle.speed[1] + 10.72).abs() < 1e-5);
    // life -= fade * 0.24, radius follows its root, alpha the life.
    assert!((particle.life - 0.88).abs() < 1e-6);
    assert!((particle.radius - 10.0 * 0.88f32.sqrt()).abs() < 1e-5);
    assert!((particle.color[3] - 0.88).abs() < 1e-6);
    // Right of where it started, gravity pulls it back left.
    assert_eq!(particle.g[0], -1.0);
    for _ in 0..10 {
      particle.update();
    }
    assert!(particle.life <= 0.0);
    assert_eq!(particle.pos, [-10000.0, -10000.0]);
  }

  #[test]
  fn spawned_particles_sit_on_the_line_in_the_fire_colour() {
    let options = options();
    let mut random = Random::new(7);
    for _ in 0..200 {
      let particle =
        Particle::spawn(&mut random, &options, (400.0, 300.0), 0.5);
      assert!((0.0..=400.0).contains(&particle.pos[0]));
      assert!((140.0..=160.0).contains(&particle.pos[1]));
      assert!((12.8..=19.2).contains(&particle.radius));
      assert!((0.1..=0.6).contains(&particle.fade));
      // #B22303: red 0.698 varies by 0.299 either way.
      let red = f64::from(particle.color[0]);
      assert!((0.698 - 0.300..=0.698 + 0.300).contains(&red), "{red}");
      assert!(particle.color[1] < 0.3);
    }
  }

  #[test]
  fn the_system_spawns_into_dead_slots_and_resizes() {
    let mut system = ParticleSystem::new(1);
    system.resize(10);
    assert_eq!(system.alive(), 0);
    let mut random = Random::new(1);
    let options = options();
    system.spawn(4, || {
      Particle::spawn(&mut random, &options, (100.0, 100.0), 0.0)
    });
    assert_eq!(system.alive(), 4);
    assert_eq!(system.drawn().len(), 4);
    system.resize(2);
    assert_eq!(system.alive(), 2);
    assert_eq!(particle_count_for_width(&options, 200.0), 1000);
    assert_eq!(particle_count_for_width(&options, 4000.0), 7000);
    assert_eq!(duration_factor_for_height(200.0), 0.5);
    assert_eq!(duration_factor_for_height(2000.0), 3.0);
  }
}
