use microphone_cleaner::{
    config::Strength,
    dsp::{FRAME, NeuralFrame},
};
use std::time::Instant;

fn main() {
    // Memory-only generated signals. Never construct an audio host, endpoint, UI, or updater.
    let mut processor = NeuralFrame::new(Strength::Strong, false);
    let mut output = [0.0; FRAME];
    let mut random = 0x12345678u32;
    let mut timings = Vec::with_capacity(1000);
    for frame in 0..1020 {
        let input = std::array::from_fn(|sample| {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            let noise = (random as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32;
            let tone =
                ((frame * FRAME + sample) as f32 * std::f32::consts::TAU * 240.0 / 48_000.0).sin();
            noise * 0.04 + tone * 0.12
        });
        let start = Instant::now();
        processor.process(&input, &mut output);
        if frame >= 20 {
            timings.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        std::hint::black_box(output);
    }
    timings.sort_by(f64::total_cmp);
    println!(
        "Synthetic CPU fixture: {} frames / 10 seconds of generated audio",
        timings.len()
    );
    println!(
        "RNNoise frame time: median {:.3} ms, p95 {:.3} ms, maximum {:.3} ms",
        timings[500], timings[950], timings[999]
    );
    println!(
        "Neural algorithmic delay: 10 ms. This does not measure device/cable/call latency or listening quality."
    );
}
