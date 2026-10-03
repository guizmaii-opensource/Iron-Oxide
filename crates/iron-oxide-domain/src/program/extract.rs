//! Finding the program in an AI assistant's answer (#108).
//!
//! A user asks their own AI assistant for a program and pastes its answer. Even when asked for
//! JSON only, assistants wrap it: Markdown code fences, a sentence before, a tip after, sometimes
//! the current program echoed back before the new one. [`extract_json`] finds the one program in
//! such a text and returns it as written. It only cuts the surrounding text away: what it returns
//! still goes through [`Program::from_json`](super::Program::from_json), so it never makes a
//! document valid that the validator would refuse.
//!
//! # The rule
//!
//! 1. **Candidates**: the complete top-level `{…}` blocks inside each code fence (```` ``` ```` or
//!    `~~~`), and those of the whole text.
//! 2. Only **program-shaped** candidates count: blocks with the program's [`SHAPE_KEYS`] as keys.
//!    A snippet such as `{"kg": 2.5}` in the prose is never chosen and never reported on. A block
//!    that is not a valid program (stray braces around the answer, `{"program": {…}}`) is looked
//!    into, [`MAX_DEPTH`] levels deep: a program inside it is a candidate, and a broken block
//!    around a program is not one itself.
//! 3. A program-shaped block that starts but never closes, with no complete program after it,
//!    means the answer was cut off: [`ExtractError::CutOff`], even if an earlier complete program
//!    (an echo of the current one) is there.
//! 4. Otherwise exactly one program-shaped candidate is the program (even if it is not valid
//!    JSON, so the validator reports its line and column); several are
//!    [`ExtractError::Several`]; none is [`ExtractError::NoJson`].
//!
//! The scan is a single pass with a depth counter (no recursion). Quotes are tracked only inside
//! braces, so prose quotes and apostrophes don't matter, while braces inside JSON strings are not
//! mistaken for structure. A block that never closes is rescanned from the next `{` (a stray `{`
//! in the prose must not hide the program after it), at most [`MAX_RESCANS`] times, so the work
//! stays linear in the input.

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;

use super::limits::MAX_DOCUMENT_BYTES;

/// Largest pasted text [`extract_json`] reads, in bytes: room for a document of
/// [`MAX_DOCUMENT_BYTES`] and a lot of prose around it.
pub const MAX_PASTE_BYTES: usize = 4 * MAX_DOCUMENT_BYTES;

/// The top-level keys that make a block program-shaped. Both are in the schema's `required` list
/// (tested); the others (`schema_version`, `rotation`) are left out so that a program missing one
/// of them is still found, and the validator says what is missing.
pub const SHAPE_KEYS: [&str; 2] = ["name", "days"];

/// How many times a scan restarts after a block that never closes.
const MAX_RESCANS: usize = 8;

/// How many levels of blocks that are not programs are looked into for a program.
const MAX_DEPTH: usize = 4;

/// Why no single program could be taken from a pasted text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtractError {
    /// Nothing but whitespace.
    Empty,
    /// Longer than [`MAX_PASTE_BYTES`].
    TooLong {
        /// The text's length, in bytes.
        bytes: usize,
    },
    /// No program-shaped block.
    NoJson,
    /// A program starts but never closes: the answer was cut off.
    CutOff,
    /// Several programs, and no way to tell which one is meant.
    Several {
        /// How many program-shaped blocks the text holds.
        count: usize,
    },
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("Paste your AI's answer first."),
            Self::TooLong { .. } => write!(
                f,
                "This text is too long: paste at most {} KiB.",
                MAX_PASTE_BYTES / 1024
            ),
            Self::NoJson => f.write_str(
                "No program found in this text. Ask your AI to answer with the program as JSON.",
            ),
            Self::CutOff => f.write_str(
                "The answer looks cut off. Ask your AI to send the complete JSON again.",
            ),
            Self::Several { count } => write!(
                f,
                "Your AI sent more than one program ({count}). Ask it to send only the new one."
            ),
        }
    }
}

impl std::error::Error for ExtractError {}

