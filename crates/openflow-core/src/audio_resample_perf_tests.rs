//! Cycle 3: exact resampling oracle and opt-in synthetic CPU measurements.
use super::*;

// Frozen pre-cycle-3 loop. Filter design, interpolation and accumulation order
// are intentionally identical; this cycle only moves bounds checks.
fn reference_downsample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate {
        return samples.to_vec();
    }
    if samples.is_empty() || from_rate == 0 || to_rate == 0 {
        return Vec::new();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let output_len = (samples.len() as f64 / ratio) as usize;
    if output_len == 0 {
        return Vec::new();
    }
    if from_rate < to_rate {
        let mut output = Vec::with_capacity(output_len);
        for i in 0..output_len {
            let position = i as f64 * ratio;
            let left = position.floor() as usize;
            if left >= samples.len() {
                break;
            }
            let right = (left + 1).min(samples.len() - 1);
            let fraction = (position - left as f64) as f32;
            output.push(samples[left] + (samples[right] - samples[left]) * fraction);
        }
        return output;
    }
    let taps = design_lowpass(0.45 * to_rate as f32, from_rate as f32, FIR_TAPS);
    let half = (taps.len() / 2) as isize;
    let mut output = Vec::with_capacity(output_len);
    for i in 0..output_len {
        let center = (i as f64 * ratio) as isize;
        let mut acc = 0.0_f32;
        for (k, &tap) in taps.iter().enumerate() {
            let index = center + k as isize - half;
            if index >= 0 && (index as usize) < samples.len() {
                acc += samples[index as usize] * tap;
            }
        }
        output.push(acc);
    }
    output
}

fn reference_partial(captured: &[f32], rate: u32) -> Result<Vec<u8>, String> {
    let mut prepared = reference_downsample(captured, rate, 16_000);
    if prepared.len() < 800 {
        return Err("Not enough audio yet".to_string());
    }
    // Both sides use the already-reviewed cycle-2 gate/gain. This isolates
    // resampling, rather than counting the previous cycle's gain a second time.
    if !gain_if_audible(&mut prepared) {
        return Err("Nothing to preview yet".to_string());
    }
    encode_wav(&prepared, 16_000)
}

fn fixture(length: usize) -> Vec<f32> {
    let mut seed = 0x495a_2121_u32;
    (0..length)
        .map(|_| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / 16_777_216.0 - 0.5
        })
        .collect()
}

// The upstream streaming filter's original per-tap lookup, evaluated against
// the captured prefix. Unlike the batch oracle it sums padded zero terms too.
fn reference_streamed(input: &[f32], rate: u32, finish: bool) -> Vec<f32> {
    let taps = design_lowpass(0.45 * 16_000.0, rate as f32, FIR_TAPS);
    let half = taps.len() / 2;
    let ratio = rate as f64 / 16_000.0;
    (0..(input.len() as f64 / ratio) as usize)
        .map(|i| (i as f64 * ratio) as usize)
        .take_while(|center| finish || center + half < input.len())
        .map(|center| {
            taps.iter()
                .enumerate()
                .map(|(k, tap)| {
                    let index = center as isize + k as isize - half as isize;
                    if index < 0 {
                        0.0
                    } else {
                        input.get(index as usize).copied().unwrap_or(0.0) * tap
                    }
                })
                .sum()
        })
        .collect()
}

#[test]
fn streaming_fast_windows_preserve_upstream_bits_after_tail_compaction() {
    for input in [fixture(4097), vec![-0.0; 129], {
        let mut impulse = vec![0.0; 129];
        impulse[0] = 1.0;
        impulse[64] = -1.0;
        impulse[128] = 1.0;
        impulse
    }] {
        for rate in [44_100, 48_000, 96_000] {
            for block in [1, 31, 63, 257, 4096] {
                let mut streamed = StreamingResampler::new(rate, 16_000);
                let mut consumed = 0;
                for chunk in input.chunks(block) {
                    streamed.append(chunk, false);
                    consumed += chunk.len();
                    let expected = reference_streamed(&input[..consumed], rate, false);
                    assert_eq!(
                        streamed
                            .output
                            .iter()
                            .map(|s| s.to_bits())
                            .collect::<Vec<_>>(),
                        expected.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
                        "rate={rate}, block={block}, consumed={consumed}"
                    );
                }
                streamed.append(&[], true);
                let expected = reference_streamed(&input, rate, true);
                assert_eq!(
                    streamed
                        .output
                        .iter()
                        .map(|s| s.to_bits())
                        .collect::<Vec<_>>(),
                    expected.iter().map(|s| s.to_bits()).collect::<Vec<_>>()
                );
                assert_eq!(
                    encode_wav_with_gain(&streamed.output, 16_000, 2.0),
                    encode_wav_with_gain(&expected, 16_000, 2.0)
                );
            }
        }
    }
}

