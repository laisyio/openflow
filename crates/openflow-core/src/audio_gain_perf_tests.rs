//! Synthetic-only gate/gain equivalence and opt-in comparative benchmarks.
//! Never constructs AudioRecorder or accesses a device, provider, or history.
use super::*;

// Preserved pre-cycle-2 implementation. Keep this independent of production
// gain helpers so a changed threshold or p95 calculation cannot update both
// sides of the oracle together.
fn legacy_speech_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut magnitudes: Vec<f32> = samples.iter().map(|sample| sample.abs()).collect();
    let index = ((magnitudes.len() as f32 * 0.95) as usize).min(magnitudes.len() - 1);
    let (_, level, _) = magnitudes.select_nth_unstable_by(index, |a, b| {
        a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
    });
    *level
}

fn legacy_auto_gain(samples: &[f32]) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }
    let level = legacy_speech_level(samples);
    if level < 1e-4 {
        return samples.to_vec();
    }
    let gain = (0.21 / level).clamp(1.0, 20.0);
    samples
        .iter()
        .map(|sample| (sample * gain).clamp(-1.0, 1.0))
        .collect()
}

fn legacy_post_resample(samples: Vec<f32>) -> Result<Vec<u8>, String> {
    if legacy_speech_level(&samples) < 1e-3 {
        return Err("Nothing to preview yet".to_string());
    }
    encode_wav(&legacy_auto_gain(&samples), 16_000)
}

fn current_post_resample(mut samples: Vec<f32>) -> Result<Vec<u8>, String> {
    if !gain_if_audible(&mut samples) {
        return Err("Nothing to preview yet".to_string());
    }
    encode_wav(&samples, 16_000)
}

fn legacy_partial(captured: &[f32], rate: u32) -> Result<Vec<u8>, String> {
    let Some(samples) = prepare_take(captured, rate) else {
        return Err("Not enough audio yet".to_string());
    };
    legacy_post_resample(samples)
}

fn legacy_stopped(captured: &[f32], rate: u32, device: &str) -> Result<Vec<u8>, String> {
    if captured.is_empty() {
        return Err("No audio recorded. Check microphone permissions.".to_string());
    }
    let Some(samples) = prepare_take(captured, rate) else {
        return Err("Recording too short.".to_string());
    };
    if legacy_speech_level(&samples) < 1e-3 {
        return Err(format!(
            "No sound reached OpenFlow from \"{device}\". Pick a different microphone in Settings."
        ));
    }
    encode_wav(&legacy_auto_gain(&samples), 16_000)
}

fn current_stopped(captured: &[f32], rate: u32, device: &str) -> Result<Vec<u8>, String> {
    encode_stopped(captured, rate, device)
}

#[derive(Clone, Copy, Debug)]
enum Shape {
    Quiet,
    Normal,
    Transient,
    Silent,
}

fn fixture(shape: Shape, rate: u32, seconds: usize) -> Vec<f32> {
    let period = rate as usize / 200;
    (0..rate as usize * seconds)
        .map(|i| {
            // A deterministic, finite, speech-band triangle. No expensive random
            // generation or system audio is included in a timed measurement.
            let phase = (i % period) as f32 / period as f32;
            let wave = 1.0 - 4.0 * (phase - 0.5).abs();
            match shape {
                Shape::Quiet => wave * 0.004,
                Shape::Normal => wave * 0.3,
                Shape::Transient if i % (rate as usize) < 20 => 0.95,
                Shape::Transient => wave * 0.025,
                Shape::Silent => 0.0,
            }
        })
        .collect()
}

#[test]
fn gain_matches_legacy_sample_bits_for_finite_inputs_and_thresholds() {
    let mut cases = vec![
        vec![],
        vec![0.0, -0.0],
        vec![f32::MAX, -f32::MAX],
        vec![f32::MIN_POSITIVE; 801],
    ];
    for level in [
        0.00009999, 0.0001, 0.00010001, 0.0009999, 0.001, 0.0010001, 0.0105, 0.21, 1.0, 1.5,
    ] {
        cases.push(
            (0..1000)
                .map(|i| if i % 2 == 0 { level } else { -level })
                .collect(),
        );
    }
    for shape in [Shape::Quiet, Shape::Normal, Shape::Transient, Shape::Silent] {
        cases.push(fixture(shape, 16_000, 1));
    }
    for input in cases {
        assert_eq!(is_silent(&input), legacy_speech_level(&input) < 1e-3);
        let actual: Vec<_> = auto_gain(&input)
            .iter()
            .map(|sample| sample.to_bits())
            .collect();
        let expected: Vec<_> = legacy_auto_gain(&input)
            .iter()
            .map(|sample| sample.to_bits())
            .collect();
        assert_eq!(actual, expected);
        let mut owned = input.clone();
        let pointer = owned.as_ptr();
        let capacity = owned.capacity();
        let audible = gain_if_audible(&mut owned);
        assert_eq!(
            audible,
            legacy_speech_level(&input).partial_cmp(&1e-3) != Some(std::cmp::Ordering::Less)
        );
        let expected = if audible {
            legacy_auto_gain(&input)
        } else {
            input
        };
        assert_eq!(
            owned.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
            expected.iter().map(|s| s.to_bits()).collect::<Vec<_>>()
        );
        assert_eq!(owned.as_ptr(), pointer, "reuse the owned mono allocation");
        assert_eq!(owned.capacity(), capacity);
    }
}

