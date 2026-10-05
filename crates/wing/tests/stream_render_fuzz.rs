//! Pathological-corpus fuzz for the streaming/reference invariant.
//! Reports divergences to the terminal on purpose: the printout *is* the
//! artifact a failing run hands to whoever ran it.
#![allow(clippy::print_stdout)]
//!
//! Deterministic (seeded LCG over a small piece vocabulary) search for inputs
//! where the incremental engine's resting state differs from `full_lines` —
//! the invariant `reconcile_matrix_shapes` pins for handcrafted shapes.
//!
//! **Ignored by default, and expected to be non-empty while the documented
//! boundaries in `docs/dev/tui-rendering.md` §4 remain** (mixed CRLF /
//! indentation / blank-line slices). Run it when touching the splitter or the
//! code cache to see the current landscape, and check whether a change moved
//! the count in either direction:
//!
//! ```text
//! cargo test -p wing --test stream_render_fuzz -- --ignored --nocapture
//! ```

use wing::config::ThemePalette;
use wing::render::markdown::Profile;
use wing::render::markdown::stream::{StreamingRender, full_lines};

fn pieces() -> Vec<&'static str> {
    vec![
        "para",
        "",
        "",
        "- item",
        "  cont",
        "    ind",
        "1.",
        "-  ",
        "```",
        "```rust",
        "  ```",
        "code",
        "  code",
        "after",
        "> quote",
        "1. one",
        "| A | B |",
        "|---|---|",
        "text:```py",
        "# head",
        "  ",
    ]
}

#[test]
#[ignore = "diagnostic corpus: see the module docs and docs/dev/tui-rendering.md §4"]
fn fuzz_pathological_corpus() {
    let palette = ThemePalette::default();
    // Deterministic LCG so a failure is reproducible.
    let mut seed: u64 = 0x2545F4914F6CDD1D;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    let pieces = pieces();
    let mut divergences = 0usize;
    let mut panics = 0usize;
    for case in 0..400 {
        let n = 3 + next() % 10;
        let mut text = String::new();
        for _ in 0..n {
            text.push_str(pieces[next() % pieces.len()]);
            // Mix LF and CRLF endings.
            text.push_str(if next() % 3 == 0 { "\r\n" } else { "\n" });
        }
        for profile in [Profile::Thinking, Profile::Content] {
            for chunk in [1usize, 5, 64] {
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut sr = StreamingRender::new(profile);
                    let mut pos = 0;
                    while pos < text.len() {
                        let mut end = (pos + chunk).min(text.len());
                        while !text.is_char_boundary(end) {
                            end += 1;
                        }
                        sr.push(&text[pos..end]);
                        let _ = sr.lines(80, &palette);
                        pos = end;
                    }
                    let got: Vec<String> = sr
                        .lines(80, &palette)
                        .iter()
                        .map(|l| l.to_string())
                        .collect();
                    let want: Vec<String> = full_lines(&text, 80, profile, &palette)
                        .iter()
                        .map(|l| l.to_string())
                        .collect();
                    got == want
                }));
                match run {
                    Ok(true) => {}
                    Ok(false) => {
                        divergences += 1;
                        if divergences <= 5 {
                            println!(
                                "--- divergence case {case} {profile:?} chunk={chunk}: {text:?}"
                            );
                        }
                    }
                    Err(_) => {
                        panics += 1;
                        println!("--- panic case {case} {profile:?} chunk={chunk}: {text:?}");
                    }
                }
            }
        }
    }
    println!("{divergences} divergences, {panics} panics over 400×2×3 runs");
    assert_eq!(panics, 0, "the engine must never panic on any input");
}