fn assert_same_bits(input: &[f32], from: u32, to: u32) {
    let actual = downsample(input, from, to);
    let expected = reference_downsample(input, from, to);
    assert_eq!(actual.len(), expected.len());
    for (index, (a, b)) in actual.iter().zip(&expected).enumerate() {
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "rate={from}->{to} len={} index={index}",
            input.len()
        );
    }
}

#[test]
fn resampler_preserves_every_sample_at_boundaries_and_rates() {
    let input = fixture(4096);
    for (from, to) in [
        (0, 0),
        (0, 16000),
        (16000, 0),
        (16000, 16000),
        (8000, 16000),
        (44100, 16000),
        (48000, 16000),
        (96000, 16000),
        (48000, 44100),
    ] {
        for length in (0..=128).chain([799, 800, 801, 4096]) {
            assert_same_bits(&input[..length], from, to);
        }
    }
    for length in [1, 31, 62, 63, 64, 65, 128, 1024] {
        for position in [0, length / 2, length - 1] {
            let mut impulse = vec![0.0; length];
            impulse[position] = 1.0;
            assert_same_bits(&impulse, 48000, 16000);
        }
    }
    assert_same_bits(&vec![-0.0; 1000], 48000, 16000);
}

#[test]
fn resampling_preserves_final_wav_and_preview_errors() {
    for rate in [8000, 16000, 44100, 48000, 96000] {
        let input = fixture(rate as usize);
        for length in [0, rate as usize / 20 - 1, rate as usize / 20, rate as usize] {
            assert_eq!(
                encode_partial(&input[..length], rate),
                reference_partial(&input[..length], rate)
            );
        }
        let silence = vec![0.0; rate as usize];
        assert_eq!(
            encode_partial(&silence, rate),
            reference_partial(&silence, rate)
        );
    }
}

#[test]
#[ignore = "synthetic resampler benchmark; run exact filter without microphone or provider"]
fn benchmark_resample_compare() {
    use std::{hint::black_box, time::Instant};
    for rate in [44_100, 48_000, 96_000] {
        for seconds in [1, 30, 180] {
            let input = fixture(rate as usize * seconds);
            assert_same_bits(&input, rate, 16000);
            assert_eq!(
                encode_partial(&input, rate),
                reference_partial(&input, rate)
            );
            for stage in ["resample", "full-partial"] {
                let measure = |reference: bool| {
                    let start = Instant::now();
                    if stage == "resample" {
                        let function = if reference {
                            reference_downsample
                        } else {
                            downsample
                        };
                        drop(black_box(function(black_box(&input), rate, 16000)));
                    } else {
                        let function = if reference {
                            reference_partial
                        } else {
                            encode_partial
                        };
                        drop(black_box(function(black_box(&input), rate)));
                    }
                    start.elapsed().as_secs_f64() * 1000.0
                };
                let mut samples = [Vec::new(), Vec::new()];
                for round in 0..10 {
                    for variant in [round % 2, 1 - round % 2] {
                        let elapsed = measure(variant == 0);
                        if round > 0 {
                            samples[variant].push(elapsed);
                        }
                    }
                }
                for values in &mut samples {
                    values.sort_by(f64::total_cmp);
                }
                println!("resample {stage} rate={rate} seconds={seconds} median_reference_ms={:.6} median_current_ms={:.6}",samples[0][4],samples[1][4]);
            }
        }
    }
}
