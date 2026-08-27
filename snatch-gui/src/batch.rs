//! Expanding one templated URL into the list of URLs it stands for.
//!
//! Numbered files are the everyday case a download manager exists for —
//! `page[001-250].jpg`, `disc[1-3].iso`, `log-[2020-2026].txt` — and typing
//! them out by hand is exactly the tedium worth automating.
//!
//! Three constructs, which compose:
//!
//! * `[001-250]` — a numeric range. The padding of the *first* bound sets the
//!   width, so `[001-250]` yields `001`, and `[1-250]` yields `1`.
//! * `[a-f]` — a letter range, either case.
//! * `{jpg,png,gif}` — an explicit list of alternatives.
//!
//! Several in one URL multiply out, so `shot[1-3]-{a,b}.png` is six URLs.
//!
//! Nothing here guesses. A bracket that is not a well-formed range is left
//! exactly as it was typed, because a URL is allowed to contain one and
//! silently mangling it would be worse than not expanding.

use anyhow::{Result, bail};

/// Refuse to build a list longer than this.
///
/// Not a technical limit — the queue would cope — but a typo like
/// `[1-100000000]` should be caught while it is still a dialog rather than
/// after it has filled the download list.
pub const MAX_EXPANSION: usize = 10_000;

/// One `[...]` or `{...}` found in a template.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    /// Where it starts and ends in the original string.
    start: usize,
    end: usize,
    /// Everything it stands for, already rendered.
    values: Vec<String>,
}

/// Does this look like a template rather than a plain URL?
///
/// Used to decide whether to offer expansion at all, so it has to agree with
/// [`expand`]: anything this accepts, that must be able to expand.
pub fn looks_like_pattern(text: &str) -> bool {
    !find_segments(text).is_empty()
}

/// Expand a template into every URL it stands for.
///
/// A string with no recognised construct expands to itself, so this is safe
/// to call on every line the user typed.
pub fn expand(template: &str) -> Result<Vec<String>> {
    let text = template.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }

    let segments = find_segments(text);
    if segments.is_empty() {
        return Ok(vec![text.to_owned()]);
    }

    // Check the size before building anything: the whole point of the cap is
    // to not allocate the runaway list in the first place.
    let mut total: usize = 1;
    for segment in &segments {
        total = total.saturating_mul(segment.values.len());
        if total > MAX_EXPANSION {
            bail!(
                "that pattern makes {}more than {MAX_EXPANSION} URLs",
                if total == usize::MAX { "far " } else { "" }
            );
        }
    }
    if total == 0 {
        bail!("that pattern makes no URLs at all");
    }

    // Odometer over the segments: the last one turns fastest, so the output
    // reads in the order someone would have typed it.
    let mut counters = vec![0_usize; segments.len()];
    let mut out = Vec::with_capacity(total);
    loop {
        let mut built = String::with_capacity(text.len() + 16);
        let mut cursor = 0;
        for (index, segment) in segments.iter().enumerate() {
            built.push_str(&text[cursor..segment.start]);
            built.push_str(&segment.values[counters[index]]);
            cursor = segment.end;
        }
        built.push_str(&text[cursor..]);
        out.push(built);

        let mut position = segments.len();
        loop {
            if position == 0 {
                return Ok(out);
            }
            position -= 1;
            counters[position] += 1;
            if counters[position] < segments[position].values.len() {
                break;
            }
            counters[position] = 0;
        }
    }
}

/// Locate every well-formed construct, left to right.
fn find_segments(text: &str) -> Vec<Segment> {
    let bytes = text.as_bytes();
    let mut segments = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        let (open, close) = match bytes[index] {
            b'[' => (b'[', b']'),
            b'{' => (b'{', b'}'),
            _ => {
                index += 1;
                continue;
            }
        };
        // No nesting: the first closer wins, which keeps a stray opener from
        // swallowing the rest of the URL.
        let Some(offset) = bytes[index + 1..].iter().position(|byte| *byte == close) else {
            index += 1;
            continue;
        };
        let end = index + 1 + offset;
        let body = &text[index + 1..end];

        let values = if open == b'[' {
            parse_range(body)
        } else {
            parse_alternatives(body)
        };

        match values {
            Some(values) if !values.is_empty() => {
                segments.push(Segment {
                    start: index,
                    end: end + 1,
                    values,
                });
                index = end + 1;
            }
            // Not a construct after all. Step past the opener only, so
            // `[not a range] but [1-3]` still finds the second one.
            _ => index += 1,
        }
    }
    segments
}

