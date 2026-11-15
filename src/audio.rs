use crate::{
    config::Strength,
    dsp::{DelayBudget, StreamDsp},
    provider::{self, CableRoute, Device},
    transport::{
        CaptureCallback, ERROR_INPUT, ERROR_OUTPUT, ERROR_STALLED, Endpoint, OutputCallback, Pump,
        Runtime, Signals,
    },
};
use cpal::{
    BufferSize, SampleFormat, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::{
    sync::{Arc, atomic::Ordering},
    thread::{self},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub inputs: Vec<Device>,
    pub routes: Vec<CableRoute>,
}

fn wasapi() -> Result<cpal::Host, String> {
    cpal::host_from_id(cpal::HostId::Wasapi).map_err(|e| e.to_string())
}

pub fn enumerate() -> Result<Catalog, String> {
    let host = wasapi()?;
    let describe = |device: cpal::Device| -> Option<Device> {
        Some(Device {
            id: device.id().ok()?.to_string(),
            name: device.description().ok()?.name().to_owned(),
        })
    };
    let mut inputs: Vec<_> = host
        .input_devices()
        .map_err(|e| e.to_string())?
        .filter_map(&describe)
        .collect();
    let mut outputs: Vec<_> = host
        .output_devices()
        .map_err(|e| e.to_string())?
        .filter_map(describe)
        .collect();
    inputs.sort_by(|a, b| a.name.cmp(&b.name));
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    let routes = provider::cable_routes(&outputs, &inputs);
    inputs.retain(|d| !provider::is_cable_capture(&d.name));
    Ok(Catalog { inputs, routes })
}

fn select_format(
    device: &cpal::Device,
    input: bool,
) -> Result<(StreamConfig, SampleFormat), String> {
    let formats: Vec<_> = if input {
        device
            .supported_input_configs()
            .map_err(|e| e.to_string())?
            .collect()
    } else {
        device
            .supported_output_configs()
            .map_err(|e| e.to_string())?
            .collect()
    };
    let mut candidates = Vec::new();
    for range in formats {
        if !(1..=2).contains(&range.channels()) {
            continue;
        }
        let sample_format = range.sample_format();
        if !matches!(
            sample_format,
            SampleFormat::F32 | SampleFormat::I16 | SampleFormat::I32 | SampleFormat::U16
        ) {
            continue;
        }
        for (preference, rate) in [48_000, 44_100, 96_000, 32_000, 16_000, 192_000]
            .into_iter()
            .enumerate()
        {
            if range.min_sample_rate() <= rate && range.max_sample_rate() >= rate {
                let supported = range.with_sample_rate(rate);
                let mut config = supported.config();
                let desired = rate / 100;
                config.buffer_size = match range.buffer_size() {
                    cpal::SupportedBufferSize::Range { min, max } => {
                        BufferSize::Fixed(desired.clamp(*min, *max))
                    }
                    cpal::SupportedBufferSize::Unknown => BufferSize::Default,
                };
                let score = preference * 10
                    + usize::from(sample_format != SampleFormat::F32) * 2
                    + usize::from(range.channels() != 1);
                candidates.push((score, config, sample_format));
                break;
            }
        }
    }
    candidates.sort_by_key(|x| x.0);
    candidates
        .into_iter()
        .next()
        .map(|(_, c, f)| (c, f))
        .ok_or_else(|| "The endpoint has no supported mono/stereo PCM format at 16–192 kHz".into())
}

#[derive(Clone, Debug)]
pub struct RunningInfo {
    pub input_name: String,
    pub call_microphone: String,
    pub input_rate: u32,
    pub output_rate: u32,
    pub requested_input_buffer: Option<u32>,
    pub requested_output_buffer: Option<u32>,
    pub budget: DelayBudget,
}

pub struct Session {
    runtime: Runtime<cpal::Stream>,
    pub signals: Arc<Signals>,
    pub info: RunningInfo,
}

impl Session {
    pub fn start(
        input_id: &str,
        output_id: &str,
        strength: Strength,
        bypass: bool,
    ) -> Result<Self, String> {
        let catalog = enumerate()?;
        let microphone = catalog
            .inputs
            .iter()
            .find(|d| d.id == input_id)
            .ok_or("The selected input microphone is unavailable")?;
        let route = catalog.routes.iter().find(|r| r.render.id == output_id)
            .ok_or("The selected VB-CABLE endpoint and its recording side must both be installed and available")?;
        if microphone.id == route.microphone.id {
            return Err("A virtual cable cannot feed itself".into());
        }
        let host = wasapi()?;
        let input_device = host
            .device_by_id(&input_id.parse().map_err(|_| "Invalid input endpoint ID")?)
            .ok_or("Input device was disconnected")?;
        let output_device = host
            .device_by_id(
                &output_id
                    .parse()
                    .map_err(|_| "Invalid output endpoint ID")?,
            )
            .ok_or("Virtual cable was disconnected")?;
        let (input_config, input_format) = select_format(&input_device, true)?;
        let (output_config, output_format) = select_format(&output_device, false)?;
        let signals = Arc::new(Signals::default());
        signals.bypass.store(bypass, Ordering::Relaxed);
        signals
            .strong
            .store(strength == Strength::Strong, Ordering::Relaxed);
        let dsp = StreamDsp::new(
            input_config.sample_rate,
            output_config.sample_rate,
            strength,
            bypass,
        )?;
        let info = RunningInfo {
            input_name: microphone.name.clone(),
            call_microphone: route.microphone.name.clone(),
            input_rate: input_config.sample_rate,
            output_rate: output_config.sample_rate,
            requested_input_buffer: fixed(input_config.buffer_size),
            requested_output_buffer: fixed(output_config.buffer_size),
            budget: dsp.delay_budget(),
        };
        let capacity = output_config.sample_rate as usize / 20;
        let target = output_config.sample_rate as usize / 50;
        let (capture, consumer) = CaptureCallback::new(
            input_config.sample_rate as usize / 20,
            input_config.channels as usize,
            signals.clone(),
        )?;
        let (producer, output) = OutputCallback::new(
            capacity,
            output_config.channels as usize,
            target,
            signals.clone(),
        )?;
        // Stream construction is paused on WASAPI; no endpoint plays until both streams and DSP exist.
        let input = input_stream(
            &input_device,
            &input_config,
            input_format,
            capture,
            signals.clone(),
        )?;
        let output = output_stream(
            &output_device,
            &output_config,
            output_format,
            output,
            signals.clone(),
        )?;
        let mut pump = Pump::new(consumer, producer, dsp, signals.clone(), capacity, target);
        let worker_signals = signals.clone();
        let worker = thread::Builder::new()
            .name("microphone-dsp".into())
            .spawn(move || {
                let mut last_audio = Instant::now();
                while !worker_signals.stop.load(Ordering::Acquire) {
                    if pump.process_available(1920) > 0 {
                        last_audio = Instant::now();
                    } else if last_audio.elapsed() > Duration::from_secs(2) {
                        worker_signals
                            .errors
                            .fetch_or(ERROR_STALLED, Ordering::Release);
                        break;
                    }
                    // Only this normal-priority worker waits. Audio callbacks never wake/lock a thread.
                    thread::park_timeout(Duration::from_millis(1));
                }
            })
            .map_err(|e| e.to_string())?;
        let runtime = Runtime {
            input: Some(input),
            output: Some(output),
            worker: Some(worker),
            signals: signals.clone(),
        };
        let session = Self {
            runtime,
            signals,
            info,
        };
        session
            .runtime
            .output
            .as_ref()
            .unwrap()
            .play()
            .map_err(|e| e.to_string())?;
        session
            .runtime
            .input
            .as_ref()
            .unwrap()
            .play()
            .map_err(|e| e.to_string())?;
        Ok(session)
    }
}

fn fixed(buffer: BufferSize) -> Option<u32> {
    if let BufferSize::Fixed(n) = buffer {
        Some(n)
    } else {
        None
    }
}

impl Endpoint for cpal::Stream {
    fn pause_endpoint(&self) {
        let _ = self.pause();
    }
}

fn input_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    mut capture: CaptureCallback,
    signals: Arc<Signals>,
) -> Result<cpal::Stream, String> {
    macro_rules! build {
        ($sample:ty, $convert:expr) => {{
            let errors = signals.clone();
            let discontinuities = signals.clone();
            let rate = config.sample_rate as f64;
            let channels = config.channels as usize;
            let mut previous: Option<(cpal::StreamInstant, usize)> = None;
            device.build_input_stream(
                config,
                move |data: &[$sample], info: &cpal::InputCallbackInfo| {
                    let timestamp = info.timestamp().capture;
                    if let Some((last, frames)) = previous {
                        let expected = frames as f64 / rate;
                        if timestamp.duration_since(&last).is_none_or(|duration| {
                            (duration.as_secs_f64() - expected).abs() > 0.002
                        }) {
                            discontinuities
                                .reset_requested
                                .store(true, Ordering::Release);
                        }
                    }
                    previous = Some((timestamp, data.len() / channels));
                    capture.capture(data, $convert);
                },
                move |_| {
                    errors.errors.fetch_or(ERROR_INPUT, Ordering::Release);
                },
                Some(Duration::from_secs(3)),
            )
        }};
    }
    let stream = match format {
        SampleFormat::F32 => build!(f32, |x| x),
        SampleFormat::I16 => build!(i16, |x| x as f32 / 32768.0),
        SampleFormat::I32 => build!(i32, |x| x as f32 / 2_147_483_648.0),
        SampleFormat::U16 => build!(u16, |x| (x as f32 - 32768.0) / 32768.0),
        _ => return Err("Unsupported capture sample format".into()),
    };
    stream.map_err(|e| e.to_string())
}

fn output_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    mut output: OutputCallback,
    signals: Arc<Signals>,
) -> Result<cpal::Stream, String> {
    macro_rules! build {
        ($sample:ty, $convert:expr) => {{
            let errors = signals.clone();
            device.build_output_stream(
                config,
                move |data: &mut [$sample], _: &cpal::OutputCallbackInfo| {
                    output.render(data, $convert);
                },
                move |_| {
                    errors.errors.fetch_or(ERROR_OUTPUT, Ordering::Release);
                },
                Some(Duration::from_secs(3)),
            )
        }};
    }
    match format {
        SampleFormat::F32 => build!(f32, |x| x),
        SampleFormat::I16 => build!(i16, |x| (x * 32768.0).round().clamp(-32768.0, 32767.0)
            as i16),
        SampleFormat::I32 => build!(i32, |x| (x as f64 * 2_147_483_648.0)
            .round()
            .clamp(-2_147_483_648.0, 2_147_483_647.0)
            as i32),
        SampleFormat::U16 => build!(u16, |x| (x * 32768.0 + 32768.0).round().clamp(0.0, 65535.0)
            as u16),
        _ => return Err("Unsupported render sample format".into()),
    }
    .map_err(|e| e.to_string())
}
