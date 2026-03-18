/// Diff-aware line classification and markdown span parsing.

// ── Line classification ─────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Normal,
    UserInput,
    Error,
    Thinking,
    System,
    DiffAdd,
    DiffRemove,
    DiffHunk,
    DiffMeta,
}

pub fn classify_line(line: &str) -> LineKind {
    if line.starts_with("> ") {
        return LineKind::UserInput;
    }
    if line.starts_with("[!]") || line.starts_with("[APPROVE?]") {
        return LineKind::Error;
    }
    if line.starts_with("[think]") {
        return LineKind::Thinking;
    }
    if line.starts_with("[approved]")
        || line.starts_with("[rejected]")
        || line.starts_with("[killed]")
    {
        return LineKind::System;
    }
    if line.starts_with("+++") || line.starts_with("---") {
        return LineKind::DiffMeta;
    }
    if line.starts_with('+') && !line.starts_with("++") {
        return LineKind::DiffAdd;
    }
    if line.starts_with('-') && !line.starts_with("--") {
        return LineKind::DiffRemove;
    }
    if line.starts_with("@@") {
        return LineKind::DiffHunk;
    }
    if line.starts_with("diff ") {
        return LineKind::DiffMeta;
    }
    LineKind::Normal
}

// ── Markdown span parsing ───────────────────────────────────────

/// A styled span within a line of text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Span {
    /// Normal text
    Text(String),
    /// `inline code`
    Code(String),
    /// **bold text**
    Bold(String),
    /// *italic text*
    Italic(String),
    /// ***bold italic***
    BoldItalic(String),
}

