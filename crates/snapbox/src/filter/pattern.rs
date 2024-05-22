use super::Filter;
use super::Redactions;
use crate::data::DataInner;
use crate::Data;

/// Adjust `actual` based on `expected`
///
/// As part of this, [`Redactions`] will be used, if any.
/// Additional built-in redactions:
/// - `...` on a line of its own: match multiple complete lines
/// - `[..]`: match multiple characters within a line
pub struct NormalizeToExpected<'a> {
    substitutions: &'a crate::Redactions,
    pattern: &'a Data,
}

impl<'a> NormalizeToExpected<'a> {
    pub fn new(substitutions: &'a crate::Redactions, pattern: &'a Data) -> Self {
        NormalizeToExpected {
            substitutions,
            pattern,
        }
    }
}

impl Filter for NormalizeToExpected<'_> {
    fn filter(&self, data: Data) -> Data {
        let source = data.source;
        let filters = data.filters;
        let inner = match data.inner {
            DataInner::Error(err) => DataInner::Error(err),
            DataInner::Binary(bin) => DataInner::Binary(bin),
            DataInner::Text(text) => {
                if let Some(pattern) = self.pattern.render() {
                    let lines = normalize_to_pattern(&text, &pattern, self.substitutions);
                    DataInner::Text(lines)
                } else {
                    DataInner::Text(text)
                }
            }
            #[cfg(feature = "json")]
            DataInner::Json(value) => {
                let mut value = value;
                if let DataInner::Json(exp) = &self.pattern.inner {
                    normalize_value_matches(&mut value, exp, self.substitutions);
                }
                DataInner::Json(value)
            }
            #[cfg(feature = "json")]
            DataInner::JsonLines(value) => {
                let mut value = value;
                if let DataInner::Json(exp) = &self.pattern.inner {
                    normalize_value_matches(&mut value, exp, self.substitutions);
                }
                DataInner::JsonLines(value)
            }
            #[cfg(feature = "term-svg")]
            DataInner::TermSvg(text) => {
                if let Some(pattern) = self.pattern.render() {
                    let lines = normalize_to_pattern(&text, &pattern, self.substitutions);
                    DataInner::TermSvg(lines)
                } else {
                    DataInner::TermSvg(text)
                }
            }
        };
        Data {
            inner,
            source,
            filters,
        }
    }
}

#[cfg(feature = "structured-data")]
fn normalize_value_matches(
    actual: &mut serde_json::Value,
    expected: &serde_json::Value,
    substitutions: &crate::Redactions,
) {
    use serde_json::Value::*;

    const KEY_WILDCARD: &str = "...";
    const VALUE_WILDCARD: &str = "{...}";

    match (actual, expected) {
        (act, String(exp)) if exp == VALUE_WILDCARD => {
            *act = serde_json::json!(VALUE_WILDCARD);
        }
        (String(act), String(exp)) => {
            *act = normalize_to_pattern(act, exp, substitutions);
        }
        (Array(act), Array(exp)) => {
            let mut sections = exp.split(|e| e == VALUE_WILDCARD).peekable();
            let mut processed = 0;
            while let Some(expected_subset) = sections.next() {
                // Process all values in the current section
                if !expected_subset.is_empty() {
                    let actual_subset = &mut act[processed..processed + expected_subset.len()];
                    for (a, e) in actual_subset.iter_mut().zip(expected_subset) {
                        normalize_value_matches(a, e, substitutions);
                    }
                    processed += expected_subset.len();
                }

                if let Some(next_section) = sections.peek() {
                    // If the next section has nothing in it, replace from processed to end with
                    // a single "{...}"
                    if next_section.is_empty() {
                        act.splice(processed.., vec![String(VALUE_WILDCARD.to_owned())]);
                        processed += 1;
                    } else {
                        let first = next_section.first().unwrap();
                        // Replace everything up until the value we are looking for with
                        // a single "{...}".
                        if let Some(index) = act.iter().position(|v| v == first) {
                            act.splice(processed..index, vec![String(VALUE_WILDCARD.to_owned())]);
                            processed += 1;
                        } else {
                            // If we cannot find the value we are looking for return early
                            break;
                        }
                    }
                }
            }
        }
        (Object(act), Object(exp)) => {
            let has_key_wildcard =
                exp.get(KEY_WILDCARD).and_then(|v| v.as_str()) == Some(VALUE_WILDCARD);
            for (actual_key, mut actual_value) in std::mem::replace(act, serde_json::Map::new()) {
                let actual_key = substitutions.redact(&actual_key);
                if let Some(expected_value) = exp.get(&actual_key) {
                    normalize_value_matches(&mut actual_value, expected_value, substitutions)
                } else if has_key_wildcard {
                    continue;
                }
                act.insert(actual_key, actual_value);
            }
            if has_key_wildcard {
                act.insert(KEY_WILDCARD.to_owned(), String(VALUE_WILDCARD.to_owned()));
            }
        }
        (_, _) => {}
    }
}

