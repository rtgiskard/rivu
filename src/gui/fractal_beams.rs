//! Deterministic, audio-time-driven branching light. The topology is built once;
//! only its retained geometry and bounded travelling pulses change during playback.

use super::fractal_visualizer::{Drive, LightSegment};
use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_2, TAU};

const ROOTS: usize = 3;
const MAX_DEPTH: u8 = 5;
const BRANCH_COUNT: usize = ROOTS * ((1 << (MAX_DEPTH as usize + 1)) - 1);
const PIECES: usize = 4;
const CURVE_SHAPE: [f32; PIECES] = [FRAC_1_SQRT_2, 1.0, FRAC_1_SQRT_2, 0.0];
const SEGMENT_COUNT: usize = BRANCH_COUNT * PIECES;
const PULSE_COUNT: usize = 12;
const NO_PARENT: usize = usize::MAX;
const PATH_LENGTH: f32 = 1.75;
const ROOT_POSITIONS: [[f32; 2]; ROOTS] = [[-0.26, 0.86], [0.02, 0.89], [0.26, 0.86]];
const ROOT_ANGLES: [f32; ROOTS] = [-0.14, 0.025, 0.14];
const ROOT_LENGTHS: [f32; ROOTS] = [0.48, 0.55, 0.46];

#[derive(Clone, Copy)]
struct Branch {
    parent: usize,
    root: usize,
    depth: u8,
    length: f32,
    split: f32,
    phase: f32,
    distance: f32,
    end: [f32; 2],
    angle: f32,
}

#[derive(Clone, Copy, Default)]
struct Pulse {
    age: f32,
    amplitude: f32,
    active: bool,
}

pub(super) struct Scene {
    branches: Vec<Branch>,
    segments: Vec<LightSegment>,
    pulses: [Pulse; PULSE_COUNT],
    next_pulse: usize,
    phases: [f32; 3],
    bands: [f32; 3],
    energy: f32,
    previous_onset: f32,
    pulse_cooldown: f32,
}

impl Scene {
    pub(super) fn new() -> Self {
        let mut scene = Self {
            branches: Vec::with_capacity(BRANCH_COUNT),
            segments: Vec::with_capacity(SEGMENT_COUNT),
            pulses: [Pulse::default(); PULSE_COUNT],
            next_pulse: 0,
            phases: [0.0; 3],
            bands: [0.0; 3],
            energy: 0.0,
            previous_onset: 0.0,
            pulse_cooldown: 0.0,
        };
        for (root, length) in ROOT_LENGTHS.iter().copied().enumerate() {
            scene.grow(NO_PARENT, root, 0, length, 0.0, 0.0);
        }
        scene.rebuild();
        scene
    }

    pub(super) fn reset(&mut self) {
        self.phases = [0.0; 3];
        self.bands = [0.0; 3];
        self.energy = 0.0;
        self.pulses = [Pulse::default(); PULSE_COUNT];
        self.next_pulse = 0;
        self.previous_onset = 0.0;
        self.pulse_cooldown = 0.0;
        self.rebuild();
    }

    pub(super) fn advance(&mut self, dt: f32, drive: Drive) {
        if dt <= 0.0 || !dt.is_finite() {
            return;
        }
        let dt = dt.min(0.1);
        for (phase, speed) in self.phases.iter_mut().zip([0.31, 0.47, 0.79]) {
            *phase = (*phase + dt * speed).rem_euclid(TAU);
        }
        // Fast attack, slower release: silence leaves an afterimage rather than
        // freezing a bright frame. The clocks still advance only with audio.
        let energy_rate = if drive.energy > self.energy {
            11.0
        } else {
            3.5
        };
        self.energy += (drive.energy - self.energy) * (1.0 - (-dt * energy_rate).exp());
        let band_blend = 1.0 - (-dt * 5.5).exp();
        for (band, target) in self.bands.iter_mut().zip(drive.bands) {
            *band += (target - *band) * band_blend;
        }
        self.pulse_cooldown = (self.pulse_cooldown - dt).max(0.0);
        for pulse in &mut self.pulses {
            if pulse.active {
                pulse.age += dt;
                if pulse.age >= 1.85 {
                    pulse.active = false;
                }
            }
        }
        // Edge detection prevents the same analysis frame from retriggering on
        // every simulation substep. These are energy-rise sparks, not beat ticks.
        if drive.energy > 0.015
            && drive.onset > 0.035
            && drive.onset > self.previous_onset + 0.02
            && self.pulse_cooldown == 0.0
        {
            self.pulses[self.next_pulse] = Pulse {
                age: 0.0,
                amplitude: 0.3 + 0.7 * drive.onset,
                active: true,
            };
            self.next_pulse = (self.next_pulse + 1) % PULSE_COUNT;
            self.pulse_cooldown = 0.12;
        }
        self.previous_onset = drive.onset;
        self.rebuild();
    }

