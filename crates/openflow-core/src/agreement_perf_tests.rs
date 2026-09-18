use super::*;
use std::hint::black_box;
use std::time::Instant;

// Previous production implementation: an exact oracle, not an approximation.
fn reference_distance(a: &[String], b: &[String]) -> usize {
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, x) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + usize::from(x != y));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

fn reference_disagreement(preview: &str, take: &str) -> f64 {
    let preview = tokens(preview);
    let take = tokens(take);
    if take.is_empty() {
        return if preview.is_empty() { 0.0 } else { 1.0 };
    }
    (reference_distance(&preview, &take) as f64 / take.len() as f64).min(1.0)
}

#[test]
fn agreement_optimization_preserves_exact_scores() {
    for (preview, take) in [
        ("", ""),
        ("", "tail"),
        ("prefix", ""),
        ("one two three", "one two three four"),
        ("one two three four", "one two three"),
        ("a b a b a", "a b a"),
        ("start wrong end", "start corrected end"),
        ("word word word", "word other word"),
        ("相同开头旧结尾", "相同开头新结尾"),
        ("Let's try İSTANBUL!", "let's try istanbul"),
    ] {
        assert_eq!(
            disagreement(preview, take),
            reference_disagreement(preview, take)
        );
    }
    let vocabulary = ["a", "b", "c", "界", "İ"];
    let mut seed = 0x43c4_ab59_u64;
    for case in 0..1200 {
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 32) as usize
        };
        let left: Vec<String> = (0..case % 67)
            .map(|_| vocabulary[next() % vocabulary.len()].to_string())
            .collect();
        let right: Vec<String> = (0..next() % 71)
            .map(|_| vocabulary[next() % vocabulary.len()].to_string())
            .collect();
        let expected = reference_distance(&left, &right);
        assert_eq!(edit_distance(&left, &right), expected);
        assert_eq!(edit_distance(&right, &left), expected);
    }
}

#[test]
#[ignore = "opt-in synthetic performance comparison, no microphone or provider"]
fn benchmark_agreement_compare() {
    let words: Vec<String> = (0..1600).map(|i| format!("token{i}")).collect();
    let whole = words.join(" ");
    let prefix = words[..1440].join(" ");
    let mut corrected = words.clone();
    corrected[800] = "correction".to_string();
    let corrected = corrected.join(" ");
    let unrelated = (0..400)
        .map(|i| format!("different{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let cases = [
        (
            "short",
            "send the note to the team",
            "send the note to the team today",
            100,
        ),
        ("identical_1600", whole.as_str(), whole.as_str(), 2),
        ("prefix_1440_1600", prefix.as_str(), whole.as_str(), 2),
        ("one_correction_1600", whole.as_str(), corrected.as_str(), 2),
        ("unrelated_1440_400", prefix.as_str(), unrelated.as_str(), 2),
    ];
    for (name, preview, take, iterations) in cases {
        assert_eq!(
            disagreement(preview, take),
            reference_disagreement(preview, take)
        );
        let mut samples = [Vec::new(), Vec::new()];
        for round in 0..10 {
            for variant in [round % 2, 1 - round % 2] {
                let function = if variant == 0 {
                    reference_disagreement
                } else {
                    disagreement
                };
                let started = Instant::now();
                for _ in 0..iterations {
                    black_box(function(black_box(preview), black_box(take)));
                }
                if round > 0 {
                    samples[variant].push(started.elapsed().as_nanos() as f64 / iterations as f64);
                }
            }
        }
        for values in &mut samples {
            values.sort_by(f64::total_cmp);
        }
        println!(
            "agreement {name}: reference_ns={:.0} current_ns={:.0}",
            samples[0][4], samples[1][4]
        );
    }
}