/// Parse a line of text into styled spans.
/// Handles: `code`, **bold**, *italic*, ***bold italic***
pub fn parse_spans(line: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut buf = String::new();

    while i < len {
        // Inline code: `...`
        if chars[i] == '`' && !peek_is(&chars, i, "```") {
            if let Some(close) = chars[i + 1..].iter().position(|&c| c == '`') {
                if !buf.is_empty() {
                    spans.push(Span::Text(buf.clone()));
                    buf.clear();
                }
                i += 1;
                let code: String = chars[i..i + close].iter().collect();
                i += close + 1;
                if !code.is_empty() {
                    spans.push(Span::Code(code));
                }
            } else {
                buf.push('`');
                i += 1;
            }
            continue;
        }

        // Bold italic: ***...***
        if peek_is(&chars, i, "***") {
            let start = i + 3;
            let mut close = None;
            let mut j = start;
            while j < len {
                if peek_is(&chars, j, "***") {
                    close = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = close {
                if !buf.is_empty() {
                    spans.push(Span::Text(buf.clone()));
                    buf.clear();
                }
                let inner: String = chars[start..end].iter().collect();
                i = end + 3;
                if !inner.is_empty() {
                    spans.push(Span::BoldItalic(inner));
                }
            } else {
                buf.push_str("***");
                i += 3;
            }
            continue;
        }

        // Bold: **...**
        if peek_is(&chars, i, "**") && !peek_is(&chars, i, "***") {
            let start = i + 2;
            let mut close = None;
            let mut j = start;
            while j < len {
                if peek_is(&chars, j, "**") {
                    close = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = close {
                if !buf.is_empty() {
                    spans.push(Span::Text(buf.clone()));
                    buf.clear();
                }
                let inner: String = chars[start..end].iter().collect();
                i = end + 2;
                if !inner.is_empty() {
                    spans.push(Span::Bold(inner));
                }
            } else {
                buf.push_str("**");
                i += 2;
            }
            continue;
        }

        // Italic: *...*  (single * not followed by another *)
        if chars[i] == '*' && !peek_is(&chars, i, "**") {
            let start = i + 1;
            let mut close = None;
            let mut j = start;
            while j < len {
                if chars[j] == '*' && !peek_is(&chars, j, "**") {
                    close = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = close {
                if !buf.is_empty() {
                    spans.push(Span::Text(buf.clone()));
                    buf.clear();
                }
                let inner: String = chars[start..end].iter().collect();
                i = end + 1;
                if !inner.is_empty() {
                    spans.push(Span::Italic(inner));
                }
            } else {
                buf.push('*');
                i += 1;
            }
            continue;
        }

        buf.push(chars[i]);
        i += 1;
    }

    if !buf.is_empty() {
        spans.push(Span::Text(buf));
    }
    if spans.is_empty() {
        spans.push(Span::Text(String::new()));
    }
    spans
}

fn peek_is(chars: &[char], i: usize, s: &str) -> bool {
    let sc: Vec<char> = s.chars().collect();
    if i + sc.len() > chars.len() {
        return false;
    }
    for (j, c) in sc.iter().enumerate() {
        if chars[i + j] != *c {
            return false;
        }
    }
    true
}

/// Detect if a line is a code fence (``` with optional language).
/// Returns Some(language) for opening fences, Some("") for closing fences.
/// Returns None if not a fence.
pub fn parse_code_fence(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with("```") {
        let lang = trimmed[3..].trim().to_string();
        Some(lang)
    } else {
        None
    }
}

/// Detect if a line is a bullet point.
/// Returns Some(indent_level, content) if it is.
pub fn parse_bullet(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    if let Some(rest) = trimmed.strip_prefix("- ") {
        Some((indent / 2, rest))
    } else if let Some(rest) = trimmed.strip_prefix("* ") {
        Some((indent / 2, rest))
    } else if trimmed.len() >= 3 {
        // Numbered list: "1. ", "2. ", etc.
        let dot_pos = trimmed.find(". ");
        if let Some(dp) = dot_pos {
            if dp <= 3 && trimmed[..dp].chars().all(|c| c.is_ascii_digit()) {
                Some((indent / 2, &trimmed[dp + 2..]))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    }
}

/// Detect if a line is a heading (# ... ######)
/// Returns Some(level, content) where level is 1-6.
pub fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.chars().take_while(|&c| c == '#').count();
    if level > 6 {
        return None;
    }
    let rest = trimmed[level..].trim_start();
    if rest.is_empty() && level == trimmed.len() {
        return None;
    }
    Some((level, rest))
}

/// Extract an optional `SESSION:<id>` prefix from a prompt.
pub fn parse_session_prompt(first_msg: &str) -> (Option<String>, String) {
    if let Some(rest) = first_msg.strip_prefix("SESSION:") {
        if let Some(nl) = rest.find('\n') {
            return (Some(rest[..nl].to_string()), rest[nl + 1..].to_string());
        }
    }
    (None, first_msg.to_string())
}

/// Normalize CLI args for persistent stream-json runtimes.
pub fn build_persistent_runtime_args(
    base_args: &[String],
    model_flag: &str,
    model_override: Option<&str>,
    session_id: Option<&str>,
) -> Vec<String> {
    let mut args = Vec::new();
    let mut i = 0;

    while i < base_args.len() {
        match base_args[i].as_str() {
            "--input-format" | "--output-format" => {
                i += 2;
            }
            "--verbose" => {
                i += 1;
            }
            _ => {
                args.push(base_args[i].clone());
                i += 1;
            }
        }
    }

    args.extend([
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
    ]);

    if let Some(model) = model_override {
        if !model.is_empty() && !model_flag.is_empty() {
            args.push(model_flag.to_string());
            args.push(model.to_string());
        }
    }

    if let Some(sid) = session_id {
        args.push("--resume".into());
        args.push(sid.to_string());
    }

    args
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffSummary {
    pub files: Vec<String>,
    pub additions: usize,
    pub removals: usize,
}

pub fn extract_latest_turn_output(lines: &[String]) -> String {
    let start = lines
        .iter()
        .rposition(|line| line.starts_with("> "))
        .map(|idx| idx + 1)
        .unwrap_or(0);

    lines[start..]
        .iter()
        .filter(|line| !line.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

pub fn summarize_diff(lines: &[String]) -> DiffSummary {
    let mut additions = 0usize;
    let mut removals = 0usize;
    let mut files = Vec::new();

    for line in lines {
        match classify_line(line) {
            LineKind::DiffAdd => additions += 1,
            LineKind::DiffRemove => removals += 1,
            _ => {}
        }

        if let Some(path) = line
            .strip_prefix("+++ b/")
            .or_else(|| line.strip_prefix("--- a/"))
        {
            if !path.is_empty() && !files.iter().any(|existing| existing == path) {
                files.push(path.to_string());
            }
        }
    }

    DiffSummary {
        files,
        additions,
        removals,
    }
}

pub fn shell_escape(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    let escaped = arg.replace('\'', "'\"'\"'");
    format!("'{}'", escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_spans_plain() {
        assert_eq!(
            parse_spans("hello world"),
            vec![Span::Text("hello world".into())]
        );
    }

    #[test]
    fn test_parse_spans_inline_code() {
        assert_eq!(
            parse_spans("use `foo` here"),
            vec![
                Span::Text("use ".into()),
                Span::Code("foo".into()),
                Span::Text(" here".into())
            ]
        );
    }

    #[test]
    fn test_parse_spans_bold() {
        assert_eq!(
            parse_spans("this is **bold** text"),
            vec![
                Span::Text("this is ".into()),
                Span::Bold("bold".into()),
                Span::Text(" text".into())
            ]
        );
    }

    #[test]
    fn test_parse_spans_italic() {
        assert_eq!(
            parse_spans("this is *italic* text"),
            vec![
                Span::Text("this is ".into()),
                Span::Italic("italic".into()),
                Span::Text(" text".into())
            ]
        );
    }

    #[test]
    fn test_parse_spans_bold_italic() {
        assert_eq!(
            parse_spans("this is ***both*** text"),
            vec![
                Span::Text("this is ".into()),
                Span::BoldItalic("both".into()),
                Span::Text(" text".into())
            ]
        );
    }

    #[test]
    fn test_parse_spans_mixed() {
        let spans = parse_spans("run `cargo build` and **check** the *output*");
        assert_eq!(
            spans,
            vec![
                Span::Text("run ".into()),
                Span::Code("cargo build".into()),
                Span::Text(" and ".into()),
                Span::Bold("check".into()),
                Span::Text(" the ".into()),
                Span::Italic("output".into()),
            ]
        );
    }

    #[test]
    fn test_parse_code_fence() {
        assert_eq!(parse_code_fence("```python"), Some("python".into()));
        assert_eq!(parse_code_fence("```"), Some("".into()));
        assert_eq!(parse_code_fence("  ```rust  "), Some("rust".into()));
        assert_eq!(parse_code_fence("hello"), None);
    }

    #[test]
    fn test_parse_bullet() {
        assert_eq!(parse_bullet("- item one"), Some((0, "item one")));
        assert_eq!(parse_bullet("  - nested"), Some((1, "nested")));
        assert_eq!(parse_bullet("* star bullet"), Some((0, "star bullet")));
        assert_eq!(parse_bullet("1. numbered"), Some((0, "numbered")));
        assert_eq!(parse_bullet("no bullet"), None);
    }

    #[test]
    fn test_parse_heading() {
        assert_eq!(parse_heading("# Title"), Some((1, "Title")));
        assert_eq!(parse_heading("## Subtitle"), Some((2, "Subtitle")));
        assert_eq!(parse_heading("### Section"), Some((3, "Section")));
        assert_eq!(parse_heading("not a heading"), None);
    }

    #[test]
    fn test_empty_line() {
        assert_eq!(parse_spans(""), vec![Span::Text("".into())]);
    }

    #[test]
    fn test_parse_session_prompt_extracts_session_id() {
        let (session_id, prompt) = parse_session_prompt("SESSION:abc123\nship it");
        assert_eq!(session_id, Some("abc123".into()));
        assert_eq!(prompt, "ship it");
    }

    #[test]
    fn test_parse_session_prompt_plain_prompt() {
        let (session_id, prompt) = parse_session_prompt("plain prompt");
        assert_eq!(session_id, None);
        assert_eq!(prompt, "plain prompt");
    }

    #[test]
    fn test_build_persistent_runtime_args_normalizes_stream_flags() {
        let base_args = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "text".to_string(),
            "--input-format".to_string(),
            "text".to_string(),
            "--verbose".to_string(),
        ];

        let args = build_persistent_runtime_args(&base_args, "--model", None, None);

        assert_eq!(
            args,
            vec![
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
            ]
        );
    }

    #[test]
    fn test_build_persistent_runtime_args_append_model_and_resume() {
        let base_args = vec!["-p".to_string()];

        let args = build_persistent_runtime_args(
            &base_args,
            "--model",
            Some("sonnet-4.6"),
            Some("sess-42"),
        );

        assert_eq!(
            args,
            vec![
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "sonnet-4.6",
                "--resume",
                "sess-42",
            ]
        );
    }

    #[test]
    fn test_build_persistent_runtime_args_skip_empty_model() {
        let base_args = vec!["-p".to_string()];

        let args = build_persistent_runtime_args(&base_args, "--model", Some(""), None);

        assert!(!args.iter().any(|arg| arg == "--model"));
    }

    #[test]
    fn test_extract_latest_turn_output_ignores_prior_turns() {
        let lines = vec![
            "> first".into(),
            "old reply".into(),
            "> second".into(),
            String::new(),
            "new reply".into(),
            "more detail".into(),
        ];

        assert_eq!(extract_latest_turn_output(&lines), "new reply\nmore detail");
    }

    #[test]
    fn test_summarize_diff_counts_files_and_lines() {
        let lines = vec![
            "--- a/kernel.cu".into(),
            "+++ b/kernel.cu".into(),
            "@@ -1,2 +1,3 @@".into(),
            "-old".into(),
            "+new".into(),
            "+another".into(),
        ];

        assert_eq!(
            summarize_diff(&lines),
            DiffSummary {
                files: vec!["kernel.cu".into()],
                additions: 2,
                removals: 1,
            }
        );
    }

    #[test]
    fn test_shell_escape_handles_quotes() {
        assert_eq!(shell_escape("hello"), "'hello'");
        assert_eq!(shell_escape("it'a"), "'it'\"'\"'a'");
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn test_unclosed_backtick() {
        assert_eq!(
            parse_spans("hello `world"),
            vec![Span::Text("hello `world".into())]
        );
    }

    #[test]
    fn test_unclosed_backtick_at_end() {
        assert_eq!(parse_spans("hello `"), vec![Span::Text("hello `".into())]);
    }

    #[test]
    fn test_unclosed_bold() {
        assert_eq!(
            parse_spans("hello **world"),
            vec![Span::Text("hello **world".into())]
        );
    }

    #[test]
    fn test_unclosed_italic() {
        assert_eq!(
            parse_spans("hello *world"),
            vec![Span::Text("hello *world".into())]
        );
    }

    #[test]
    fn test_unclosed_bold_italic() {
        assert_eq!(
            parse_spans("hello ***world"),
            vec![Span::Text("hello ***world".into())]
        );
    }

    #[test]
    fn test_unclosed_backtick_with_closed_after() {
        assert_eq!(
            parse_spans("`ok` then `broken"),
            vec![Span::Code("ok".into()), Span::Text(" then `broken".into())]
        );
    }

    #[test]
    fn test_unclosed_bold_with_closed_after() {
        assert_eq!(
            parse_spans("**ok** then **broken"),
            vec![Span::Bold("ok".into()), Span::Text(" then **broken".into())]
        );
    }
}