/// `001-250`, `1-9`, `0-100:5`, `a-f`.
fn parse_range(body: &str) -> Option<Vec<String>> {
    let (range, step) = match body.split_once(':') {
        Some((range, step)) => (range, step.trim().parse::<u64>().ok().filter(|s| *s > 0)?),
        None => (body, 1),
    };
    let (from, to) = range.split_once('-')?;
    let (from, to) = (from.trim(), to.trim());
    if from.is_empty() || to.is_empty() {
        return None;
    }

    // Letters: single characters only, so `a-f` is a range but `abc-def` is
    // not something anyone means.
    if from.len() == 1 && to.len() == 1 {
        let (start, end) = (from.as_bytes()[0], to.as_bytes()[0]);
        let both_letters = start.is_ascii_alphabetic() && end.is_ascii_alphabetic();
        // Mixed case would run through the punctuation between `Z` and `a`.
        if both_letters && start.is_ascii_uppercase() == end.is_ascii_uppercase() {
            return Some(letter_range(start, end, step));
        }
    }

    if !from.bytes().all(|b| b.is_ascii_digit()) || !to.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // The first bound's written width is the padding, which is what makes
    // `[001-250]` produce `001` and `[1-250]` produce `1`.
    let width = from.len();
    let start: u64 = from.parse().ok()?;
    let end: u64 = to.parse().ok()?;

    let mut values = Vec::new();
    if start <= end {
        let mut current = start;
        while current <= end && values.len() <= MAX_EXPANSION {
            values.push(format!("{current:0width$}"));
            current += step;
        }
    } else {
        // Counting down is a real thing people want, and refusing it would
        // just make them type the list out.
        let mut current = start;
        loop {
            if values.len() > MAX_EXPANSION {
                break;
            }
            values.push(format!("{current:0width$}"));
            let Some(next) = current.checked_sub(step) else {
                break;
            };
            if next < end {
                break;
            }
            current = next;
        }
    }
    Some(values)
}

fn letter_range(start: u8, end: u8, step: u64) -> Vec<String> {
    let step = step.max(1) as usize;
    let mut values = Vec::new();
    if start <= end {
        values.extend(
            (start..=end)
                .step_by(step)
                .map(|byte| (byte as char).to_string()),
        );
    } else {
        values.extend(
            (end..=start)
                .rev()
                .step_by(step)
                .map(|byte| (byte as char).to_string()),
        );
    }
    values
}