#[test]
fn stop_and_preview_preserve_exact_wav_errors_duration_and_input() {
    for rate in [16_000, 48_000] {
        for shape in [Shape::Quiet, Shape::Normal, Shape::Transient, Shape::Silent] {
            let input = fixture(shape, rate, 1);
            let unchanged: Vec<_> = input.iter().map(|sample| sample.to_bits()).collect();
            for length in [
                0,
                1,
                rate as usize / 25,
                rate as usize / 20 - 1,
                rate as usize / 20,
                input.len(),
            ] {
                let take = &input[..length];
                let partial = encode_partial(take, rate);
                assert_eq!(
                    partial,
                    legacy_partial(take, rate),
                    "{shape:?} rate={rate} length={length}"
                );
                let stopped = current_stopped(take, rate, "Synthetic fixture");
                assert_eq!(stopped, legacy_stopped(take, rate, "Synthetic fixture"));
                if let Ok(wav) = partial {
                    assert_eq!(
                        wav_duration_ms(&wav),
                        Some((length * 1000 / rate as usize) as i64)
                    );
                    assert_eq!(stopped.unwrap(), wav);
                }
            }
            assert_eq!(
                input
                    .iter()
                    .map(|sample| sample.to_bits())
                    .collect::<Vec<_>>(),
                unchanged
            );
        }
    }
}

#[test]
#[ignore = "synthetic-only release comparison; run this exact filter, never all ignored tests"]
fn benchmark_audio_gain_compare() {
    use std::{
        hint::black_box,
        time::{Duration, Instant},
    };
    let mut cases = Vec::new();
    for seconds in [1, 30, 180] {
        for shape in [Shape::Quiet, Shape::Normal, Shape::Transient, Shape::Silent] {
            cases.push((shape, 16_000, seconds));
        }
    }
    // Include FIR-dominated end-to-end costs without benchmarking every long
    // shape twice; the post-resample cases isolate the changed gain work.
    cases.extend([
        (Shape::Quiet, 48_000, 1),
        (Shape::Normal, 48_000, 30),
        (Shape::Transient, 48_000, 180),
        (Shape::Silent, 48_000, 1),
    ]);
    for (shape, rate, seconds) in cases {
        let captured = fixture(shape, rate, seconds);
        assert_eq!(
            encode_partial(&captured, rate),
            legacy_partial(&captured, rate)
        );
        assert_eq!(
            current_stopped(&captured, rate, "Synthetic fixture"),
            legacy_stopped(&captured, rate, "Synthetic fixture")
        );
        let prepared = prepare_take(&captured, rate).unwrap();
        assert_eq!(
            current_post_resample(prepared.clone()),
            legacy_post_resample(prepared.clone())
        );
        for stage in ["partial", "post-resample"] {
            // Post-resample already covered for these same durations at16k.
            if stage == "post-resample" && rate == 48_000 {
                continue;
            }
            let iterations = if seconds == 1 { 4 } else { 1 };
            let measure = |legacy: bool| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    // Establish identical owned inputs outside the timer for
                    // the post-resample stage, which receives an owned buffer.
                    let owned = (stage == "post-resample").then(|| prepared.clone());
                    let start = Instant::now();
                    let output = if let Some(owned) = owned {
                        if legacy {
                            legacy_post_resample(black_box(owned))
                        } else {
                            current_post_resample(black_box(owned))
                        }
                    } else if legacy {
                        legacy_partial(black_box(&captured), rate)
                    } else {
                        encode_partial(black_box(&captured), rate)
                    };
                    black_box(output).ok();
                    elapsed += start.elapsed();
                }
                elapsed.as_secs_f64() * 1_000_000.0 / iterations as f64
            };
            let mut old = Vec::new();
            let mut current = Vec::new();
            for sample in 0..11 {
                let (before, after) = if sample % 2 == 0 {
                    (measure(true), measure(false))
                } else {
                    let after = measure(false);
                    (measure(true), after)
                };
                if sample >= 2 {
                    old.push(before);
                    current.push(after);
                }
            }
            old.sort_by(f64::total_cmp);
            current.sort_by(f64::total_cmp);
            println!("gain {stage} {shape:?} rate={rate} seconds={seconds} samples=9 warmups=2 legacy_us={:.3} current_us={:.3} legacy_p95_us={:.3} current_p95_us={:.3} speedup={:.3}x", old[4], current[4], old[8], current[8], old[4] / current[4]);
        }
    }
}