    pub(super) fn segments(&self) -> &[LightSegment] {
        &self.segments
    }

    fn grow(
        &mut self,
        parent: usize,
        root: usize,
        depth: u8,
        length: f32,
        split: f32,
        distance: f32,
    ) {
        let index = self.branches.len();
        // A stateless integer hash gives each fixed branch its own proportions.
        // There is no random generator and no topology decision during playback.
        let character = character(index);
        self.branches.push(Branch {
            parent,
            root,
            depth,
            length,
            split,
            phase: character * TAU,
            distance,
            end: [0.0; 2],
            angle: 0.0,
        });
        if depth < MAX_DEPTH {
            let child_length = length * (0.67 + 0.055 * character);
            let distance = distance + length / PATH_LENGTH;
            self.grow(
                index,
                root,
                depth + 1,
                child_length,
                -0.88 - 0.2 * character,
                distance,
            );
            self.grow(
                index,
                root,
                depth + 1,
                child_length * 0.93,
                1.08 - 0.2 * character,
                distance,
            );
        }
    }

    fn rebuild(&mut self) {
        self.segments.clear();
        let low = self.bands[0];
        let mid = self.bands[1];
        let high = self.bands[2];
        let extension = 0.88 + 0.24 * low;
        let mut pulse_profiles = [(0.0, 0.0); PULSE_COUNT];
        let mut active_pulses = 0;
        for pulse in &self.pulses {
            if pulse.active {
                // Cache propagation and decay once, not once per light segment.
                pulse_profiles[active_pulses] =
                    (pulse.age * 0.64, pulse.amplitude / (1.0 + pulse.age));
                active_pulses += 1;
            }
        }
        for index in 0..self.branches.len() {
            let branch = self.branches[index];
            let (from, inherited_angle) = if branch.parent == NO_PARENT {
                (ROOT_POSITIONS[branch.root], ROOT_ANGLES[branch.root])
            } else {
                let parent = self.branches[branch.parent];
                (parent.end, parent.angle * 0.63)
            };
            let sway = (self.phases[0] + branch.phase).sin() * 0.025
                + (self.phases[1] + branch.phase * 2.0).sin() * 0.018;
            let angle = inherited_angle
                + branch.split * (0.27 + 0.30 * mid) * (0.88 + 0.045 * branch.depth as f32)
                + sway * (0.3 + 0.7 * low);
            let length = branch.length * extension;
            let direction = angle - FRAC_PI_2;
            let (sin, cos) = direction.sin_cos();
            let to = bounded([from[0] + cos * length, from[1] + sin * length]);
            self.branches[index].end = to;
            self.branches[index].angle = angle;
            let bend = length * (0.02 + 0.08 * low) * (self.phases[2] + branch.phase).sin();
            let (tone, light) = match branch.depth {
                0 => (0, 0.14 + 0.65 * low),
                1 | 2 => (1, 0.10 + 0.62 * mid),
                _ => (2, 0.025 + 0.65 * high),
            };
            let base_light = self.energy * light;
            let detail = if tone == 2 { 0.32 + 0.68 * high } else { 1.0 };
            let mut previous = from;
            for piece in 1..=PIECES {
                let t = piece as f32 / PIECES as f32;
                let curve = CURVE_SHAPE[piece - 1] * bend;
                let point = bounded([
                    from[0] + (to[0] - from[0]) * t - sin * curve,
                    from[1] + (to[1] - from[1]) * t + cos * curve,
                ]);
                let distance =
                    branch.distance + branch.length / PATH_LENGTH * (t - 0.5 / PIECES as f32);
                let mut pulse_light = 0.0;
                for &(front, amplitude) in &pulse_profiles[..active_pulses] {
                    let profile = (1.0 - (distance - front).abs() / 0.095).max(0.0);
                    pulse_light += profile * profile * amplitude;
                }
                self.segments.push(LightSegment {
                    from: previous,
                    to: point,
                    strength: (base_light + pulse_light * detail).clamp(0.0, 1.0),
                    tone,
                });
                previous = point;
            }
        }
    }
}

