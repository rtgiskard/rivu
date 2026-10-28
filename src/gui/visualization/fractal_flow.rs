//! Deterministic, layered streamlines through a three-scale curl field.
//!
//! Audio bands deform distinct spatial scales; the onset input is an energy-rise
//! pulse, not a beat clock. History and render storage stay bounded for the
//! entire playback session; the bounded display clock advances held audio targets.

use super::fractal_visualizer::{Drive, LightSegment};
use std::f32::consts::TAU;

const PARTICLES: usize = 96;
const TRAIL_POINTS: usize = 24;
// Reserve one of the 24 points for the continuously moving head.
const HISTORY_POINTS: usize = TRAIL_POINTS - 1;
const MAX_SEGMENTS: usize = PARTICLES * (TRAIL_POINTS - 1);
const SAMPLE_INTERVAL: f32 = 1.0 / 40.0;
const SEED: u32 = 0x61c8_4f23;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct TrailPoint {
    position: [f32; 2],
    light: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Particle {
    position: [f32; 2],
    history: [TrailPoint; HISTORY_POINTS],
    first: usize,
    count: usize,
    sample_time: f32,
    generation: u32,
    tone: usize,
}

impl Particle {
    fn seeded(index: usize, generation: u32) -> Self {
        // Stratified initial coverage, with repeatable jitter rather than a
        // clump at the origin. Respawns use the same deterministic seed family.
        let seed = SEED
            .wrapping_add((index as u32).wrapping_mul(0x9e37_79b9))
            .wrapping_add(generation.wrapping_mul(0x85eb_ca6b));
        let position = if generation == 0 {
            [
                -0.9 + ((index % 12) as f32 + unit_hash(seed)) * 1.8 / 12.0,
                -0.9 + ((index / 12) as f32 + unit_hash(seed ^ 0xa511_e9b3)) * 1.8 / 8.0,
            ]
        } else {
            [
                (unit_hash(seed) * 2.0 - 1.0) * 0.88,
                (unit_hash(seed ^ 0xa511_e9b3) * 2.0 - 1.0) * 0.88,
            ]
        };
        let mut particle = Self {
            position,
            history: [TrailPoint::default(); HISTORY_POINTS],
            first: 0,
            count: 1,
            sample_time: 0.0,
            generation,
            tone: index % 3,
        };
        particle.history[0].position = position;
        particle
    }

    fn append(&mut self, light: f32) {
        let next = (self.first + self.count) % HISTORY_POINTS;
        self.history[next] = TrailPoint {
            position: self.position,
            light,
        };
        if self.count == HISTORY_POINTS {
            self.first = (self.first + 1) % HISTORY_POINTS;
        } else {
            self.count += 1;
        }
    }
}

pub(super) struct Scene {
    // One fixed history allocation on style selection, never copied with scene control state.
    particles: Box<[Particle; PARTICLES]>,
    phases: [f32; 3],
    energy: f32,
    pulse: f32,
    head_light: f32,
    segments: Vec<LightSegment>,
}

impl Scene {
    pub(super) fn new() -> Self {
        Self {
            particles: Box::new(std::array::from_fn(|index| Particle::seeded(index, 0))),
            phases: [0.0; 3],
            energy: 0.0,
            pulse: 0.0,
            head_light: 0.0,
            segments: Vec::with_capacity(MAX_SEGMENTS),
        }
    }

    #[cfg(test)]
    pub(super) fn reset(&mut self) {
        for (index, particle) in self.particles.iter_mut().enumerate() {
            *particle = Particle::seeded(index, 0);
        }
        self.phases = [0.0; 3];
        self.energy = 0.0;
        self.pulse = 0.0;
        self.head_light = 0.0;
        self.segments.clear();
    }

    // Integrate every bounded substep; the native-frame owner publishes geometry
    // with rebuild_segments only after the final step. No substep reads segments.
    pub(super) fn advance(&mut self, dt: f32, drive: Drive) {
        if dt <= 0.0 {
            return;
        }

        let release = if drive.energy > self.energy {
            24.0
        } else {
            4.0
        };
        self.energy += (drive.energy - self.energy) * (1.0 - (-release * dt).exp());
        self.pulse = (self.pulse * (-6.0 * dt).exp()).max(drive.onset);
        self.head_light = (self.energy * (0.42 + 0.58 * self.energy) + self.pulse * 0.32).min(1.0);
        let history_fade = (-2.5 * dt).exp();

        // Low: broad vortices, mid: branching bends, high: fine ripples.
        // Each also changes its own evolution rate, not merely the palette.
        let weights = [
            0.25 + 0.70 * drive.bands[0],
            0.10 + 0.40 * drive.bands[1],
            0.035 + 0.17 * drive.bands[2],
        ];
        let rates = [0.12, 0.21, 0.34];
        for (scale, phase) in self.phases.iter_mut().enumerate() {
            *phase =
                (*phase + dt * rates[scale] * (1.0 + 2.0 * drive.bands[scale])).rem_euclid(TAU);
        }
        let speed = 0.28 + 0.42 * drive.bands[0] + 0.28 * drive.bands[1] + 0.18 * drive.bands[2];
        let drift = [0.10 * self.phases[0].cos(), 0.07 * self.phases[0].sin()];
        for (index, particle) in self.particles.iter_mut().enumerate() {
            for offset in 0..particle.count {
                let point = &mut particle.history[(particle.first + offset) % HISTORY_POINTS];
                point.light *= history_fade;
            }
            let velocity = flow(
                particle.position,
                particle.tone,
                self.phases,
                weights,
                drift,
            );
            let travel = dt * speed * (0.85 + particle.tone as f32 * 0.075);
            let next = [
                particle.position[0] + velocity[0] * travel,
                particle.position[1] + velocity[1] * travel,
            ];
            if next.iter().any(|coordinate| coordinate.abs() > 1.0) {
                // Never connect opposite edges (or a new seed) with a trail.
                *particle = Particle::seeded(index, particle.generation.wrapping_add(1));
                particle.history[0].light = self.head_light;
                continue;
            }
            particle.position = next;
            particle.sample_time += dt;
            // Parent provides steps <= 1/120 s, so this appends at most once.
            if particle.sample_time >= SAMPLE_INTERVAL {
                particle.sample_time -= SAMPLE_INTERVAL;
                particle.append(self.head_light);
            }
        }
    }

    pub(super) fn segments(&self) -> &[LightSegment] {
        &self.segments
    }

    pub(super) fn rebuild_segments(&mut self) {
        self.segments.clear();
        for particle in self.particles.iter() {
            let mut from = particle.history[particle.first];
            for offset in 1..=particle.count {
                let to = if offset == particle.count {
                    TrailPoint {
                        position: particle.position,
                        light: self.head_light,
                    }
                } else {
                    particle.history[(particle.first + offset) % HISTORY_POINTS]
                };
                // Old tails taper, while an energy-rise pulse briefly lights
                // the whole retained filament without scheduling beat events.
                let taper = 0.30 + 0.70 * offset as f32 / particle.count as f32;
                let strength =
                    ((from.light + to.light) * 0.5 * taper * (1.0 + 0.85 * self.pulse)).min(1.0);
                if strength > 0.001 && from.position != to.position {
                    self.segments.push(LightSegment {
                        from: from.position,
                        to: to.position,
                        strength,
                        tone: particle.tone,
                    });
                }
                from = to;
            }
        }
    }
}

fn unit_hash(mut value: u32) -> f32 {
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^= value >> 16;
    (value >> 8) as f32 / 16_777_216.0
}

fn flow(
    position: [f32; 2],
    tone: usize,
    phases: [f32; 3],
    weights: [f32; 3],
    drift: [f32; 2],
) -> [f32; 2] {
    let mut velocity = drift;
    let frequencies = [1.7, 3.8, 8.4];
    let layer = (tone as f32 - 1.0) * 0.08;
    for scale in 0..3 {
        let x = position[0] * frequencies[scale] + phases[scale] + layer;
        let y = position[1] * frequencies[scale] - phases[scale] * 0.7 + layer;
        let (sin_x, cos_x) = x.sin_cos();
        let (sin_y, cos_y) = y.sin_cos();
        // Curl of a sinusoidal stream function: nearby particles follow
        // related bends instead of wandering as independent random points.
        velocity[0] += weights[scale] * sin_x * cos_y;
        velocity[1] -= weights[scale] * cos_x * sin_y;
    }
    let length_squared = velocity[0] * velocity[0] + velocity[1] * velocity[1];
    if length_squared > 1.0 {
        let inverse_length = length_squared.sqrt().recip();
        velocity[0] *= inverse_length;
        velocity[1] *= inverse_length;
    }
    velocity
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEP: f32 = 1.0 / 120.0;

    fn active() -> Drive {
        Drive {
            bands: [0.8, 0.6, 0.9],
            energy: 0.75,
            onset: 0.35,
        }
    }

    fn assert_same(left: &Scene, right: &Scene) {
        assert_eq!(left.particles, right.particles);
        assert_eq!(left.phases, right.phases);
        assert_eq!(left.energy, right.energy);
        assert_eq!(left.pulse, right.pulse);
        assert_eq!(left.head_light, right.head_light);
        assert_eq!(left.segments.len(), right.segments.len());
        for (left, right) in left.segments.iter().zip(&right.segments) {
            assert_eq!(left.from, right.from);
            assert_eq!(left.to, right.to);
            assert_eq!(left.strength, right.strength);
            assert_eq!(left.tone, right.tone);
        }
    }

    #[test]
    fn frame_batches_preserve_every_substep_and_final_segments() {
        let mut batched = Scene::new();
        let mut eager = Scene::new();
        // Vary batch length and inputs, retaining enough motion to exercise respawns.
        for frame in 0..360 {
            let steps = [1, 2, 6, 30, 60][frame % 5];
            for step in 0..steps {
                let drive = Drive {
                    bands: [0.8, (frame % 73) as f32 / 72.0, 0.9],
                    energy: if frame % 11 == 0 { 0.0 } else { 0.75 },
                    onset: if step == 0 { 0.35 } else { 0.0 },
                };
                batched.advance(STEP, drive);
                eager.advance(STEP, drive);
                eager.rebuild_segments();
            }
            batched.rebuild_segments();
            assert_same(&batched, &eager);
        }
        assert!(
            batched
                .particles
                .iter()
                .any(|particle| particle.generation > 0)
        );
    }

    #[test]
    fn zero_time_preserves_entire_scene_even_with_changed_drive() {
        let mut scene = Scene::new();
        let mut reference = Scene::new();
        for _ in 0..180 {
            scene.advance(STEP, active());
            reference.advance(STEP, active());
        }
        scene.rebuild_segments();
        reference.rebuild_segments();
        scene.advance(0.0, Drive::default());
        scene.advance(
            0.0,
            Drive {
                onset: 1.0,
                ..active()
            },
        );
        assert_same(&scene, &reference);
    }

    #[test]
    fn reset_replays_seeded_motion_and_reuses_storage() {
        let mut scene = Scene::new();
        let mut reference = Scene::new();
        let storage = scene.segments.as_ptr();
        for _ in 0..480 {
            scene.advance(STEP, active());
        }
        scene.rebuild_segments();
        scene.reset();
        assert_same(&scene, &reference);
        assert_eq!(scene.segments.as_ptr(), storage);
        for step in 0..300 {
            let drive = if step % 17 == 0 {
                active()
            } else {
                Drive::default()
            };
            scene.advance(STEP, drive);
            reference.advance(STEP, drive);
            scene.rebuild_segments();
            reference.rebuild_segments();
            assert_same(&scene, &reference);
        }
        assert_eq!(scene.segments.as_ptr(), storage);
    }

    #[test]
    fn long_run_stays_finite_connected_and_bounded() {
        let mut scene = Scene::new();
        let capacity = scene.segments.capacity();
        let storage = scene.segments.as_ptr();
        for step in 0..12_000 {
            let drive = Drive {
                bands: [
                    (step % 101) as f32 / 100.0,
                    (step % 73) as f32 / 72.0,
                    (step % 47) as f32 / 46.0,
                ],
                energy: (step % 89) as f32 / 88.0,
                onset: if step % 137 == 0 { 1.0 } else { 0.0 },
            };
            scene.advance(STEP, drive);
            scene.rebuild_segments();
            assert!(scene.segments.len() <= MAX_SEGMENTS);
            for particle in scene.particles.iter() {
                assert!((1..=HISTORY_POINTS).contains(&particle.count));
                for coordinate in particle.position {
                    assert!(coordinate.is_finite() && (-1.0..=1.0).contains(&coordinate));
                }
            }
            for segment in scene.segments() {
                assert!(segment.strength.is_finite());
                assert!((0.0..=1.0).contains(&segment.strength));
                assert!(segment.tone < 3);
                for coordinate in segment.from.into_iter().chain(segment.to) {
                    assert!(coordinate.is_finite() && (-1.0..=1.0).contains(&coordinate));
                }
                let dx = segment.to[0] - segment.from[0];
                let dy = segment.to[1] - segment.from[1];
                assert!(dx * dx + dy * dy < 0.01, "disconnected trail segment");
            }
        }
        assert_eq!(scene.segments.capacity(), capacity);
        assert_eq!(scene.segments.as_ptr(), storage);
    }

    #[test]
    fn boundary_respawn_discards_old_trail_before_rendering() {
        let mut scene = Scene::new();
        // At this edge the fine-scale field has an outward x component.
        // Evaluate that deterministic field rather than depending on a
        // naturally occurring crossing at a particular playback timestamp.
        let weights = [0.25, 0.10, 0.205];
        let position = [1.0, 0.0];
        assert!(flow(position, 0, [0.001, 0.00175, 0.0085], weights, [0.1, 0.0])[0] > 0.0);
        let particle = &mut scene.particles[0];
        particle.position = position;
        particle.count = HISTORY_POINTS;
        particle.history.fill(TrailPoint {
            position: [-0.8, 0.8],
            light: 1.0,
        });
        scene.advance(
            STEP,
            Drive {
                bands: [0.0, 0.0, 1.0],
                energy: 1.0,
                onset: 0.0,
            },
        );
        scene.rebuild_segments();
        let particle = &scene.particles[0];
        assert_eq!(particle.generation, 1);
        assert_eq!(particle.count, 1);
        assert_eq!(particle.history[particle.first].position, particle.position);
        assert!(
            scene
                .segments()
                .iter()
                .all(|segment| segment.from != [-0.8, 0.8])
        );
    }

    #[test]
    fn accepted_silence_fades_to_no_visible_trails() {
        let mut scene = Scene::new();
        for _ in 0..240 {
            scene.advance(STEP, active());
        }
        scene.rebuild_segments();
        assert!(!scene.segments().is_empty());
        let before = scene
            .segments()
            .iter()
            .map(|segment| segment.strength)
            .fold(0.0, f32::max);
        for _ in 0..60 {
            scene.advance(STEP, Drive::default());
        }
        scene.rebuild_segments();
        let after = scene
            .segments()
            .iter()
            .map(|segment| segment.strength)
            .fold(0.0, f32::max);
        assert!(after < before);
        for _ in 0..720 {
            scene.advance(STEP, Drive::default());
        }
        scene.rebuild_segments();
        assert!(scene.segments().is_empty());
    }

    #[test]
    fn each_band_changes_its_spatial_scale_and_motion() {
        let mut quiet = Scene::new();
        quiet.advance(
            STEP,
            Drive {
                energy: 0.5,
                ..Drive::default()
            },
        );
        for band in 0..3 {
            let mut scene = Scene::new();
            let mut drive = Drive {
                energy: 0.5,
                ..Drive::default()
            };
            drive.bands[band] = 1.0;
            scene.advance(STEP, drive);
            assert!(scene.phases[band] > quiet.phases[band]);
            assert!(
                scene
                    .particles
                    .iter()
                    .zip(&quiet.particles)
                    .any(|(left, right)| left.position != right.position)
            );
        }
    }
}
