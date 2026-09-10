//! JSON-lines adapter for evaluation of the actual app dictionary code.
//! Nothing is inserted, persisted, or sent to a provider.
use std::io::{self, BufRead};

#[derive(serde::Deserialize)]
struct Input {
    text: String,
    dictionary: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for line in io::stdin().lock().lines() {
        let input: Input = serde_json::from_str(&line?)?;
        let text = openflow_core::postpass::apply(&input.text, Some(&input.dictionary));
        println!("{}", serde_json::to_string(&text)?);
    }
    Ok(())
}
