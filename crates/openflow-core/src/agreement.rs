//! How far the live preview ended up from the take it was previewing.
//!
//! **This measures; it does not gate.** What to do about a recording the model
//! changed its mind about is a product decision, and this codebase has deleted a
//! silence gate twice for rejecting recordings people meant. So the number is
//! reported and nothing acts on it.
//!
//! ## Why the preview is worth comparing against at all
//!
//! Under noise Qwen3-ASR does not fail, it invents: on 24 noisy recordings
//! measured through the app's own capture path, 12 of the 1.7B takes came back
//! fluent, grammatical and completely wrong, and there is nothing in the
//! response to tell them apart -- the endpoints return an empty `segments`
//! array, so no `avg_logprob` and no `no_speech_prob`.
//!
//! There is one thing to compare against that costs nothing: [the preview]. It
//! runs the same model on a prefix of the same audio, so the pair is one model
//! asked twice. On a recording that transcribes cleanly the two agree almost
//! word for word; on one the model is inventing, the prefix and the whole take
//! produce unrelated fluent sentences:
//!
//! ```text
//! take     We measure the ability to obtain resources and services.
//! preview  We measured the luminities of 1000 stars.
//!
//! take     这台空调坏了，我得去维修。
//! preview  这台车的话，它有。
//! ```
//!
//! Over 65 takes whose final text was non-empty -- the ones that reach a
//! document, since `insert.rs` already drops the empty ones -- **11 of the 15
//! that came back more than half wrong** disagreed with their preview by more
//! than 70%, and **none of the 50 usable ones did**. Running a second, different
//! model over the whole take scores no better and costs an inference.
//!
//! That 11 is this function's own score on those 65 pairs, not the measuring
//! harness's: the harness canonicalises numbers and units before comparing, so
//! "sixteen thousand hertz" and "16,000 Hz" agree there and it caught 12. That
//! normalisation is worth having when scoring accuracy against a script; it is
//! not worth carrying here to move one recording.
//!
//! ## Why the coverage number travels with it
//!
//! That only holds while the preview is nearly as long as the take.
//! [`crate::engine::should_hold`] retires previewing the first time a reading
//! runs over `PARTIAL_INTERVAL`, and `retire_preview` leaves the last one on
//! screen, so the preview a take ends up with can be from very early in it --
//! the local runner reads a 1.7B in about 0.98 s, over the 800 ms budget on the
//! first reading. Measured against how much of the take the preview had seen:
//!
//! | preview covered | flagged of the bad | flagged of the usable |
//! |---|---|---|
//! | all but the last 0.8 s | 80% | 0% |
//! | 75% | 80% | 6% |
//! | 50% | 73% | 10% |
//! | 25% | 100% | 76% |
//!
//! At a quarter it is worse than useless, so the two numbers are only
//! meaningful together and are reported together.
//!
//! [the preview]: crate::engine::Engine

/// The words of `text`, for comparing two transcripts of the same audio.
///
/// Latin runs count as one token each and CJK counts per character, which is
/// how the same sentence in either script comes out to a comparable count.
/// Case and punctuation go: a preview that wrote "Let's" against a take that
/// wrote "let's" has not changed its mind about anything.
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut latin = String::new();
    for character in text.chars() {
        if is_cjk(character) {
            if !latin.is_empty() {
                out.push(std::mem::take(&mut latin));
            }
            out.push(character.to_string());
        } else if character.is_alphanumeric() || character == '\'' {
            latin.extend(character.to_lowercase());
        } else if !latin.is_empty() {
            out.push(std::mem::take(&mut latin));
        }
    }
    if !latin.is_empty() {
        out.push(latin);
    }
    out
}

fn is_cjk(character: char) -> bool {
    matches!(character, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}')
}

/// How much of `take` the `preview` does not account for, from 0.0 for word for
/// word to 1.0 for nothing in common.
///
/// Scaled by the take rather than by the longer of the two, because the take is
/// what will be inserted: a preview that stopped early is short by the words it
/// had not heard yet, and that is what the coverage number beside this is for.
pub fn disagreement(preview: &str, take: &str) -> f64 {
    let preview = tokens(preview);
    let take = tokens(take);
    if take.is_empty() {
        return if preview.is_empty() { 0.0 } else { 1.0 };
    }
    (edit_distance(&preview, &take) as f64 / take.len() as f64).min(1.0)
}

fn edit_distance(a: &[String], b: &[String]) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transcript_does_not_disagree_with_itself() {
        let text = "Let's ship the pull request after the review comments land.";
        assert_eq!(disagreement(text, text), 0.0);
        assert_eq!(
            disagreement("这台空调坏了，我得去维修。", "这台空调坏了，我得去维修。"),
            0.0
        );
    }

    #[test]
    fn case_and_punctuation_are_not_a_change_of_mind() {
        assert_eq!(
            disagreement(
                "let's ship the pull request",
                "Let's ship the pull request!"
            ),
            0.0
        );
    }

    /// The preview is always short by whatever was said in the last interval,
    /// so a tail of missing words has to read as a small number, not a large
    /// one. This is the shape of every clean recording in the measurement.
    #[test]
    fn a_preview_missing_only_its_tail_barely_disagrees() {
        let take = "Set the sample rate to sixteen thousand hertz and keep it mono";
        let preview = "Set the sample rate to sixteen thousand hertz and keep it";
        assert!(
            disagreement(preview, take) < 0.2,
            "{}",
            disagreement(preview, take)
        );
    }

    /// And the shape this exists to notice: two fluent sentences with nothing to
    /// do with each other, both from the same model on the same audio.
    #[test]
    fn two_unrelated_fluent_sentences_disagree_almost_completely() {
        let take = "We measure the ability to obtain resources and services.";
        let preview = "We measured the luminities of 1000 stars.";
        assert!(
            disagreement(preview, take) > 0.7,
            "{}",
            disagreement(preview, take)
        );

        let take = "这台空调坏了，我得去维修。";
        let preview = "这台车的话，它有。";
        assert!(
            disagreement(preview, take) > 0.7,
            "{}",
            disagreement(preview, take)
        );
    }

    #[test]
    fn a_preview_that_read_nothing_disagrees_completely() {
        assert_eq!(disagreement("", "a transcript that arrived"), 1.0);
        // But two silences agree, and `insert.rs` drops the empty take anyway.
        assert_eq!(disagreement("", ""), 0.0);
    }
}