fn normalize_to_pattern(input: &str, pattern: &str, redactions: &Redactions) -> String {
    if input == pattern {
        return input.to_owned();
    }

    let input = redactions.redact(input);

    let mut normalized: Vec<&str> = Vec::new();
    let mut input_index = 0;
    let input_lines: Vec<_> = crate::utils::LinesWithTerminator::new(&input).collect();
    let mut pattern_lines = crate::utils::LinesWithTerminator::new(pattern).peekable();
    'outer: while let Some(pattern_line) = pattern_lines.next() {
        if is_line_elide(pattern_line) {
            if let Some(next_pattern_line) = pattern_lines.peek() {
                for (index_offset, next_input_line) in
                    input_lines[input_index..].iter().copied().enumerate()
                {
                    if line_matches(next_input_line, next_pattern_line, redactions) {
                        normalized.push(pattern_line);
                        input_index += index_offset;
                        continue 'outer;
                    }
                }
                // Give up doing further normalization
                break;
            } else {
                // Give up doing further normalization
                normalized.push(pattern_line);
                // captured rest so don't copy remaining lines over
                input_index = input_lines.len();
                break;
            }
        } else {
            let Some(input_line) = input_lines.get(input_index) else {
                // Give up doing further normalization
                break;
            };

            if line_matches(input_line, pattern_line, redactions) {
                input_index += 1;
                normalized.push(pattern_line);
            } else {
                // Give up doing further normalization
                break;
            }
        }
    }

    normalized.extend(input_lines[input_index..].iter().copied());
    normalized.join("")
}

fn is_line_elide(line: &str) -> bool {
    line == "...\n" || line == "..."
}

