use microphone_cleaner::{
    config::Strength,
    dsp::{FRAME, NeuralFrame, StreamDsp, finite_sample},
    transport::{
        CaptureCallback, ERROR_INPUT, Endpoint, OutputCallback, Pump, Rendered, Runtime, Signals,
    },
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct CountAllocations;
thread_local! { static TRACK: Cell<bool> = const { Cell::new(false) }; static ALLOCATIONS: Cell<usize> = const { Cell::new(0) }; }
// SAFETY: Allocation and deallocation are forwarded unchanged to System. The optional counter
// uses const-initialized thread-local cells and neither allocates nor observes other threads.
unsafe impl GlobalAlloc for CountAllocations {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACK.try_with(Cell::get).unwrap_or(false) {
            ALLOCATIONS.with(|n| n.set(n.get() + 1));
        }
        // SAFETY: Forward the exact layout to the underlying allocator.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: Pointer and layout originated from the forwarded System allocation.
        unsafe {
            System.dealloc(pointer, layout);
        }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if TRACK.try_with(Cell::get).unwrap_or(false) {
            ALLOCATIONS.with(|n| n.set(n.get() + 1));
        }
        // SAFETY: Forward the original allocation and requested size to System.
        unsafe { System.realloc(pointer, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountAllocations = CountAllocations;

fn without_allocations(f: impl FnOnce()) {
    ALLOCATIONS.with(|n| n.set(0));
    TRACK.with(|n| n.set(true));
    f();
    TRACK.with(|n| n.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
}

fn noise(state: &mut u32) -> f32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    (*state as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32
}

#[test]
fn callbacks_use_bounded_queues_without_allocations() {
    let signals = Arc::new(Signals::default());
    let (mut capture, mut captured) = CaptureCallback::new(2048, 2, signals.clone()).unwrap();
    let (mut rendered, mut output) = OutputCallback::new(2048, 2, 480, signals).unwrap();
    let input = [0.25f32; 960];
    let mut buffer = [0.0f32; 960];
    for _ in 0..480 {
        rendered
            .push(Rendered {
                sample: 0.25,
                generation: 0,
            })
            .unwrap();
    }
    without_allocations(|| {
        capture.capture(&input, |x| x);
        output.render(&mut buffer, |x| x);
    });
    assert!(buffer.iter().all(|&sample| sample == 0.25));
    assert_eq!(captured.slots(), 480);
    assert_eq!(captured.pop().unwrap().sample, 0.25);
}

#[test]
fn capture_preserves_stereo_partial_frames_and_sequence() {
    let signals = Arc::new(Signals::default());
    let (mut capture, mut queue) = CaptureCallback::new(16, 2, signals).unwrap();
    capture.capture(&[0.2, 0.4, 0.6], |x| x);
    capture.capture(&[0.8, -0.5, 0.5], |x| x);
    for (index, expected) in [0.3f32, 0.7, 0.0].into_iter().enumerate() {
        let frame = queue.pop().unwrap();
        assert!((frame.sample - expected).abs() < 1e-6);
        assert_eq!(frame.sequence, index as u64);
    }
    assert!(queue.is_empty());
}

#[test]
fn capture_overflow_is_explicit_and_never_grows_the_queue() {
    let signals = Arc::new(Signals::default());
    let (mut capture, queue) = CaptureCallback::new(8, 1, signals.clone()).unwrap();
    capture.capture(&[0.5; 100], |x| x);
    assert_eq!(queue.slots(), 8);
    assert_eq!(signals.input_dropped.load(Ordering::Relaxed), 92);
    assert!(signals.reset_requested.load(Ordering::Acquire));
}

#[test]
fn invalid_input_never_reaches_neural_engine_or_output() {
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 3.0, -8.0, 0.0] {
        assert!(finite_sample(value).is_finite() && finite_sample(value).abs() <= 1.0);
    }
    let mut neural = NeuralFrame::new(Strength::Strong, false);
    let mut result = [0.0; FRAME];
    for _ in 0..4 {
        neural.process(&[f32::NAN; FRAME], &mut result);
        assert!(result.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
    }
}

#[test]
fn neural_silence_stays_silent() {
    let mut neural = NeuralFrame::new(Strength::Strong, false);
    let mut result = [1.0; FRAME];
    for _ in 0..8 {
        neural.process(&[0.0; FRAME], &mut result);
        assert!(result.iter().all(|x| x.abs() < 1e-8));
    }
}

#[test]
fn neural_bypass_preserves_pcm_with_exactly_one_frame_delay() {
    let mut neural = NeuralFrame::new(Strength::Strong, true);
    let mut output = [0.0; FRAME];
    let mut previous = [0.0; FRAME];
    for frame in 0..20 {
        let input = std::array::from_fn(|i| ((frame * FRAME + i) as f32 * 0.039).sin() * 0.4);
        neural.process(&input, &mut output);
        assert_eq!(output, previous);
        previous = input;
    }
    neural.reset();
    neural.process(&[0.0; FRAME], &mut output);
    assert_eq!(output, [0.0; FRAME]);
}

#[test]
fn real_neural_engine_attenuates_generated_stationary_noise() {
    let mut neural = NeuralFrame::new(Strength::Strong, false);
    let mut random = 0x12345678;
    let mut input_energy = 0.0f64;
    let mut output_energy = 0.0f64;
    let mut output = [0.0; FRAME];
    for frame in 0..160 {
        let input = std::array::from_fn(|_| noise(&mut random) * 0.08);
        neural.process(&input, &mut output);
        if frame > 80 {
            input_energy += input.iter().map(|x| (*x as f64).powi(2)).sum::<f64>();
            output_energy += output.iter().map(|x| (*x as f64).powi(2)).sum::<f64>();
        }
        assert!(output.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
    }
    // This is a deterministic algorithm regression, not a speech-quality or advertised dB target.
    assert!(
        output_energy < input_energy * 0.9,
        "synthetic noise ratio = {}",
        output_energy / input_energy
    );
}

#[test]
fn warm_stream_processing_reuses_allocated_dsp_buffers() {
    let mut pipeline = StreamDsp::new(44_100, 48_000, Strength::Natural, false).unwrap();
    for _ in 0..882 {
        pipeline.push(0.1, |_| {}).unwrap();
    }
    without_allocations(|| {
        for _ in 0..4410 {
            pipeline.push(0.1, |_| {}).unwrap();
        }
    });
}

fn render(rate: u32, out_rate: u32, chunks: &[usize]) -> (Vec<f32>, u64, u64) {
    let input: Vec<_> = (0..rate)
        .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 300.0 / rate as f32).sin() * 0.2)
        .collect();
    let mut dsp = StreamDsp::new(rate, out_rate, Strength::Natural, true).unwrap();
    let budget = dsp.delay_budget();
    assert_eq!(budget.neural_ms, 10.0);
    assert!(budget.resampling_ms < 10.0);
    let mut output = Vec::new();
    let mut offset = 0;
    let mut index = 0;
    while offset < input.len() {
        let count = chunks[index % chunks.len()].min(input.len() - offset);
        for &sample in &input[offset..offset + count] {
            dsp.push(sample, |x| output.push(x)).unwrap();
        }
        index += 1;
        offset += count;
    }
    (output, dsp.input_frames, dsp.neural_frames)
}

#[test]
fn arbitrary_callback_chunks_produce_identical_streams_across_sample_rates() {
    for (rate, out) in [
        (44_100, 48_000),
        (48_000, 44_100),
        (16_000, 48_000),
        (96_000, 48_000),
    ] {
        let (a, input, frames) = render(rate, out, &[rate as usize]);
        let (b, _, _) = render(rate, out, &[1, 13, 487, 99, 1024]);
        assert_eq!(a, b);
        assert_eq!(input, rate as u64);
        assert!((98..=100).contains(&frames));
        assert!((a.len() as i64 - out as i64).abs() < out as i64 / 30);
        assert!(a.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
    }
}

#[test]
fn limiter_also_protects_a_resampled_bypass() {
    let mut dsp = StreamDsp::new(44_100, 48_000, Strength::Strong, true).unwrap();
    let mut count = 0;
    for i in 0..4410 {
        dsp.push(if i % 2 == 0 { 6.0 } else { -6.0 }, |sample| {
            count += 1;
            assert!(sample.is_finite() && sample.abs() <= 1.0);
        })
        .unwrap();
    }
    assert!(count > 4000);
}

#[test]
fn fake_capture_worker_and_render_callbacks_run_without_underflow_after_priming() {
    let signals = Arc::new(Signals::default());
    let (mut capture, consumer) = CaptureCallback::new(2400, 1, signals.clone()).unwrap();
    let (producer, mut render) = OutputCallback::new(2400, 2, 960, signals.clone()).unwrap();
    let dsp = StreamDsp::new(48_000, 48_000, Strength::Natural, true).unwrap();
    let mut pump = Pump::new(consumer, producer, dsp, signals.clone(), 2400, 960);
    let mut output = [0.0; 960];
    for _ in 0..60 {
        capture.capture(&[0.25; FRAME], |x| x);
        assert_eq!(pump.process_available(1920), FRAME);
        render.render(&mut output, |x| x);
    }
    assert_eq!(signals.underruns.load(Ordering::Relaxed), 0);
    assert_eq!(signals.input_dropped.load(Ordering::Relaxed), 0);
    assert_eq!(signals.output_dropped.load(Ordering::Relaxed), 0);
    assert!(output.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
    assert!(
        output
            .iter()
            .zip(output.iter().skip(1))
            .step_by(2)
            .all(|(a, b)| a == b)
    );
}

#[test]
fn independent_fake_clocks_are_compensated_with_bounded_queues() {
    for faster_input in [false, true] {
        let signals = Arc::new(Signals::default());
        let (mut capture, consumer) = CaptureCallback::new(2400, 1, signals.clone()).unwrap();
        let (producer, mut render) = OutputCallback::new(2400, 1, 960, signals.clone()).unwrap();
        let dsp = StreamDsp::new(48_000, 48_000, Strength::Natural, true).unwrap();
        let mut pump = Pump::new(consumer, producer, dsp, signals.clone(), 2400, 960);
        let input = [0.1; FRAME + 1];
        let mut output = [0.0; FRAME];
        // ±521 ppm for 20 seconds of virtual time. No sleeping or actual audio endpoints.
        for tick in 0..2000 {
            let frames = if tick % 4 == 0 {
                if faster_input { FRAME + 1 } else { FRAME - 1 }
            } else {
                FRAME
            };
            capture.capture(&input[..frames], |x| x);
            pump.process_available(1920);
            render.render(&mut output, |x| x);
        }
        assert_eq!(signals.input_dropped.load(Ordering::Relaxed), 0);
        assert_eq!(signals.output_dropped.load(Ordering::Relaxed), 0);
        assert_eq!(signals.underruns.load(Ordering::Relaxed), 0);
        assert_eq!(signals.discontinuities.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn underflow_silences_output_and_requests_a_fresh_timeline() {
    let signals = Arc::new(Signals::default());
    let (mut producer, mut callback) = OutputCallback::new(8, 1, 4, signals.clone()).unwrap();
    for _ in 0..4 {
        producer
            .push(Rendered {
                sample: 0.5,
                generation: 0,
            })
            .unwrap();
    }
    let mut output = [9.0; 8];
    callback.render(&mut output, |x| x);
    assert_eq!(&output[..4], &[0.5; 4]);
    assert_eq!(&output[4..], &[0.0; 4]);
    assert_eq!(signals.underruns.load(Ordering::Relaxed), 1);
    assert!(signals.reset_requested.load(Ordering::Acquire));
}

#[test]
fn output_preserves_interleaved_frames_across_partial_callback_slices() {
    let signals = Arc::new(Signals::default());
    let (mut producer, mut callback) = OutputCallback::new(32, 2, 2, signals).unwrap();
    for sample in [0.1, 0.2, 0.3, 0.4] {
        producer
            .push(Rendered {
                sample,
                generation: 0,
            })
            .unwrap();
    }
    let mut first = [0.0; 3];
    let mut second = [0.0; 5];
    callback.render(&mut first, |x| x);
    callback.render(&mut second, |x| x);
    assert_eq!(first, [0.1, 0.1, 0.2]);
    assert_eq!(second, [0.2, 0.3, 0.3, 0.4, 0.4]);
}

#[test]
fn stopped_callbacks_silence_without_consuming_or_queuing_audio() {
    let signals = Arc::new(Signals::default());
    let (mut capture, captured) = CaptureCallback::new(16, 1, signals.clone()).unwrap();
    let (mut producer, mut callback) = OutputCallback::new(32, 1, 2, signals.clone()).unwrap();
    for _ in 0..4 {
        producer
            .push(Rendered {
                sample: 0.5,
                generation: 0,
            })
            .unwrap();
    }
    signals.stop.store(true, Ordering::Release);
    let mut output = [8.0; 4];
    without_allocations(|| {
        capture.capture(&[0.5; 4], |x| x);
        callback.render(&mut output, |x| x);
    });
    assert!(captured.is_empty());
    assert_eq!(producer.slots(), 28);
    assert_eq!(output, [0.0; 4]);
}

#[test]
fn stale_generations_never_play_after_a_reset() {
    let signals = Arc::new(Signals::default());
    let (mut producer, mut callback) = OutputCallback::new(32, 1, 4, signals.clone()).unwrap();
    for _ in 0..8 {
        producer
            .push(Rendered {
                sample: 0.8,
                generation: 0,
            })
            .unwrap();
    }
    signals.generation.store(1, Ordering::Release);
    for _ in 0..8 {
        producer
            .push(Rendered {
                sample: 0.2,
                generation: 1,
            })
            .unwrap();
    }
    let mut output = [0.0; 4];
    callback.render(&mut output, |x| x);
    assert_eq!(output, [0.2; 4]);
}

#[test]
fn cancellation_stops_the_pump_and_device_loss_drops_all_owned_resources() {
    struct FakeEndpoint(Arc<AtomicUsize>);
    impl Endpoint for FakeEndpoint {
        fn pause_endpoint(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    impl Drop for FakeEndpoint {
        fn drop(&mut self) {
            self.0.fetch_add(10, Ordering::Relaxed);
        }
    }
    let signals = Arc::new(Signals::default());
    let counter = Arc::new(AtomicUsize::new(0));
    let worker_signals = signals.clone();
    let worker = std::thread::spawn(move || {
        while !worker_signals.stop.load(Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_millis(1));
        }
    });
    let runtime = Runtime {
        input: Some(FakeEndpoint(counter.clone())),
        output: Some(FakeEndpoint(counter.clone())),
        worker: Some(worker),
        signals: signals.clone(),
    };
    signals.errors.store(ERROR_INPUT, Ordering::Release);
    drop(runtime);
    assert_eq!(counter.load(Ordering::Relaxed), 22);
    assert!(signals.stop.load(Ordering::Acquire));
    let (_, capture) = CaptureCallback::new(480, 1, signals.clone()).unwrap();
    let (output, _) = OutputCallback::new(960, 1, 480, signals.clone()).unwrap();
    let mut pump = Pump::new(
        capture,
        output,
        StreamDsp::new(48_000, 48_000, Strength::Natural, false).unwrap(),
        signals,
        960,
        480,
    );
    assert_eq!(pump.process_available(100), 0);
}
