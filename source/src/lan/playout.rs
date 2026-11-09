//! Bounded packet reordering, loss concealment and asynchronous clock matching.
use super::{
    protocol::{finite, Kind, Packet, FRAMES, RATE},
    LanStats,
};
use rtrb::{Consumer, Producer};
use rubato::{
    Resampler, SincFixedOut, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::sync::{atomic::Ordering, Arc};

const SLOTS: usize = 80; // 200 ms, including a maximum-size hardware capture burst.

pub struct Packetizer {
    producer: Producer<Packet>,
    packet: Packet,
    used: usize,
    stats: Arc<LanStats>,
}

impl Packetizer {
    pub fn set_quantum(&mut self, frames: usize) {
        self.packet.quantum = frames.clamp(FRAMES, 4096) as u16;
    }
    pub fn new(producer: Producer<Packet>, channels: u8, stats: Arc<LanStats>) -> Self {
        Self {
            producer,
            packet: Packet::audio(channels),
            used: 0,
            stats,
        }
    }
    pub fn push(&mut self, left: f32, right: f32) {
        let generation = self.stats.generation.load(Ordering::Acquire);
        if self.packet.generation != generation {
            self.packet.generation = generation;
            self.packet.position = 0;
            self.packet.sequence = 0;
            self.used = 0;
        }
        let channels = self.packet.channels as usize;
        self.packet.samples[self.used * channels] = finite(left);
        if channels == 2 {
            self.packet.samples[self.used * channels + 1] = finite(right);
        }
        self.used += 1;
        if self.used == FRAMES {
            if self.producer.push(self.packet.clone()).is_err() {
                self.stats.overruns.fetch_add(1, Ordering::Relaxed);
            }
            self.packet.sequence = self.packet.sequence.wrapping_add(1);
            self.packet.position += FRAMES as u64;
            self.used = 0;
        }
    }
}

/// A slow PI loop follows clock drift, rather than reacting to each packet burst.
#[derive(Default)]
pub struct DriftController {
    average: f64,
    integral: f64,
    correction: f64,
    initialized: bool,
}
impl DriftController {
    /// Returns input frames per output frame. Update once per 120 output frames.
    pub fn update(&mut self, fill: f64, target: f64) -> f64 {
        if !self.initialized {
            self.average = target;
            self.initialized = true;
        }
        self.average += 0.0025 * (fill - self.average);
        let error = self.average - target;
        let proposed = (self.integral + error * 5.0e-10).clamp(-0.001, 0.001);
        let raw = error * 5.0e-6 + proposed;
        if raw.abs() <= 0.001 || raw.signum() != error.signum() {
            self.integral = proposed;
        }
        let desired = (error * 5.0e-6 + self.integral).clamp(-0.001, 0.001);
        self.correction += (desired - self.correction).clamp(-0.0000025, 0.0000025);
        1.0 + self.correction
    }
}

pub struct Playout {
    slots: Vec<Option<Packet>>,
    head: Option<u64>,
    newest_end: u64,
    current: Packet,
    current_missing: bool,
    started: bool,
    target: usize,
    source_quantum: usize,
    render_quantum: usize,
    generation: u64,
    stats: Arc<LanStats>,
    resampler: SincFixedOut<f32>,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    output_index: usize,
    controller: DriftController,
    last: [f32; 2],
    fade: usize,
    starved_frames: usize,
}

impl Playout {
    pub fn new(buffer_ms: f32, stats: Arc<LanStats>) -> Result<Self, String> {
        let params = SincInterpolationParameters {
            sinc_len: 64,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let resampler =
            SincFixedOut::<f32>::new(1.0, 1.002, params, FRAMES, 2).map_err(|e| e.to_string())?;
        let input = resampler.input_buffer_allocate(true);
        let output = resampler.output_buffer_allocate(true);
        Ok(Self {
            slots: vec![None; SLOTS],
            head: None,
            newest_end: 0,
            current: Packet::audio(2),
            current_missing: false,
            started: false,
            target: (buffer_ms.clamp(2.5, 30.0) * RATE as f32 / 1000.0) as usize,
            source_quantum: FRAMES,
            render_quantum: FRAMES,
            generation: stats.generation.load(Ordering::Acquire),
            stats,
            resampler,
            input,
            output,
            output_index: FRAMES,
            controller: DriftController::default(),
            last: [0.0; 2],
            fade: 0,
            starved_frames: 0,
        })
    }

    pub fn reset(&mut self) {
        self.slots.fill(None);
        self.head = None;
        self.newest_end = 0;
        self.started = false;
        self.source_quantum = FRAMES;
        self.output_index = FRAMES;
        self.resampler.reset();
        self.controller = DriftController::default();
        self.last = [0.0; 2];
        self.fade = 0;
        self.starved_frames = 0;
        self.stats.fill_frames.store(0, Ordering::Relaxed);
        self.stats.drift_ppm.store(0, Ordering::Relaxed);
    }

    pub fn receive(&mut self, queue: &mut Consumer<Packet>) {
        let generation = self.stats.generation.load(Ordering::Acquire);
        if generation != self.generation {
            self.reset();
            self.generation = generation;
        }
        // Bound callback work even while the network producer is running.
        for _ in 0..SLOTS {
            let Ok(packet) = queue.pop() else {
                break;
            };
            if packet.generation == generation && self.stats.connected.load(Ordering::Acquire) {
                self.push(packet);
            }
        }
        if !self.stats.connected.load(Ordering::Acquire) {
            self.reset();
        }
    }

    pub fn push(&mut self, packet: Packet) {
        if packet.kind != Kind::Audio
            || !matches!(packet.channels, 1 | 2)
            || packet.position % FRAMES as u64 != 0
        {
            return;
        }
        let position = packet.position;
        if let Some(head) = self.head {
            if self.started && position < head {
                self.stats.late.fetch_add(1, Ordering::Relaxed);
                return;
            }
            if position.saturating_sub(head) >= (SLOTS * FRAMES) as u64 {
                self.stats.overruns.fetch_add(1, Ordering::Relaxed);
                self.reset(); // Re-prime with fresh audio instead of playing a backlog.
            }
        }
        self.head = Some(self.head.map_or(position, |head| {
            if self.started {
                head
            } else {
                head.min(position)
            }
        }));
        self.newest_end = self.newest_end.max(position + FRAMES as u64);
        self.source_quantum = (packet.quantum as usize).clamp(FRAMES, 4096);
        let slot = (position / FRAMES as u64) as usize % SLOTS;
        if self.slots[slot]
            .as_ref()
            .is_some_and(|p| p.position == position)
        {
            self.stats.duplicates.fetch_add(1, Ordering::Relaxed);
        } else {
            self.slots[slot] = Some(packet);
        }
    }

    fn next_input(&mut self) -> [f32; 2] {
        let position = self.head.unwrap();
        let offset = position as usize % FRAMES;
        if offset == 0 {
            let slot = (position / FRAMES as u64) as usize % SLOTS;
            match self.slots[slot].take().filter(|p| p.position == position) {
                Some(packet) => {
                    self.current = packet;
                    self.current_missing = false;
                    self.starved_frames = 0;
                }
                None => {
                    self.current_missing = true;
                    self.stats.lost.fetch_add(1, Ordering::Relaxed);
                    if position >= self.newest_end {
                        self.stats.underruns.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        self.head = Some(position + 1);
        if self.current_missing {
            self.starved_frames += 1;
            self.fade = 0;
            let scale = 1.0 - (offset + 1).min(48) as f32 / 48.0;
            let frame = [self.last[0] * scale, self.last[1] * scale];
            if offset >= 47 {
                self.last = [0.0; 2];
            }
            frame
        } else {
            let channel = self.current.channels as usize;
            let frame = [
                self.current.samples[offset * channel],
                self.current.samples[offset * channel + if channel == 2 { 1 } else { 0 }],
            ];
            self.last = frame;
            self.fade = (self.fade + 1).min(48);
            let gain = self.fade as f32 / 48.0;
            [frame[0] * gain, frame[1] * gain]
        }
    }

    fn produce(&mut self) -> bool {
        let Some(mut head) = self.head else {
            return false;
        };
        let mut fill = self.newest_end.saturating_sub(head) as usize;
        if !self.started {
            let quantum = self.source_quantum.max(self.render_quantum);
            let required =
                self.target + quantum + self.resampler.input_frames_next().saturating_sub(FRAMES);
            if fill < required {
                return false;
            }
            let excess = fill.saturating_sub(required);
            let drop = excess / FRAMES * FRAMES;
            head += drop as u64;
            self.head = Some(head);
            fill -= drop;
            for slot in &mut self.slots {
                if slot.as_ref().is_some_and(|p| p.position < head) {
                    *slot = None;
                }
            }
            self.started = true;
        }
        // Target is the reserve AFTER this block, not the fill before consuming it.
        let target = self.target + (self.source_quantum.max(self.render_quantum) + FRAMES) / 2;
        let ratio = self.controller.update(fill as f64, target as f64);
        self.stats
            .drift_ppm
            .store(((ratio - 1.0) * 1e6) as i32, Ordering::Relaxed);
        if self
            .resampler
            .set_resample_ratio(1.0 / ratio, true)
            .is_err()
        {
            return false;
        }
        let needed = self.resampler.input_frames_next();
        for i in 0..needed {
            let frame = self.next_input();
            self.input[0][i] = frame[0];
            self.input[1][i] = frame[1];
        }
        if self
            .resampler
            .process_into_buffer(&self.input, &mut self.output, None)
            .is_err()
        {
            return false;
        }
        self.stats.fill_frames.store(
            self.newest_end.saturating_sub(self.head.unwrap()),
            Ordering::Relaxed,
        );
        self.output_index = 0;
        true
    }

    pub fn render(&mut self, output: &mut [f32], channels: usize) {
        if channels == 0 {
            return;
        }
        self.render_quantum = (output.len() / channels).max(FRAMES);
        for frame in output.chunks_exact_mut(channels) {
            if self.output_index >= FRAMES && !self.produce() {
                frame.fill(0.0);
                continue;
            }
            let left = self.output[0][self.output_index];
            let right = self.output[1][self.output_index];
            self.output_index += 1;
            frame.fill(0.0);
            frame[0] = if channels == 1 {
                (left + right) * 0.5
            } else {
                left
            };
            if channels >= 2 {
                frame[1] = right;
            }
        }
        if self.starved_frames > RATE as usize / 20 {
            self.reset();
        }
    }
}