fn line_matches(mut input: &str, pattern: &str, redactions: &Redactions) -> bool {
    if input == pattern {
        return true;
    }

    let pattern = redactions.clear(pattern);
    let mut sections = pattern.split("[..]").peekable();
    while let Some(section) = sections.next() {
        if let Some(remainder) = input.strip_prefix(section) {
            if let Some(next_section) = sections.peek() {
                if next_section.is_empty() {
                    input = "";
                } else if let Some(restart_index) = remainder.find(next_section) {
                    input = &remainder[restart_index..];
                }
            } else {
                return remainder.is_empty();
            }
        } else {
            return false;
        }
    }

    false
}

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn empty() {
        let input = "";
        let pattern = "";
        let expected = "";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn literals_match() {
        let input = "Hello\nWorld";
        let pattern = "Hello\nWorld";
        let expected = "Hello\nWorld";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn pattern_shorter() {
        let input = "Hello\nWorld";
        let pattern = "Hello\n";
        let expected = "Hello\nWorld";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn input_shorter() {
        let input = "Hello\n";
        let pattern = "Hello\nWorld";
        let expected = "Hello\n";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn all_different() {
        let input = "Hello\nWorld";
        let pattern = "Goodbye\nMoon";
        let expected = "Hello\nWorld";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn middles_diverge() {
        let input = "Hello\nWorld\nGoodbye";
        let pattern = "Hello\nMoon\nGoodbye";
        let expected = "Hello\nWorld\nGoodbye";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn elide_delimited_with_sub() {
        let input = "Hello World\nHow are you?\nGoodbye World";
        let pattern = "Hello [..]\n...\nGoodbye [..]";
        let expected = "Hello [..]\n...\nGoodbye [..]";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn leading_elide() {
        let input = "Hello\nWorld\nGoodbye";
        let pattern = "...\nGoodbye";
        let expected = "...\nGoodbye";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn trailing_elide() {
        let input = "Hello\nWorld\nGoodbye";
        let pattern = "Hello\n...";
        let expected = "Hello\n...";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn middle_elide() {
        let input = "Hello\nWorld\nGoodbye";
        let pattern = "Hello\n...\nGoodbye";
        let expected = "Hello\n...\nGoodbye";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn post_elide_diverge() {
        let input = "Hello\nSun\nAnd\nWorld";
        let pattern = "Hello\n...\nMoon";
        let expected = "Hello\nSun\nAnd\nWorld";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn post_diverge_elide() {
        let input = "Hello\nWorld\nGoodbye\nSir";
        let pattern = "Hello\nMoon\nGoodbye\n...";
        let expected = "Hello\nWorld\nGoodbye\nSir";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn inline_elide() {
        let input = "Hello\nWorld\nGoodbye\nSir";
        let pattern = "Hello\nW[..]d\nGoodbye\nSir";
        let expected = "Hello\nW[..]d\nGoodbye\nSir";
        let actual = normalize_to_pattern(input, pattern, &Redactions::new());
        assert_eq!(expected, actual);
    }

    #[test]
    fn line_matches_cases() {
        let cases = [
            ("", "", true),
            ("", "[..]", true),
            ("hello", "hello", true),
            ("hello", "goodbye", false),
            ("hello", "[..]", true),
            ("hello", "he[..]", true),
            ("hello", "go[..]", false),
            ("hello", "[..]o", true),
            ("hello", "[..]e", false),
            ("hello", "he[..]o", true),
            ("hello", "he[..]e", false),
            ("hello", "go[..]o", false),
            ("hello", "go[..]e", false),
            (
                "hello world, goodbye moon",
                "hello [..], goodbye [..]",
                true,
            ),
            (
                "hello world, goodbye moon",
                "goodbye [..], goodbye [..]",
                false,
            ),
            (
                "hello world, goodbye moon",
                "goodbye [..], hello [..]",
                false,
            ),
            ("hello world, goodbye moon", "hello [..], [..] moon", true),
            (
                "hello world, goodbye moon",
                "goodbye [..], [..] moon",
                false,
            ),
            ("hello world, goodbye moon", "hello [..], [..] world", false),
        ];
        for (line, pattern, expected) in cases {
            let actual = line_matches(line, pattern, &Redactions::new());
            assert_eq!(expected, actual, "line={:?}  pattern={:?}", line, pattern);
        }
    }

    #[test]
    fn substitute_literal() {
        let input = "Hello world!";
        let pattern = "Hello [OBJECT]!";
        let mut sub = Redactions::new();
        sub.insert("[OBJECT]", "world").unwrap();
        let actual = normalize_to_pattern(input, pattern, &sub);
        assert_eq!(actual, pattern);
    }

    #[test]
    fn substitute_path() {
        let input = "input: /home/epage";
        let pattern = "input: [HOME]";
        let mut sub = Redactions::new();
        let sep = std::path::MAIN_SEPARATOR.to_string();
        let redacted = PathBuf::from(sep).join("home").join("epage");
        sub.insert("[HOME]", redacted).unwrap();
        let actual = normalize_to_pattern(input, pattern, &sub);
        assert_eq!(actual, pattern);
    }

    #[test]
    fn substitute_overlapping_path() {
        let input = "\
a: /home/epage
b: /home/epage/snapbox";
        let pattern = "\
a: [A]
b: [B]";
        let mut sub = Redactions::new();
        let sep = std::path::MAIN_SEPARATOR.to_string();
        let redacted = PathBuf::from(&sep).join("home").join("epage");
        sub.insert("[A]", redacted).unwrap();
        let redacted = PathBuf::from(sep)
            .join("home")
            .join("epage")
            .join("snapbox");
        sub.insert("[B]", redacted).unwrap();
        let actual = normalize_to_pattern(input, pattern, &sub);
        assert_eq!(actual, pattern);
    }

    #[test]
    fn substitute_disabled() {
        let input = "cargo";
        let pattern = "cargo[EXE]";
        let mut sub = Redactions::new();
        sub.insert("[EXE]", "").unwrap();
        let actual = normalize_to_pattern(input, pattern, &sub);
        assert_eq!(actual, pattern);
    }

    #[test]
    #[cfg(feature = "regex")]
    fn substitute_regex_unnamed() {
        let input = "Hello world!";
        let pattern = "Hello [OBJECT]!";
        let mut sub = Redactions::new();
        sub.insert("[OBJECT]", regex::Regex::new("world").unwrap())
            .unwrap();
        let actual = normalize_to_pattern(input, pattern, &sub);
        assert_eq!(actual, pattern);
    }

    #[test]
    #[cfg(feature = "regex")]
    fn substitute_regex_named() {
        let input = "Hello world!";
        let pattern = "Hello [OBJECT]!";
        let mut sub = Redactions::new();
        sub.insert(
            "[OBJECT]",
            regex::Regex::new("(?<redacted>world)!").unwrap(),
        )
        .unwrap();
        let actual = normalize_to_pattern(input, pattern, &sub);
        assert_eq!(actual, pattern);
    }
}
