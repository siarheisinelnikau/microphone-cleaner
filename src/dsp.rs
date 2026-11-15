use crate::config::Strength;
use nnnoiseless::DenoiseState;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

pub const RATE: u32 = 48_000;
pub const FRAME: usize = 480;

pub fn finite_sample(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

pub struct NeuralFrame {
    engine: Box<DenoiseState<'static>>,
    pristine: Box<DenoiseState<'static>>,
    dry: [f32; FRAME],
    pcm: [f32; FRAME],
    wet: [f32; FRAME],
    mix: f32,
    target: f32,
    pub speech_probability: f32,
}

impl NeuralFrame {
    pub fn new(strength: Strength, bypass: bool) -> Self {
        let engine = DenoiseState::new();
        let pristine = engine.clone();
        let target = if bypass { 0.0 } else { Self::amount(strength) };
        Self {
            engine,
            pristine,
            dry: [0.0; FRAME],
            pcm: [0.0; FRAME],
            wet: [0.0; FRAME],
            mix: target,
            target,
            speech_probability: 0.0,
        }
    }

    fn amount(strength: Strength) -> f32 {
        match strength {
            Strength::Natural => 0.75,
            Strength::Strong => 1.0,
        }
    }

    pub fn set_mode(&mut self, strength: Strength, bypass: bool) {
        self.target = if bypass { 0.0 } else { Self::amount(strength) };
    }

    pub fn reset(&mut self) {
        self.engine.clone_from(&self.pristine);
        self.dry.fill(0.0);
        self.mix = self.target;
        self.speech_probability = 0.0;
    }

    pub fn process(&mut self, input: &[f32; FRAME], output: &mut [f32; FRAME]) {
        for (dst, &src) in self.pcm.iter_mut().zip(input) {
            *dst = finite_sample(src) * 32768.0;
        }
        self.speech_probability = self
            .engine
            .process_frame(&mut self.wet, &self.pcm)
            .clamp(0.0, 1.0);
        // RNNoise overlap/add synthesizes the preceding 10 ms. Delay dry identically so toggling
        // bypass never switches to a different point in time. Keep the neural state warm in bypass.
        for i in 0..FRAME {
            let difference = self.target - self.mix;
            self.mix += difference.clamp(-1.0 / 240.0, 1.0 / 240.0);
            output[i] =
                finite_sample(self.dry[i] * (1.0 - self.mix) + self.wet[i] / 32768.0 * self.mix);
            self.dry[i] = finite_sample(input[i]);
        }
    }
}

struct RateStage {
    resampler: Option<SincFixedIn<f32>>,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    used: usize,
    chunk: usize,
    base_ratio: f64,
    delay: usize,
}

impl RateStage {
    fn new(from: u32, to: u32, chunk: usize, adjustable: bool) -> Result<Self, String> {
        let ratio = to as f64 / from as f64;
        if from == to && !adjustable {
            return Ok(Self {
                resampler: None,
                input: vec![vec![0.0; chunk]],
                output: vec![vec![0.0; chunk]],
                used: 0,
                chunk,
                base_ratio: ratio,
                delay: 0,
            });
        }
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.92,
            interpolation: SincInterpolationType::Cubic,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let resampler =
            SincFixedIn::new(ratio, 1.005, params, chunk, 1).map_err(|e| e.to_string())?;
        let input = resampler.input_buffer_allocate(true);
        let output = resampler.output_buffer_allocate(true);
        let delay = resampler.output_delay();
        Ok(Self {
            resampler: Some(resampler),
            input,
            output,
            used: 0,
            chunk,
            base_ratio: ratio,
            delay,
        })
    }

    fn push(&mut self, sample: f32) -> Result<Option<&[f32]>, String> {
        self.input[0][self.used] = finite_sample(sample);
        self.used += 1;
        if self.used != self.chunk {
            return Ok(None);
        }
        self.used = 0;
        let count = match self.resampler.as_mut() {
            Some(resampler) => {
                resampler
                    .process_into_buffer(&self.input, &mut self.output, None)
                    .map_err(|e| e.to_string())?
                    .1
            }
            None => {
                self.output[0].copy_from_slice(&self.input[0]);
                self.chunk
            }
        };
        Ok(Some(&self.output[0][..count]))
    }

    fn reset(&mut self) {
        self.used = 0;
        self.input[0].fill(0.0);
        self.output[0].fill(0.0);
        if let Some(resampler) = &mut self.resampler {
            resampler.reset();
            let _ = resampler.set_resample_ratio(self.base_ratio, false);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DelayBudget {
    pub neural_ms: f64,
    pub resampling_ms: f64,
    pub maximum_framing_ms: f64,
    // Device/driver/call-app latency is deliberately absent: it has not been measured.
}

pub struct StreamDsp {
    input_stage: RateStage,
    neural: NeuralFrame,
    output_stage: RateStage,
    frame: [f32; FRAME],
    clean: [f32; FRAME],
    used: usize,
    input_rate: u32,
    output_rate: u32,
    pub input_frames: u64,
    pub neural_frames: u64,
    pub output_frames: u64,
    clock_error: f64,
    clock_integral: f64,
    clock_frame: u64,
}

impl StreamDsp {
    pub fn new(
        input_rate: u32,
        output_rate: u32,
        strength: Strength,
        bypass: bool,
    ) -> Result<Self, String> {
        if !(16_000..=192_000).contains(&input_rate) || !(16_000..=192_000).contains(&output_rate) {
            return Err("Supported device rates are 16–192 kHz".into());
        }
        Ok(Self {
            input_stage: RateStage::new(input_rate, RATE, (input_rate / 200) as usize, false)?,
            neural: NeuralFrame::new(strength, bypass),
            output_stage: RateStage::new(RATE, output_rate, FRAME, true)?,
            frame: [0.0; FRAME],
            clean: [0.0; FRAME],
            used: 0,
            input_rate,
            output_rate,
            input_frames: 0,
            neural_frames: 0,
            output_frames: 0,
            clock_error: 0.0,
            clock_integral: 0.0,
            clock_frame: 0,
        })
    }

    pub fn delay_budget(&self) -> DelayBudget {
        DelayBudget {
            neural_ms: 10.0,
            resampling_ms: self.input_stage.delay as f64 / 48.0
                + self.output_stage.delay as f64 * 1000.0 / self.output_rate as f64,
            maximum_framing_ms: 10.0
                + self.input_stage.chunk as f64 * 1000.0 / self.input_rate as f64,
        }
    }

    pub fn set_mode(&mut self, strength: Strength, bypass: bool) {
        self.neural.set_mode(strength, bypass);
    }

    pub fn reset(&mut self) {
        self.input_stage.reset();
        self.output_stage.reset();
        self.neural.reset();
        self.used = 0;
        self.frame.fill(0.0);
        self.clean.fill(0.0);
        self.clock_error = 0.0;
        self.clock_integral = 0.0;
        self.clock_frame = self.neural_frames;
    }

    pub fn compensate_clock(&mut self, queue_frames: usize, target_frames: usize) {
        if self.clock_frame == self.neural_frames {
            return;
        }
        let elapsed = (self.neural_frames - self.clock_frame).min(10) as f64;
        self.clock_frame = self.neural_frames;
        if let Some(resampler) = &mut self.output_stage.resampler {
            // Smooth packet jitter and integrate clock drift once per processed 10 ms frame.
            // The bounded PI servo can follow ±2,000 ppm without requiring a permanent queue
            // offset. Larger disruptions reset and re-prime rather than retaining stale speech.
            let error = (queue_frames as f64 - target_frames as f64) / self.output_rate as f64;
            self.clock_error += (error - self.clock_error) * 0.02;
            self.clock_integral =
                (self.clock_integral - self.clock_error * 0.0002 * elapsed).clamp(-0.002, 0.002);
            let correction = (self.clock_integral - self.clock_error * 0.04).clamp(-0.002, 0.002);
            let _ = resampler
                .set_resample_ratio(self.output_stage.base_ratio * (1.0 + correction), true);
        }
    }

    pub fn push(&mut self, sample: f32, mut emit: impl FnMut(f32)) -> Result<(), String> {
        self.input_frames += 1;
        if let Some(samples) = self.input_stage.push(sample)? {
            for &sample in samples {
                self.frame[self.used] = sample;
                self.used += 1;
                if self.used == FRAME {
                    self.used = 0;
                    self.neural.process(&self.frame, &mut self.clean);
                    self.neural_frames += 1;
                    for &sample in &self.clean {
                        if let Some(output) = self.output_stage.push(sample)? {
                            for &sample in output {
                                emit(finite_sample(sample));
                                self.output_frames += 1;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
