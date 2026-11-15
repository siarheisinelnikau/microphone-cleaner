use crate::dsp::finite_sample;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

pub trait Endpoint {
    fn pause_endpoint(&self);
}

pub struct Runtime<S: Endpoint> {
    pub input: Option<S>,
    pub output: Option<S>,
    pub worker: Option<std::thread::JoinHandle<()>>,
    pub signals: Arc<Signals>,
}

impl<S: Endpoint> Drop for Runtime<S> {
    fn drop(&mut self) {
        self.signals.stop.store(true, Ordering::Release);
        if let Some(input) = self.input.take() {
            input.pause_endpoint();
            drop(input);
        }
        if let Some(output) = self.output.take() {
            output.pause_endpoint();
            drop(output);
        }
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
        self.signals.input_peak.store(0, Ordering::Relaxed);
        self.signals.output_peak.store(0, Ordering::Relaxed);
    }
}

pub const ERROR_INPUT: u32 = 1;
pub const ERROR_OUTPUT: u32 = 2;
pub const ERROR_STALLED: u32 = 4;
pub const ERROR_DSP: u32 = 8;

#[derive(Default)]
pub struct Signals {
    pub stop: AtomicBool,
    pub bypass: AtomicBool,
    pub strong: AtomicBool,
    pub errors: AtomicU32,
    pub generation: AtomicU64,
    pub reset_requested: AtomicBool,
    pub input_peak: AtomicU32,
    pub output_peak: AtomicU32,
    pub captured_frames: AtomicU64,
    pub played_frames: AtomicU64,
    pub input_dropped: AtomicU64,
    pub output_dropped: AtomicU64,
    pub underruns: AtomicU64,
    pub discontinuities: AtomicU64,
    pub queue_frames: AtomicU32,
}

#[derive(Clone, Copy, Debug)]
pub struct Captured {
    pub sample: f32,
    pub sequence: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct Rendered {
    pub sample: f32,
    pub generation: u64,
}

pub struct CaptureCallback {
    producer: Producer<Captured>,
    channels: usize,
    sequence: u64,
    partial_sum: f32,
    partial_channels: usize,
    signals: Arc<Signals>,
}

impl CaptureCallback {
    pub fn new(
        capacity: usize,
        channels: usize,
        signals: Arc<Signals>,
    ) -> Result<(Self, Consumer<Captured>), String> {
        if capacity == 0 || !(1..=8).contains(&channels) {
            return Err("Invalid capture buffer or channel count".into());
        }
        let (producer, consumer) = RingBuffer::new(capacity);
        Ok((
            Self {
                producer,
                channels,
                sequence: 0,
                partial_sum: 0.0,
                partial_channels: 0,
                signals,
            },
            consumer,
        ))
    }

    // This is shared by the real WASAPI adapter and fake devices. No allocation, mutex, logging,
    // file/network IO, DSP construction, or waiting is permitted in the callback.
    pub fn capture<T: Copy>(&mut self, samples: &[T], convert: impl Fn(T) -> f32) {
        if self.signals.stop.load(Ordering::Acquire) {
            return;
        }
        let mut peak = 0.0f32;
        let mut captured = 0;
        let mut dropped = 0;
        for &value in samples {
            let sample = finite_sample(convert(value));
            self.partial_sum += sample / self.channels as f32;
            self.partial_channels += 1;
            if self.partial_channels == self.channels {
                let mono = self.partial_sum;
                self.partial_sum = 0.0;
                self.partial_channels = 0;
                peak = peak.max(mono.abs());
                if self
                    .producer
                    .push(Captured {
                        sample: mono,
                        sequence: self.sequence,
                    })
                    .is_err()
                {
                    dropped += 1;
                }
                self.sequence = self.sequence.wrapping_add(1);
                captured += 1;
            }
        }
        self.signals
            .input_peak
            .store(peak.to_bits(), Ordering::Relaxed);
        self.signals
            .captured_frames
            .fetch_add(captured, Ordering::Relaxed);
        self.signals
            .input_dropped
            .fetch_add(dropped, Ordering::Relaxed);
        if dropped > 0 {
            self.signals.reset_requested.store(true, Ordering::Release);
        }
    }
}

pub struct Pump {
    capture: Consumer<Captured>,
    output: Producer<Rendered>,
    dsp: crate::dsp::StreamDsp,
    signals: Arc<Signals>,
    expected_sequence: Option<u64>,
    capacity: usize,
    target: usize,
}

impl Pump {
    pub fn new(
        capture: Consumer<Captured>,
        output: Producer<Rendered>,
        dsp: crate::dsp::StreamDsp,
        signals: Arc<Signals>,
        capacity: usize,
        target: usize,
    ) -> Self {
        Self {
            capture,
            output,
            dsp,
            signals,
            expected_sequence: None,
            capacity,
            target,
        }
    }