/// The program in a pasted text, as written: see the [module](self) for the rule.
///
/// # Errors
/// [`ExtractError`], with a message for the user.
pub fn extract_json(text: &str) -> Result<&str, ExtractError> {
    if text.len() > MAX_PASTE_BYTES {
        return Err(ExtractError::TooLong { bytes: text.len() });
    }
    if text
        .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .is_empty()
    {
        return Err(ExtractError::Empty);
    }
    let mut complete = BTreeSet::new();
    let mut unclosed = Vec::new();
    for region in fences(text)
        .into_iter()
        .chain(std::iter::once(0..text.len()))
    {
        scan(text, region, &mut complete, &mut unclosed);
    }
    let candidates = program_candidates(text, complete);
    let slice = |range: &Range<usize>| text.get(range.clone()).unwrap_or_default();
    // Cut off: a program-shaped block that never closes, after the last complete candidate. Its
    // keys are only looked for up to the end of its own region, so the check stays linear.
    let last_candidate = candidates.iter().map(|range| range.start).max();
    let cut_off = unclosed.iter().any(|&(start, end)| {
        last_candidate.is_none_or(|last| start > last)
            && has_shape_keys(text.get(start..end).unwrap_or_default())
    });
    match candidates.as_slice() {
        _ if cut_off => Err(ExtractError::CutOff),
        [only] => Ok(slice(only)),
        [] => Err(ExtractError::NoJson),
        several => Err(ExtractError::Several {
            count: several.len(),
        }),
    }
}

/// What a complete block is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A JSON object with the program's keys.
    Program,
    /// Not valid JSON, but with the program's keys: a broken program, or stray braces around one.
    Broken,
    /// Anything else.
    Other,
}

fn shape(block: &str) -> Shape {
    match serde_json::from_str::<serde_json::Value>(block) {
        Ok(serde_json::Value::Object(object))
            if SHAPE_KEYS.iter().all(|key| object.contains_key(*key)) =>
        {
            Shape::Program
        }
        Ok(_) => Shape::Other,
        Err(_) if has_shape_keys(block) => Shape::Broken,
        Err(_) => Shape::Other,
    }
}

/// The program candidates among the complete blocks, sorted: the programs, the inner programs of
/// blocks that are not programs (up to [`MAX_DEPTH`] levels; the inner blocks of a level are
/// disjoint, so each level reads the text at most once per region), and the broken blocks that
/// have no program inside.
fn program_candidates(text: &str, complete: BTreeSet<(usize, usize)>) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut programs = BTreeSet::new();
    let mut broken = Vec::new();
    let mut level: Vec<(usize, usize)> = complete.into_iter().collect();
    for depth in 0..=MAX_DEPTH {
        let mut inner = BTreeSet::new();
        for &(start, end) in &level {
            match shape(text.get(start..end).unwrap_or_default()) {
                Shape::Program => {
                    programs.insert((start, end));
                    continue;
                }
                Shape::Broken => broken.push((start, end)),
                Shape::Other => {}
            }
            if depth < MAX_DEPTH && end > start + 1 {
                scan_once(bytes, start + 1..end - 1, &mut inner);
            }
        }
        level = inner.into_iter().collect();
        if level.is_empty() {
            break;
        }
    }
    // A broken block around a program is stray braces, not a second candidate.
    let mut candidates: BTreeSet<(usize, usize)> = programs.iter().copied().collect();
    for (start, end) in broken {
        let holds_program = programs
            .range((start + 1, 0)..)
            .next()
            .is_some_and(|&(inner_start, _)| inner_start < end);
        if !holds_program {
            candidates.insert((start, end));
        }
    }
    candidates
        .into_iter()
        .map(|(start, end)| start..end)
        .collect()
}

/// The byte ranges of the contents of the code fences: from the line after an opening fence (three
/// or more backticks or tildes) to its closing fence (the same character, at least as many), or to
/// the end of the text when it never closes.
fn fences(text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut open: Option<(char, usize, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let trimmed = line.trim_start_matches([' ', '\t']);
        let Some(mark) = trimmed.chars().next().filter(|c| *c == '`' || *c == '~') else {
            continue;
        };
        let run = trimmed.chars().take_while(|c| *c == mark).count();
        if run < 3 {
            continue;
        }
        match open {
            None => open = Some((mark, run, offset)),
            Some((open_mark, open_run, content))
                if mark == open_mark && run >= open_run && trimmed[run..].trim().is_empty() =>
            {
                found.push(content..start);
                open = None;
            }
            Some(_) => {}
        }
    }
    if let Some((_, _, content)) = open {
        found.push(content..text.len());
    }
    found
}