fn character(index: usize) -> f32 {
    let mut value = (index as u32).wrapping_add(0x6d2b_79f5);
    value = (value ^ (value >> 16)).wrapping_mul(0x85eb_ca6b);
    value = (value ^ (value >> 13)).wrapping_mul(0xc2b2_ae35);
    value ^= value >> 16;
    (value & 0xffff) as f32 / 65_535.0
}

fn bounded(point: [f32; 2]) -> [f32; 2] {
    [point[0].clamp(-0.95, 0.95), point[1].clamp(-0.95, 0.95)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 120.0;

    fn sounding() -> Drive {
        Drive {
            bands: [0.8, 0.6, 0.7],
            energy: 0.8,
            onset: 0.0,
        }
    }

    fn same_segments(left: &[LightSegment], right: &[LightSegment]) {
        assert_eq!(left.len(), right.len());
        for (left, right) in left.iter().zip(right) {
            assert_eq!(left.from, right.from);
            assert_eq!(left.to, right.to);
            assert_eq!(left.strength, right.strength);
            assert_eq!(left.tone, right.tone);
        }
    }

    fn light(scene: &Scene, tone: Option<usize>) -> f32 {
        scene
            .segments()
            .iter()
            .filter(|segment| tone.is_none_or(|tone| segment.tone == tone))
            .map(|segment| segment.strength)
            .sum()
    }

    #[test]
    fn paused_audio_does_not_change_geometry_light_or_pulses() {
        let mut scene = Scene::new();
        scene.advance(
            DT,
            Drive {
                onset: 1.0,
                ..sounding()
            },
        );
        for _ in 0..45 {
            scene.advance(DT, sounding());
        }
        let segments = scene.segments().to_vec();
        let phases = scene.phases;
        let pulse_age = scene.pulses[0].age;
        let energy = scene.energy;
        let bands = scene.bands;
        let previous_onset = scene.previous_onset;
        let cooldown = scene.pulse_cooldown;
        for _ in 0..100 {
            scene.advance(
                0.0,
                Drive {
                    onset: 1.0,
                    ..Drive::default()
                },
            );
        }
        same_segments(&segments, scene.segments());
        assert_eq!(scene.phases, phases);
        assert_eq!(scene.pulses[0].age, pulse_age);
        assert_eq!(scene.energy, energy);
        assert_eq!(scene.bands, bands);
        assert_eq!(scene.previous_onset, previous_onset);
        assert_eq!(scene.pulse_cooldown, cooldown);
    }

    #[test]
    fn reset_replays_identical_topology_and_motion() {
        let mut scene = Scene::new();
        let mut fresh = Scene::new();
        for _ in 0..240 {
            scene.advance(DT, sounding());
        }
        scene.reset();
        same_segments(scene.segments(), fresh.segments());
        for step in 0..180 {
            let drive = Drive {
                onset: if step % 37 == 0 { 0.8 } else { 0.0 },
                ..sounding()
            };
            scene.advance(DT, drive);
            fresh.advance(DT, drive);
            same_segments(scene.segments(), fresh.segments());
        }
    }

    #[test]
    fn long_playback_keeps_storage_depth_and_coordinates_bounded() {
        let mut scene = Scene::new();
        let branch_capacity = scene.branches.capacity();
        let segment_capacity = scene.segments.capacity();
        for step in 0..2400 {
            let t = step as f32 * 0.017;
            let drive = Drive {
                bands: [
                    (t.sin() + 1.0) * 0.5,
                    (t * 1.7).sin().abs(),
                    (t * 2.3).cos().abs(),
                ],
                energy: if step % 200 < 140 { 1.0 } else { 0.0 },
                onset: if step % 19 == 0 { 1.0 } else { 0.0 },
            };
            scene.advance(0.1, drive);
            assert_eq!(scene.branches.len(), BRANCH_COUNT);
            assert_eq!(scene.segments().len(), SEGMENT_COUNT);
            assert_eq!(scene.branches.capacity(), branch_capacity);
            assert_eq!(scene.segments.capacity(), segment_capacity);
            assert!(
                scene
                    .branches
                    .iter()
                    .all(|branch| branch.depth <= MAX_DEPTH)
            );
            assert!(
                scene
                    .phases
                    .iter()
                    .all(|phase| phase.is_finite() && (0.0..TAU).contains(phase))
            );
            assert!(scene.pulses.iter().filter(|pulse| pulse.active).count() <= PULSE_COUNT);
            for segment in scene.segments() {
                assert!(
                    segment
                        .from
                        .iter()
                        .chain(&segment.to)
                        .all(|coordinate| coordinate.is_finite() && coordinate.abs() <= 0.95)
                );
                assert!(segment.strength.is_finite() && (0.0..=1.0).contains(&segment.strength));
                assert!(segment.tone <= 2);
            }
        }
    }

    #[test]
    fn bands_control_distinct_geometry_and_fine_light() {
        let mut quiet_high = Scene::new();
        let mut bright_high = Scene::new();
        let mut wide = Scene::new();
        let mut extended = Scene::new();
        for _ in 0..180 {
            quiet_high.advance(
                DT,
                Drive {
                    bands: [0.0, 0.0, 0.0],
                    energy: 0.8,
                    onset: 0.0,
                },
            );
            bright_high.advance(
                DT,
                Drive {
                    bands: [0.0, 0.0, 1.0],
                    energy: 0.8,
                    onset: 0.0,
                },
            );
            wide.advance(
                DT,
                Drive {
                    bands: [0.0, 1.0, 0.0],
                    energy: 0.8,
                    onset: 0.0,
                },
            );
            extended.advance(
                DT,
                Drive {
                    bands: [1.0, 0.0, 0.0],
                    energy: 0.8,
                    onset: 0.0,
                },
            );
        }
        assert!(light(&bright_high, Some(2)) > light(&quiet_high, Some(2)) * 5.0);
        // The first child follows the root's four pieces; mids widen its fork.
        assert_ne!(
            quiet_high.segments()[PIECES * 2 - 1].to,
            wide.segments()[PIECES * 2 - 1].to
        );
        assert!(extended.segments()[PIECES - 1].to[1] < quiet_high.segments()[PIECES - 1].to[1]);
        assert_eq!(
            quiet_high.segments()[PIECES - 1].to,
            bright_high.segments()[PIECES - 1].to
        );
    }

    #[test]
    fn rise_pulse_moves_outward_and_silence_extinguishes_light() {
        let mut scene = Scene::new();
        for _ in 0..180 {
            scene.advance(
                DT,
                Drive {
                    energy: 0.8,
                    onset: 0.0,
                    bands: [0.0; 3],
                },
            );
        }
        scene.advance(
            DT,
            Drive {
                energy: 0.8,
                onset: 1.0,
                bands: [0.0; 3],
            },
        );
        let root_light = light(&scene, Some(0));
        let fine_light = light(&scene, Some(2));
        // Keeping one onset high must not retrigger each substep.
        for _ in 0..150 {
            scene.advance(
                DT,
                Drive {
                    energy: 0.8,
                    onset: 1.0,
                    bands: [0.0; 3],
                },
            );
        }
        assert_eq!(scene.next_pulse, 1);
        assert!(light(&scene, Some(0)) < root_light);
        assert!(light(&scene, Some(2)) > fine_light);
        let sounding_light = light(&scene, None);
        for _ in 0..720 {
            // Even an anomalous rise value cannot launch a spark in silence.
            scene.advance(
                DT,
                Drive {
                    onset: 1.0,
                    ..Drive::default()
                },
            );
        }
        assert_eq!(scene.next_pulse, 1);
        assert!(scene.pulses.iter().all(|pulse| !pulse.active));
        assert!(light(&scene, None) < sounding_light * 0.0001);
    }

    #[test]
    fn repeated_rises_recycle_the_fixed_pulse_pool() {
        let mut scene = Scene::new();
        let mut peak_active = 0;
        for step in 0..408 {
            let drive = Drive {
                onset: if step % 17 == 0 { 1.0 } else { 0.0 },
                ..sounding()
            };
            scene.advance(DT, drive);
            peak_active = peak_active.max(scene.pulses.iter().filter(|pulse| pulse.active).count());
        }
        assert_eq!(peak_active, PULSE_COUNT);
        assert_eq!(scene.next_pulse, 0);
        assert!(
            scene
                .pulses
                .iter()
                .all(|pulse| pulse.active && pulse.age.is_finite())
        );
    }
}