    fn reset(&mut self) {
        self.dsp.reset();
        self.expected_sequence = None;
        self.signals.generation.fetch_add(1, Ordering::AcqRel);
        self.signals.discontinuities.fetch_add(1, Ordering::Relaxed);
    }

    pub fn process_available(&mut self, budget: usize) -> usize {
        if self.signals.stop.load(Ordering::Acquire) {
            return 0;
        }
        if self.signals.reset_requested.swap(false, Ordering::AcqRel) {
            // Discard the bounded old capture backlog after over/underrun. Never replay stale speech.
            let count = self.capture.slots();
            for _ in 0..count {
                let _ = self.capture.pop();
            }
            self.reset();
        }
        let strength = if self.signals.strong.load(Ordering::Relaxed) {
            crate::config::Strength::Strong
        } else {
            crate::config::Strength::Natural
        };
        self.dsp
            .set_mode(strength, self.signals.bypass.load(Ordering::Relaxed));
        self.dsp
            .compensate_clock(self.capacity - self.output.slots(), self.target);
        let mut processed = 0;
        for _ in 0..budget {
            let Ok(item) = self.capture.pop() else {
                break;
            };
            if self
                .expected_sequence
                .is_some_and(|sequence| sequence != item.sequence)
            {
                self.reset();
            }
            self.expected_sequence = Some(item.sequence.wrapping_add(1));
            let generation = self.signals.generation.load(Ordering::Acquire);
            let signals = &self.signals;
            let output = &mut self.output;
            if self
                .dsp
                .push(item.sample, |sample| {
                    if output.push(Rendered { sample, generation }).is_err() {
                        signals.output_dropped.fetch_add(1, Ordering::Relaxed);
                        signals.reset_requested.store(true, Ordering::Release);
                    }
                })
                .is_err()
            {
                self.signals.errors.fetch_or(ERROR_DSP, Ordering::Release);
                break;
            }
            processed += 1;
        }
        processed
    }
}

pub struct OutputCallback {
    consumer: Consumer<Rendered>,
    channels: usize,
    prime_frames: usize,
    primed: bool,
    generation: u64,
    signals: Arc<Signals>,
    channel_position: usize,
    current_sample: f32,
}

impl OutputCallback {
    pub fn new(
        capacity: usize,
        channels: usize,
        prime_frames: usize,
        signals: Arc<Signals>,
    ) -> Result<(Producer<Rendered>, Self), String> {
        if capacity == 0
            || channels == 0
            || channels > 8
            || prime_frames == 0
            || prime_frames > capacity
        {
            return Err("Invalid output buffer or channel count".into());
        }
        let (producer, consumer) = RingBuffer::new(capacity);
        Ok((
            producer,
            Self {
                consumer,
                channels,
                prime_frames,
                primed: false,
                generation: 0,
                signals,
                channel_position: 0,
                current_sample: 0.0,
            },
        ))
    }

    pub fn render<T: Copy>(&mut self, samples: &mut [T], convert: impl Fn(f32) -> T) {
        if self.signals.stop.load(Ordering::Acquire) {
            samples.fill(convert(0.0));
            self.signals.output_peak.store(0, Ordering::Relaxed);
            return;
        }
        let current = self.signals.generation.load(Ordering::Acquire);
        if current != self.generation {
            self.primed = false;
            self.generation = current;
            self.current_sample = 0.0;
            // The queue is bounded, so stale generations can be discarded with bounded work.
            // Leave current-generation data for the next call rather than mix stream timelines.
            let available = self.consumer.slots();
            for _ in 0..available {
                if let Ok(item) = self.consumer.peek() {
                    if item.generation == current {
                        break;
                    }
                    let _ = self.consumer.pop();
                }
            }
        }
        if !self.primed && self.consumer.slots() >= self.prime_frames {
            self.primed = true;
        }
        let mut peak = 0.0f32;
        let mut played = 0;
        let mut starved = false;
        for value in samples {
            if self.channel_position == 0 {
                self.current_sample = if self.primed {
                    match self.consumer.pop() {
                        Ok(item) if item.generation == current => finite_sample(item.sample),
                        _ => {
                            self.primed = false;
                            starved = true;
                            0.0
                        }
                    }
                } else {
                    0.0
                };
                played += 1;
            }
            peak = peak.max(self.current_sample.abs());
            *value = convert(self.current_sample);
            self.channel_position = (self.channel_position + 1) % self.channels;
        }
        if starved {
            self.signals.underruns.fetch_add(1, Ordering::Relaxed);
            self.signals.reset_requested.store(true, Ordering::Release);
        }
        self.signals
            .output_peak
            .store(peak.to_bits(), Ordering::Relaxed);
        self.signals
            .played_frames
            .fetch_add(played, Ordering::Relaxed);
        self.signals
            .queue_frames
            .store(self.consumer.slots() as u32, Ordering::Relaxed);
    }
}