/// Adds the top-level `{…}` blocks of `region` to `complete`, and the start of any block that
/// never closes to `unclosed`, rescanning after it from the next `{`.
fn scan(
    text: &str,
    region: Range<usize>,
    complete: &mut BTreeSet<(usize, usize)>,
    unclosed: &mut Vec<(usize, usize)>,
) {
    let bytes = text.as_bytes();
    let mut from = region.start;
    for _ in 0..=MAX_RESCANS {
        let Some(open) = scan_once(bytes, from..region.end, complete) else {
            return;
        };
        unclosed.push((open, region.end));
        match bytes
            .get(open + 1..region.end)
            .and_then(|rest| rest.iter().position(|byte| *byte == b'{'))
        {
            Some(next) => from = open + 1 + next,
            None => return,
        }
    }
}

/// One pass over `range`: its complete blocks, and the start of a last block that never closes.
fn scan_once(
    bytes: &[u8],
    range: Range<usize>,
    complete: &mut BTreeSet<(usize, usize)>,
) -> Option<usize> {
    let mut depth = 0_usize;
    let mut start = range.start;
    let mut in_string = false;
    let mut escaped = false;
    // Every byte that matters is ASCII, and UTF-8 never uses ASCII bytes inside a multi-byte
    // character, so a byte scan is exact and the ranges fall on character boundaries.
    for index in range {
        let Some(&byte) = bytes.get(index) else {
            break;
        };
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'{' => {
                if depth == 0 {
                    start = index;
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    complete.insert((start, index + 1));
                }
            }
            b'"' if depth > 0 => in_string = true,
            _ => {}
        }
    }
    (depth > 0).then_some(start)
}

