use super::Step;
use regex::Regex;

/// Applies the declarative pipeline (specs.md §5.3) in order. `exit_code` is
/// passed explicitly because `MatchOutput` and `OnEmpty` are "confirmed
/// success" shortcuts — business rule 2/3 (specs.md §4, epistemic failure
/// finding, section 11): neither can fire if the process didn't exit with
/// 0, or a real error would turn into a generic success message.
pub fn apply(steps: &[Step], raw: &str, exit_code: i32) -> String {
    apply_inner(steps, raw, exit_code, true)
}

/// Same pipeline, applied to stderr: `on_empty` is skipped — an empty stderr
/// after filtering just means "no warnings left", and a "completed with no
/// output" message there would sit right next to the real stdout summary
/// (found live with `npm install`, 2026-09-24).
pub fn apply_stderr(steps: &[Step], raw: &str, exit_code: i32) -> String {
    apply_inner(steps, raw, exit_code, false)
}

fn apply_inner(steps: &[Step], raw: &str, exit_code: i32, allow_on_empty: bool) -> String {
    let mut working = raw.to_string();

    for step in steps {
        match step {
            Step::StripAnsi => working = strip_ansi(&working),
            Step::Replace {
                pattern,
                replacement,
            } => {
                let Ok(re) = Regex::new(pattern) else {
                    continue;
                }; // fail-open (rule 3)
                working = re.replace_all(&working, replacement.as_str()).into_owned();
            }
            Step::MatchOutput { pattern, message } => {
                if exit_code != 0 {
                    continue;
                }
                let Ok(re) = Regex::new(pattern) else {
                    continue;
                };
                if re.is_match(&working) {
                    return message.clone(); // short-circuit: specs.md §5.4a
                }
            }
            Step::KeepLinesMatching { patterns } => {
                let res: Vec<Regex> = patterns.iter().filter_map(|p| Regex::new(p).ok()).collect();
                working = working
                    .lines()
                    .filter(|l| res.iter().any(|re| re.is_match(l)))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            Step::StripLinesMatching { patterns } => {
                let res: Vec<Regex> = patterns.iter().filter_map(|p| Regex::new(p).ok()).collect();
                working = working
                    .lines()
                    .filter(|l| !res.iter().any(|re| re.is_match(l)))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            Step::Dedup => {
                let mut out: Vec<&str> = Vec::new();
                for line in working.lines() {
                    if out.last() != Some(&line) {
                        out.push(line);
                    }
                }
                working = out.join("\n");
            }
            Step::TruncateLines { max_chars } => {
                working = working
                    .lines()
                    .map(|l| {
                        if l.chars().count() > *max_chars {
                            let cut: String = l.chars().take(*max_chars).collect();
                            format!("{cut}…")
                        } else {
                            l.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            Step::MaxLines { limit, tail } => {
                let lines: Vec<&str> = working.lines().collect();
                let total = lines.len();
                if total > *limit {
                    let tail = (*tail).min(*limit);
                    let marker = format!("[+{} lines omitted]", total - limit);
                    let mut kept: Vec<&str> = lines[..limit - tail].to_vec();
                    kept.push(&marker);
                    kept.extend_from_slice(&lines[total - tail..]);
                    working = kept.join("\n");
                }
            }
            Step::CompactPath => working = compact_path(&working),
            Step::CollapseLinesMatching { patterns, label } => {
                let res: Vec<Regex> = patterns.iter().filter_map(|p| Regex::new(p).ok()).collect();
                let mut out: Vec<&str> = Vec::new();
                let mut marker_at = None;
                let mut count = 0;
                for line in working.lines() {
                    if res.iter().any(|re| re.is_match(line)) {
                        count += 1;
                        if marker_at.is_none() {
                            marker_at = Some(out.len());
                            out.push(""); // placeholder, filled below
                        }
                    } else {
                        out.push(line);
                    }
                }
                if let Some(at) = marker_at {
                    let marker = format!("[+{count} lines omitted: {label}]");
                    let mut lines: Vec<String> = out.iter().map(|l| l.to_string()).collect();
                    lines[at] = marker;
                    working = lines.join("\n");
                }
            }
            Step::SqueezeSpaces => working = squeeze_spaces(&working),
            Step::OnEmpty { message } => {
                if allow_on_empty && exit_code == 0 && working.trim().is_empty() {
                    working = message.clone();
                }
            }
        }
    }

    working
}

fn squeeze_spaces(s: &str) -> String {
    let re = Regex::new(r"(\S) {2,}").expect("static regex is valid");
    s.lines()
        .map(|l| re.replace_all(l, "$1 ").into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn compact_path(s: &str) -> String {
    let mut out = s.to_string();
    if let Ok(cwd) = std::env::current_dir() {
        let cwd = cwd.to_string_lossy();
        if cwd.len() > 1 {
            out = replace_path(&out, cwd.trim_end_matches('/'), ".", "");
        }
    }
    if let Ok(home) = std::env::var("HOME")
        && home.len() > 1
    {
        out = replace_path(&out, home.trim_end_matches('/'), "~", "~/");
    }
    out
}

/// Replaces `prefix` only where it is a whole path — never glued to a longer
/// name on either side, so `/repo/app` doesn't turn `/repo/app-web` into
/// `.-web` (a path that doesn't exist). `prefix/…` becomes `dir…`; the
/// bare `prefix` becomes `whole`.
fn replace_path(s: &str, prefix: &str, whole: &str, dir: &str) -> String {
    let is_name = |c: char| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | '~');
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (i, _) in s.match_indices(prefix) {
        if i < last {
            continue;
        }
        let end = i + prefix.len();
        if s[..i].chars().next_back().is_some_and(is_name) {
            continue;
        }
        match s[end..].chars().next() {
            Some('/') => {
                out.push_str(&s[last..i]);
                out.push_str(dir);
                last = end + 1;
            }
            Some(c) if is_name(c) => continue,
            _ => {
                out.push_str(&s[last..i]);
                out.push_str(whole);
                last = end;
            }
        }
    }
    out.push_str(&s[last..]);
    out
}

fn strip_ansi(s: &str) -> String {
    // Regex compiled once per call (v1) — commands only pass through here
    // once per invocation, negligible cost next to the child process.
    // CSI with any parameter/intermediate bytes (`ESC[?25l`, `ESC[38;5;1m`),
    // OSC up to BEL or ST (terminal hyperlinks, titles), and two-byte escapes.
    let re = Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[@-Z\\-_]")
        .expect("static ANSI regex is valid");
    re.replace_all(s, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bornes::comandos::camada_b::Step;

    #[test]
    fn strip_ansi_removes_color_codes() {
        let input = "\x1b[32mok\x1b[0m failed: \x1b[31mno\x1b[0m";
        let out = apply(&[Step::StripAnsi], input, 0);
        assert_eq!(out, "ok failed: no");
    }

    #[test]
    fn strip_ansi_removes_cursor_codes_and_hyperlinks() {
        let input = "\x1b[?25lbuild\x1b[?25h \x1b[38;5;196mERR\x1b[0m \x1b]8;;https://x.io\x07docs\x1b]8;;\x07 \x1b]0;title\x1b\\done";
        assert_eq!(apply(&[Step::StripAnsi], input, 0), "build ERR docs done");
        // Plain text with brackets is left alone.
        assert_eq!(apply(&[Step::StripAnsi], "a[0m] [?25l]", 0), "a[0m] [?25l]");
    }

    #[test]
    fn match_output_short_circuits_only_on_success() {
        let steps = vec![Step::MatchOutput {
            pattern: "up to date".into(),
            message: "docker: nothing to pull".into(),
        }];
        assert_eq!(
            apply(&steps, "image is up to date\n", 0),
            "docker: nothing to pull"
        );
        // exit_code != 0 -> can't fabricate success (rule 2/3)
        assert_eq!(
            apply(&steps, "image is up to date\n", 1),
            "image is up to date\n"
        );
    }

    #[test]
    fn strip_lines_matching_removes_noise() {
        let steps = vec![Step::StripLinesMatching {
            patterns: vec!["^Pulling".into(), "^Digest:".into()],
        }];
        let input = "Pulling fs layer\nDigest: sha256:abc\nStatus: Downloaded\n";
        assert_eq!(apply(&steps, input, 0), "Status: Downloaded");
    }

    #[test]
    fn max_lines_caps_and_marks_omission() {
        let steps = vec![Step::MaxLines { limit: 2, tail: 0 }];
        let input = "a\nb\nc\nd\n";
        assert_eq!(apply(&steps, input, 0), "a\nb\n[+2 lines omitted]");
    }

    #[test]
    fn max_lines_with_tail_keeps_the_verdict() {
        let steps = vec![Step::MaxLines { limit: 3, tail: 1 }];
        let input = "e1\ne2\ne3\ne4\n✖ 4 problems (4 errors)\n";
        assert_eq!(
            apply(&steps, input, 0),
            "e1\ne2\n[+2 lines omitted]\n✖ 4 problems (4 errors)"
        );
        // Short output is untouched; a tail larger than the limit is clamped.
        assert_eq!(apply(&steps, "a\nb", 0), "a\nb");
        let all_tail = vec![Step::MaxLines { limit: 1, tail: 5 }];
        assert_eq!(apply(&all_tail, "a\nb\nc", 0), "[+2 lines omitted]\nc");
    }

    #[test]
    fn on_empty_only_fires_on_success() {
        let steps = vec![
            Step::StripLinesMatching {
                patterns: vec![".*".into()],
            },
            Step::OnEmpty {
                message: "nothing changed".into(),
            },
        ];
        assert_eq!(apply(&steps, "x\ny\n", 0), "nothing changed");
        assert_eq!(apply(&steps, "x\ny\n", 1), "");
    }

    #[test]
    fn dedup_collapses_consecutive_duplicates() {
        let steps = vec![Step::Dedup];
        assert_eq!(apply(&steps, "a\na\nb\nb\nb\na\n", 0), "a\nb\na");
    }

    #[test]
    fn on_empty_never_fires_on_stderr() {
        let steps = vec![
            Step::StripLinesMatching {
                patterns: vec!["^npm warn".into()],
            },
            Step::OnEmpty {
                message: "npm install: completed with no output".into(),
            },
        ];
        assert_eq!(apply_stderr(&steps, "npm warn deprecated x\n", 0), "");
    }

    #[test]
    fn collapse_replaces_matches_with_one_marker() {
        let steps = vec![Step::CollapseLinesMatching {
            patterns: vec!["^[┌├└] ".into()],
            label: "route table".into(),
        }];
        let input = "Route (app)\n┌ ○ /\n├ ○ /about\n└ ○ /terms\nDone";
        assert_eq!(
            apply(&steps, input, 0),
            "Route (app)\n[+3 lines omitted: route table]\nDone"
        );
        assert_eq!(apply(&steps, "nothing here", 0), "nothing here");
    }

    #[test]
    fn squeeze_spaces_keeps_indentation() {
        let steps = vec![Step::SqueezeSpaces];
        assert_eq!(
            apply(&steps, "  1:23  error    Unexpected any     rule", 0),
            "  1:23 error Unexpected any rule"
        );
    }

    #[test]
    fn compact_path_makes_cwd_relative() {
        let cwd = std::env::current_dir().unwrap();
        let input = format!(
            "{}/app/util1.ts\n> x build {}",
            cwd.display(),
            cwd.display()
        );
        assert_eq!(
            apply(&[Step::CompactPath], &input, 0),
            "app/util1.ts\n> x build ."
        );
    }

    #[test]
    fn paths_are_replaced_only_when_whole() {
        let r = |s: &str| replace_path(s, "/repo/app", ".", "");
        assert_eq!(r("/repo/app/src/a.ts:1"), "src/a.ts:1");
        assert_eq!(r("in /repo/app: ok"), "in .: ok");
        assert_eq!(r("'/repo/app'"), "'.'");
        // A sibling with the same prefix, or the prefix inside a longer path,
        // is a different path: left untouched.
        assert_eq!(r("/repo/app-web/x.ts"), "/repo/app-web/x.ts");
        assert_eq!(r("/repo/apps/x.ts"), "/repo/apps/x.ts");
        assert_eq!(r("/opt/repo/app/x.ts"), "/opt/repo/app/x.ts");
        let h = |s: &str| replace_path(s, "/Users/m", "~", "~/");
        assert_eq!(h("/Users/m/.npm/_logs"), "~/.npm/_logs");
        assert_eq!(h("/Users/mike/x"), "/Users/mike/x");
    }
}