/// `jpg,png,gif`.
///
/// Requires a comma: `{}` appears in URLs often enough that treating every
/// brace pair as a construct would break them.
fn parse_alternatives(body: &str) -> Option<Vec<String>> {
    if !body.contains(',') {
        return None;
    }
    let values: Vec<String> = body
        .split(',')
        .map(|value| value.trim().to_owned())
        .collect();
    // `{a,,b}` is almost certainly a typo, but an empty alternative is a
    // legitimate way to say "with and without this part".
    Some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_url_expands_to_itself() {
        assert_eq!(
            expand("https://example.com/file.iso").expect("expands"),
            vec!["https://example.com/file.iso"]
        );
        assert!(!looks_like_pattern("https://example.com/file.iso"));
    }

    #[test]
    fn a_numeric_range_keeps_the_padding_of_its_first_bound() {
        let urls = expand("https://x.test/p[001-003].jpg").expect("expands");
        assert_eq!(
            urls,
            vec![
                "https://x.test/p001.jpg",
                "https://x.test/p002.jpg",
                "https://x.test/p003.jpg",
            ]
        );
        // Unpadded stays unpadded.
        let urls = expand("https://x.test/p[8-11].jpg").expect("expands");
        assert_eq!(
            urls.first().map(String::as_str),
            Some("https://x.test/p8.jpg")
        );
        assert_eq!(
            urls.last().map(String::as_str),
            Some("https://x.test/p11.jpg")
        );
    }

    #[test]
    fn a_step_is_honoured() {
        let urls = expand("https://x.test/[0-10:5].bin").expect("expands");
        assert_eq!(
            urls,
            vec![
                "https://x.test/0.bin",
                "https://x.test/5.bin",
                "https://x.test/10.bin",
            ]
        );
    }

    #[test]
    fn a_descending_range_counts_down() {
        let urls = expand("https://x.test/[3-1].bin").expect("expands");
        assert_eq!(
            urls,
            vec![
                "https://x.test/3.bin",
                "https://x.test/2.bin",
                "https://x.test/1.bin",
            ]
        );
    }

    #[test]
    fn a_letter_range_works_in_either_case() {
        assert_eq!(
            expand("https://x.test/[a-c].txt").expect("expands"),
            vec![
                "https://x.test/a.txt",
                "https://x.test/b.txt",
                "https://x.test/c.txt",
            ]
        );
        assert_eq!(
            expand("https://x.test/[A-C].txt").expect("expands"),
            vec![
                "https://x.test/A.txt",
                "https://x.test/B.txt",
                "https://x.test/C.txt",
            ]
        );
        // Mixed case would walk through the punctuation between Z and a.
        assert!(!looks_like_pattern("https://x.test/[A-c].txt"));
    }

    #[test]
    fn alternatives_expand() {
        assert_eq!(
            expand("https://x.test/img.{jpg,png}").expect("expands"),
            vec!["https://x.test/img.jpg", "https://x.test/img.png"]
        );
    }

    #[test]
    fn several_constructs_multiply_out() {
        // Last one turns fastest, so the order reads the way it was typed.
        let urls = expand("https://x.test/s[1-2]-{a,b}.png").expect("expands");
        assert_eq!(
            urls,
            vec![
                "https://x.test/s1-a.png",
                "https://x.test/s1-b.png",
                "https://x.test/s2-a.png",
                "https://x.test/s2-b.png",
            ]
        );
    }

    #[test]
    fn a_bracket_that_is_not_a_range_is_left_alone() {
        // URLs are allowed to contain brackets, and mangling one would be
        // worse than not expanding it.
        for text in [
            "https://x.test/a[b]c",
            "https://x.test/[]",
            "https://x.test/[not-a-range]",
            "https://x.test/[1-]",
            "https://x.test/[-9]",
            "https://x.test/[abc-def]",
            // An IPv6 literal, which must survive untouched.
            "https://[2001:db8::1]/file.iso",
        ] {
            assert!(!looks_like_pattern(text), "{text} should not look like one");
            assert_eq!(expand(text).expect("expands"), vec![text]);
        }
        // A brace pair with no comma is not an alternation.
        assert!(!looks_like_pattern("https://x.test/{single}"));
    }

    #[test]
    fn a_stray_opener_does_not_swallow_a_real_range() {
        let urls = expand("https://x.test/[oops/p[1-2].jpg").expect("expands");
        assert_eq!(
            urls,
            vec!["https://x.test/[oops/p1.jpg", "https://x.test/[oops/p2.jpg",]
        );
    }

    #[test]
    fn a_runaway_pattern_is_refused_before_it_is_built() {
        let error = expand("https://x.test/[1-100000000].bin").expect_err("must refuse");
        assert!(error.to_string().contains("more than"), "{error}");

        // And the product of several, which is the easier mistake to make.
        let error = expand("https://x.test/[1-100][1-100][1-100].bin").expect_err("must refuse");
        assert!(error.to_string().contains("more than"), "{error}");
    }

    #[test]
    fn the_cap_itself_is_reachable() {
        // Exactly at the limit is allowed; one past it is not.
        let urls = expand(&format!("https://x.test/[1-{MAX_EXPANSION}].bin")).expect("expands");
        assert_eq!(urls.len(), MAX_EXPANSION);
        assert!(expand(&format!("https://x.test/[1-{}].bin", MAX_EXPANSION + 1)).is_err());
    }

    #[test]
    fn an_empty_line_expands_to_nothing() {
        assert!(expand("   ").expect("expands").is_empty());
    }
}