/// Whether every one of the [`SHAPE_KEYS`] appears in `text` as a key: `"name"` then `:`.
fn has_shape_keys(text: &str) -> bool {
    SHAPE_KEYS.iter().all(|key| {
        let quoted = format!("\"{key}\"");
        text.match_indices(&quoted).any(|(at, _)| {
            text.get(at + quoted.len()..)
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROGRAM: &str = r#"{
  "schema_version": 1,
  "name": "Mine",
  "days": [{ "id": "a", "name": "A", "exercises": [
    { "id": "squat", "name": "Squat", "work": { "reps": { "sets": 3, "reps": 5 } }, "rest": 120 }
  ] }],
  "rotation": ["a"]
}"#;

    const OTHER: &str = r#"{"schema_version": 1, "name": "Old", "days": [{"id": "a", "name": "A",
  "exercises": [{"id": "row", "name": "Row", "work": {"reps": {"sets": 3, "reps": 8}}, "rest": 90}]}],
  "rotation": ["a"]}"#;

    fn cut() -> &'static str {
        PROGRAM.get(..PROGRAM.len() * 3 / 5).unwrap()
    }

    #[test]
    fn a_bare_document_is_taken_as_it_is() {
        assert_eq!(extract_json(PROGRAM), Ok(PROGRAM));
        let padded = format!("\n\n  {PROGRAM}\n ");
        assert_eq!(extract_json(&padded), Ok(PROGRAM));
        assert_eq!(extract_json(&format!("\u{feff}{PROGRAM}")), Ok(PROGRAM));
        assert_eq!(
            extract_json(&PROGRAM.replace('\n', "\r\n")).map(str::len),
            Ok(PROGRAM.replace('\n', "\r\n").len())
        );
    }

    #[test]
    fn fences_and_prose_are_cut_away() {
        for wrapped in [
            format!("```json\n{PROGRAM}\n```"),
            format!("```\n{PROGRAM}\n```"),
            format!("~~~json\n{PROGRAM}\n~~~"),
            format!(
                "Here is your program:\n\n```json\n{PROGRAM}\n```\n\nGood luck, and don't skip the warm-ups!"
            ),
            format!(
                "Sure! It's a 3-day \"full body\" plan.\n{PROGRAM}\nLet me know if you'd like changes."
            ),
            // Four-backtick fences around three-backtick ones.
            format!("````\n```json\n{PROGRAM}\n```\n````"),
        ] {
            assert_eq!(extract_json(&wrapped), Ok(PROGRAM), "{wrapped}");
        }
    }

    #[test]
    fn braces_quotes_and_fences_inside_strings_are_text() {
        let tricky =
            r#"{"name": "A } tricky { \"name\" ```json``` \\", "days": "{}{}", "x": ["}"]}"#;
        assert_eq!(extract_json(tricky), Ok(tricky));
        let wrapped = format!("```json\n{tricky}\n```\nThat's it }}");
        assert_eq!(extract_json(&wrapped), Ok(tricky));
        let accented = r#"{"name": "Séance — jambes 💪", "days": []}"#;
        assert_eq!(
            extract_json(&format!("Voilà « ton » programme : {accented} 🙂")),
            Ok(accented)
        );
    }

    #[test]
    fn snippets_that_are_not_programs_are_never_chosen() {
        for prose in [
            "Loads are written as {kg} or {lb}, e.g. {like this}.",
            r#"An increment looks like {"kg": 2.5}."#,
            r#"Days look like {"id": "a", "name": "Day A"}."#,
        ] {
            let text = format!("{prose}\n```json\n{PROGRAM}\n```");
            assert_eq!(extract_json(&text), Ok(PROGRAM), "{prose}");
        }
        // Only snippets: no program, and nothing to report on them.
        assert_eq!(
            extract_json(r#"Use {"kg": 2.5} or {"lb": 5}."#),
            Err(ExtractError::NoJson)
        );
        assert_eq!(extract_json("{a} and {b}"), Err(ExtractError::NoJson));
    }

    #[test]
    fn a_broken_program_is_returned_for_the_validator_even_beside_a_valid_snippet() {
        let broken = r#"{"schema_version": 1, "name": "Mine", "days": [],}"#;
        assert_eq!(extract_json(&format!("```json\n{broken}\n```")), Ok(broken));
        // Review of #111: the valid snippet used to win, and its "unknown field" was reported.
        assert_eq!(
            extract_json(&format!(
                "Increments look like {{\"kg\": 2.5}}.\n```json\n{broken}\n```"
            )),
            Ok(broken)
        );
        assert_eq!(extract_json(&format!("Use {{kg}}.\n{broken}")), Ok(broken));
    }

    #[test]
    fn several_programs_are_ambiguous() {
        assert_eq!(
            extract_json(&format!("{PROGRAM}\n\nAnd again:\n{PROGRAM}")),
            Err(ExtractError::Several { count: 2 })
        );
        assert_eq!(
            extract_json(&format!(
                "Your current program:\n{OTHER}\nThe new one:\n```json\n{PROGRAM}\n```"
            )),
            Err(ExtractError::Several { count: 2 })
        );
        assert_eq!(
            extract_json(&format!("[{PROGRAM}, {OTHER}]")),
            Err(ExtractError::Several { count: 2 })
        );
    }

    #[test]
    fn a_cut_off_program_says_so_even_after_an_echoed_one() {
        assert_eq!(
            extract_json(&format!("```json\n{}", cut())),
            Err(ExtractError::CutOff)
        );
        // Review of #111: the echoed current program used to be taken instead.
        assert_eq!(
            extract_json(&format!(
                "Your current program:\n{OTHER}\n\nHere is the updated one:\n```json\n{}",
                cut()
            )),
            Err(ExtractError::CutOff)
        );
        assert_eq!(
            extract_json(&format!("Use {{kg}}.\n{}", cut())),
            Err(ExtractError::CutOff)
        );
        // An unclosed string swallows the rest of a program.
        assert_eq!(
            extract_json(r#"{"name": "x", "days": [], "notes": "never closed }"#),
            Err(ExtractError::CutOff)
        );
    }

    #[test]
    fn a_stray_brace_in_the_prose_does_not_mean_cut_off() {
        // Review of #111, including the fuzzer's minimal case `}{`.
        for before in [
            "}{",
            "Note: the JSON starts with a `{`.",
            "(the document opens with \"{\"):",
            "{ {{ {",
            "Here {\"",
        ] {
            for text in [
                format!("{before}\n```json\n{PROGRAM}\n```"),
                format!("{before}\n{PROGRAM}"),
                format!("{PROGRAM}\n{before}"),
            ] {
                assert_eq!(extract_json(&text), Ok(PROGRAM), "{text}");
            }
        }
        // A stray brace without a program is just no program.
        assert_eq!(
            extract_json("Here is a { for you"),
            Err(ExtractError::NoJson)
        );
    }

    #[test]
    fn text_without_a_program_is_refused() {
        assert_eq!(extract_json(""), Err(ExtractError::Empty));
        assert_eq!(extract_json(" \n\t "), Err(ExtractError::Empty));
        assert_eq!(extract_json("\u{feff}"), Err(ExtractError::Empty));
        assert_eq!(
            extract_json("I can't help with that. What are your goals?"),
            Err(ExtractError::NoJson)
        );
        assert_eq!(extract_json("} oops ]"), Err(ExtractError::NoJson));
        assert_eq!(extract_json("[1, 2, 3]"), Err(ExtractError::NoJson));
        assert_eq!(extract_json("{}"), Err(ExtractError::NoJson));
    }

    #[test]
    fn huge_or_deep_input_is_bounded() {
        let huge = "a".repeat(MAX_PASTE_BYTES + 1);
        assert_eq!(
            extract_json(&huge),
            Err(ExtractError::TooLong {
                bytes: MAX_PASTE_BYTES + 1
            })
        );
        let prose = "lorem ipsum \"quoted\" it's ".repeat(MAX_PASTE_BYTES / 64);
        let text = format!("{prose}{PROGRAM}{prose}");
        assert!(text.len() <= MAX_PASTE_BYTES);
        assert_eq!(extract_json(&text), Ok(PROGRAM));
        // Deep nesting: no recursion here; serde's own depth limit stops the parse check.
        let open = "{".repeat(100_000);
        assert_eq!(extract_json(&open), Err(ExtractError::NoJson));
        let deep = format!("{open}{}", "}".repeat(100_000));
        assert_eq!(extract_json(&deep), Err(ExtractError::NoJson));
        // Many tiny blocks, and many fences.
        assert_eq!(
            extract_json(&"{} ".repeat(100_000)),
            Err(ExtractError::NoJson)
        );
        let fenced = "```\n{}\n```\n".repeat(20_000);
        assert_eq!(extract_json(&fenced), Err(ExtractError::NoJson));
    }

    /// Fix round of #111: every fence used to rescan the rest of the text (12.9 s for 1 MiB).
    #[test]
    fn many_fences_take_linear_time() {
        let started = std::time::Instant::now();
        // Each fence opens a block that never closes and has no program keys.
        let unit = "```\n{\n```\n";
        let text = unit.repeat(MAX_PASTE_BYTES / unit.len());
        assert!(text.len() > MAX_PASTE_BYTES - unit.len());
        assert_eq!(extract_json(&text), Err(ExtractError::NoJson));
        let unit = "```json\n{\"name\": \"x\", \"days\": [\n```\n";
        let text = unit.repeat(MAX_PASTE_BYTES / unit.len());
        assert_eq!(extract_json(&text), Err(ExtractError::CutOff));
        let closed = "```json\n{\"name\": \"x\", \"days\": []}\n```\n";
        let text = closed.repeat(MAX_PASTE_BYTES / closed.len());
        assert!(matches!(
            extract_json(&text),
            Err(ExtractError::Several { .. })
        ));
        let elapsed = started.elapsed();
        // Linear: tens of milliseconds even in a debug build; quadratic took seconds in release.
        assert!(elapsed.as_secs_f64() < 2.0, "{elapsed:?}");
    }

    /// Fix round of #111: a stray `{` before and `}` after made one block around the program.
    #[test]
    fn a_program_wrapped_in_stray_braces_is_found() {
        for text in [
            format!("{{\n```json\n{PROGRAM}\n```\n}}"),
            format!("{{ Here you go:\n{PROGRAM}\nEnjoy }}"),
            format!("{{{{ {PROGRAM} }}}}"),
            format!("{{\"program\": {PROGRAM}}}"),
        ] {
            assert_eq!(extract_json(&text), Ok(PROGRAM), "{text}");
        }
        // Two programs inside the same stray braces are still two.
        assert_eq!(
            extract_json(&format!("{{ {PROGRAM}\n{OTHER} }}")),
            Err(ExtractError::Several { count: 2 })
        );
        assert_eq!(
            extract_json(&format!(
                "{{\n```json\n{PROGRAM}\n```\n```json\n{OTHER}\n```\n}}"
            )),
            Err(ExtractError::Several { count: 2 })
        );
    }

    #[test]
    fn errors_read_as_sentences() {
        assert_eq!(
            ExtractError::TooLong { bytes: 1 }.to_string(),
            "This text is too long: paste at most 1024 KiB."
        );
        assert_eq!(
            ExtractError::Several { count: 2 }.to_string(),
            "Your AI sent more than one program (2). Ask it to send only the new one."
        );
        assert!(
            ExtractError::CutOff
                .to_string()
                .starts_with("The answer looks cut off")
        );
        assert!(
            ExtractError::NoJson
                .to_string()
                .starts_with("No program found")
        );
    }

    #[test]
    fn the_shape_keys_are_required_by_the_schema() {
        let schema: serde_json::Value =
            serde_json::from_str(super::super::PROGRAM_SCHEMA_JSON).unwrap();
        let required = schema["required"].as_array().unwrap();
        for key in SHAPE_KEYS {
            assert!(required.iter().any(|value| value == key), "{key}");
        }
    }
}
