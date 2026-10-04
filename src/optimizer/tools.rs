//! The optimization tools an agent can be configured with, and the catalog the TUI
//! offers them from.
//!
//! # Why this replaced the AST round-trip
//!
//! `apply_rule` used to parse the text into nodes and *re-emit* every one of them.
//! That is a whole-text rewrite in order to edit two node kinds, and it cost more
//! than it bought: a fenced block with no language tag came back tagged ```` ```json ````,
//! because the parser had decided it was JSON and the printer tagged it. Text was
//! also a single opaque node, so no tool could touch it.
//!
//! A tool here is handed the text and returns the text. Each one locates what it
//! cares about with [`fenced_blocks`] and splices the *original* bytes, so anything
//! not explicitly rewritten survives exactly — no fence gains a language, no line
//! ends up somewhere else.
//!
//! # Why the catalog is code
//!
//! The names used to be free text, and an unrecognised one was a silent no-op:
//! `compress_code` misspelled as `compres_code` did nothing and said nothing. The
//! picker now lists [`TOOLS`], and [`unknown`] names the ones that are not in it, so
//! a typo is a log line instead of a mystery.

use serde_json::Value;
use std::ops::Range;

/// One selectable optimization, with the sentence the picker shows for it.
pub struct OptTool {
    pub id: &'static str,
    pub summary: &'static str,
}

/// Every optimization that exists. The picker renders this and nothing else, so
/// what is offered and what is implemented cannot drift apart.
pub const TOOLS: &[OptTool] = &[
    OptTool {
        id: "compress_code",
        summary: "drops blank lines and comments inside code blocks",
    },
    OptTool {
        id: "minify_json",
        summary: "re-serialises JSON without whitespace",
    },
    OptTool {
        id: "strip_ansi",
        summary: "removes terminal colour escapes from captured output",
    },
    OptTool {
        id: "collapse_blanks",
        summary: "collapses runs of blank lines in prose",
    },
    OptTool {
        id: "dedent_code",
        summary: "removes the shared indentation of a code block",
    },
];

pub fn is_known(name: &str) -> bool {
    TOOLS.iter().any(|tool| tool.id == name)
}

/// The configured names that name no tool.
///
/// Called when an agent is loaded, so a mistyped tool is a warning at startup
/// rather than a setting that appears to do nothing for ever.
pub fn unknown(names: &[String]) -> Vec<&str> {
    names
        .iter()
        .filter(|name| !is_known(name))
        .map(String::as_str)
        .collect()
}

/// Run one tool. An unrecognised name returns the input untouched — the warning is
/// [`unknown`]'s job, not this function's, and it must not fail a chat turn.
pub fn apply(name: &str, input: &str) -> String {
    match name {
        "compress_code" => compress_code(input),
        "minify_json" => minify_json(input),
        "strip_ansi" => strip_ansi(input),
        "collapse_blanks" => collapse_blanks(input),
        "dedent_code" => dedent_code(input),
        _ => input.to_string(),
    }
}

/// A fenced block, located rather than parsed.
struct Block<'a> {
    /// The bytes between the fences.
    body: Range<usize>,
    /// The tag after the opening fence. `None` when there was none, which is not
    /// the same as an empty one: a block with no tag is left untagged.
    lang: Option<&'a str>,
}

/// Every fenced block in `input`, in order, with an unclosed one kept to the end.
///
/// Byte offsets, because the tools splice. Line handling is `split_inclusive`, so
/// a text without a trailing newline still has its last block found.
fn fenced_blocks(input: &str) -> Vec<Block<'_>> {
    let mut blocks = Vec::new();
    let mut offset = 0usize;
    // The opening fence, while a block is open: (tag, body start).
    let mut open: Option<(Option<&str>, usize)> = None;

    for line in input.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if let Some(tag) = bare.strip_prefix("```") {
            let tag = tag.trim();
            match open.take() {
                None => {
                    open = Some((
                        if tag.is_empty() { None } else { Some(tag) },
                        offset + line.len(),
                    ));
                }
                Some((lang, body_start)) => {
                    blocks.push(Block {
                        body: body_start..offset,
                        lang,
                    });
                }
            }
        }
        offset += line.len();
    }

    // An unclosed block is still a block the user wrote; keeping it means a tool
    // treats it like any other rather than leaving it as a stray fence.
    if let Some((lang, body_start)) = open {
        blocks.push(Block {
            body: body_start..offset,
            lang,
        });
    }

    blocks
}

/// Splice `replacement` over `span`, copying everything outside it.
fn splice(input: &str, span: &Range<usize>, replacement: &str) -> String {
    let mut out = String::with_capacity(input.len());
    out.push_str(&input[..span.start]);
    out.push_str(replacement);
    out.push_str(&input[span.end..]);
    out
}

/// Languages where a leading `#` is a comment and not a preprocessor directive.
///
/// The distinction is the whole reason `compress_code` needs a table. `#include`
/// and `#define` start with `#` and dropping them does not shorten the code, it
/// breaks the build — so a language absent from this list has nothing stripped.
const HASH_COMMENT: &[&str] = &[
    "python",
    "py",
    "ruby",
    "rb",
    "sh",
    "bash",
    "zsh",
    "shell",
    "fish",
    "perl",
    "pl",
    "r",
    "toml",
    "yaml",
    "yml",
    "dockerfile",
    "make",
    "makefile",
    "conf",
    "ini",
];

/// Languages where a leading `//` is a comment.
const SLASH_COMMENT: &[&str] = &[
    "c",
    "cpp",
    "c++",
    "h",
    "hpp",
    "java",
    "javascript",
    "js",
    "typescript",
    "ts",
    "rust",
    "rs",
    "go",
    "csharp",
    "cs",
    "kotlin",
    "kt",
    "swift",
    "scala",
    "php",
    "dart",
    "vue",
];

/// Languages where a leading `--` is a comment.
const DASH_COMMENT: &[&str] = &["sql", "lua", "haskell", "hs"];

/// Languages where leading whitespace is meaning, not layout.
///
/// `dedent_code` skips these outright: in Python an indented block *is* the block,
/// and in YAML the nesting is the document.
const INDENT_IS_SEMANTIC: &[&str] = &["python", "py", "yaml", "yml", "toml", "coffee", "haml"];

fn lang_matches(lang: Option<&str>, list: &[&str]) -> bool {
    match lang {
        Some(tag) => {
            let tag = tag.to_ascii_lowercase();
            list.iter().any(|known| *known == tag)
        }
        None => false,
    }
}

/// The comment marker for a block, or `None` to strip nothing.
///
/// `None` for an untagged block on purpose. Half the value of `compress_code` was
/// that it stripped `#` from any block at all, and that is also how it deleted
/// `#!/bin/bash` and turned a shell script into something that would not run.
fn comment_marker(lang: Option<&str>) -> Option<&'static str> {
    if lang_matches(lang, HASH_COMMENT) {
        Some("#")
    } else if lang_matches(lang, SLASH_COMMENT) {
        Some("//")
    } else if lang_matches(lang, DASH_COMMENT) {
        Some("--")
    } else {
        None
    }
}

/// Drop blank lines and whole-line comments from every code block.
///
/// Only whole lines are ever removed, so no line can be split or joined, and the
/// fences and the block's language tag are copied through untouched.
pub fn compress_code(input: &str) -> String {
    let mut out = input.to_string();
    for block in fenced_blocks(input).into_iter().rev() {
        let Some(marker) = comment_marker(block.lang) else {
            continue;
        };
        let body = &input[block.body.clone()];
        let kept: Vec<&str> = body
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    return false;
                }
                // A shebang is the interpreter line, not a comment: without it the
                // script is not the program the author wrote.
                if trimmed.starts_with("#!") {
                    return true;
                }
                !trimmed.starts_with(marker)
            })
            .collect();
        let replacement = if kept.is_empty() {
            String::new()
        } else {
            format!("{}\n", kept.join("\n"))
        };
        out = splice(&out, &block.body, &replacement);
    }
    out
}

/// Re-serialise every JSON block without whitespace.
///
/// A block is treated as JSON when it is tagged `json` or carries no tag at all —
/// and a tag that was absent stays absent. The previous version printed every JSON
/// node as ```` ```json ````, so an untagged block came back claiming to be JSON
/// that its author had deliberately not labelled.
pub fn minify_json(input: &str) -> String {
    let mut out = input.to_string();
    for block in fenced_blocks(input).into_iter().rev() {
        if !matches!(block.lang, None | Some("json")) {
            continue;
        }
        let body = &input[block.body.clone()];
        let Ok(value) = serde_json::from_str::<Value>(body) else {
            continue;
        };
        let minified = match &value {
            Value::String(s) => serde_json::to_string(&Value::String(s.clone())),
            _ => serde_json::to_string(&value),
        };
        if let Ok(minified) = minified {
            out = splice(&out, &block.body, &format!("{minified}\n"));
        }
    }
    out
}

/// Remove terminal escape sequences, leaving every other byte alone.
///
/// Handles CSI (`ESC [ … final`), OSC (`ESC ] … BEL`/`ST`) and the two-byte
/// charset selects. Only ASCII is consumed while skipping, so multi-byte text
/// passes through byte for byte — which is the property that matters, since a tool
/// that mangled a `ñ` would be worse than the escapes it removed.
pub fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != 0x1b {
            // Not an escape: copy one whole character.
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i] & 0xc0) == 0x80 {
                i += 1;
            }
            out.push_str(&input[start..i]);
            continue;
        }

        match bytes.get(i + 1) {
            Some(b'[') => {
                // CSI: parameter bytes, then intermediate bytes, then one final byte.
                i += 2;
                while i < bytes.len() && (0x30..=0x3f).contains(&bytes[i]) {
                    i += 1;
                }
                while i < bytes.len() && (0x20..=0x2f).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1;
            }
            Some(b']') => {
                // OSC: runs until BEL or ST.
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            Some(b'(' | b')' | b'*' | b'+') => i += 3,
            _ => i += 2,
        }
    }

    out
}

/// Collapse runs of two or more blank lines into one, in prose only.
///
/// Skipped inside code blocks: a blank line between two functions is structure, and
/// between a `#!/bin/bash` and the next line it is the end of the marker.
pub fn collapse_blanks(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_code = false;
    let mut blanks_in_a_row = 0usize;

    for line in input.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if bare.starts_with("```") {
            in_code = !in_code;
            blanks_in_a_row = 0;
            out.push_str(line);
            continue;
        }
        if in_code {
            out.push_str(line);
            continue;
        }
        if bare.trim().is_empty() {
            blanks_in_a_row += 1;
            if blanks_in_a_row > 1 {
                continue;
            }
        } else {
            blanks_in_a_row = 0;
        }
        out.push_str(line);
    }
    out
}

/// Remove the indentation every line in a block shares.
///
/// Skipped for the languages in [`INDENT_IS_SEMANTIC`], and skipped for an untagged
/// block: an untagged block of Python is still Python, and the one way to be wrong
/// about that is to guess.
pub fn dedent_code(input: &str) -> String {
    let mut out = input.to_string();
    for block in fenced_blocks(input).into_iter().rev() {
        let Some(tag) = block.lang else {
            continue;
        };
        if lang_matches(Some(tag), INDENT_IS_SEMANTIC) {
            continue;
        }
        let body = &input[block.body.clone()];
        let common = body
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.len() - line.trim_start().len())
            .min()
            .unwrap_or(0);
        if common == 0 {
            continue;
        }
        let dedented: Vec<String> = body
            .lines()
            .map(|line| {
                if line.len() >= common {
                    line[common..].to_string()
                } else {
                    line.trim_start().to_string()
                }
            })
            .collect();
        let replacement = format!("{}\n", dedented.join("\n"));
        out = splice(&out, &block.body, &replacement);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog is the contract the picker offers against, so a tool that exists
    /// but is not listed could never be selected, and one that is listed but does
    /// not exist does nothing.
    #[test]
    fn every_listed_tool_does_something() {
        // One sample each, because a tool that finds nothing in a sample proves
        // nothing about itself.
        let cases = [
            ("compress_code", "```python\n# c\nx = 1\n```"),
            ("minify_json", "```json\n{ \"a\" : 1 }\n```"),
            ("strip_ansi", "\u{1b}[31mrojo\u{1b}[0m"),
            ("collapse_blanks", "uno\n\n\n\ndos"),
            ("dedent_code", "```rust\n    fn a() {}\n```"),
        ];
        for (id, sample) in cases {
            assert!(is_known(id), "{id} is applied but not listed");
            assert_ne!(
                apply(id, sample),
                sample,
                "{id} is listed but changed nothing on {sample:?}"
            );
        }
        assert_eq!(cases.len(), TOOLS.len(), "a tool has no sample here");
    }

    #[test]
    fn an_unknown_name_changes_nothing_and_is_reported() {
        let input = "unchanged";
        assert_eq!(apply("compres_code", input), input);

        let names = vec!["compress_code".to_string(), "compres_code".to_string()];
        assert_eq!(unknown(&names), vec!["compres_code"], "the typo is named");
        assert!(unknown(&["minify_json".to_string()]).is_empty());
    }

    /// The bug that started this: an untagged block came back tagged `json`.
    #[test]
    fn an_untagged_json_block_stays_untagged() {
        let input = "Mira:\n```\n{\"a\": 1, \"b\": [2, 3]}\n```\nfin";
        let out = minify_json(input);

        assert!(
            out.contains("```\n{\"a\":1,\"b\":[2,3]}\n```"),
            "minified but not re-tagged: {out:?}"
        );
        assert!(
            !out.contains("```json"),
            "the author left the tag off and it must stay off: {out:?}"
        );
    }

    #[test]
    fn a_json_tag_is_respected_too() {
        let input = "```json\n{ \"a\" : 1 }\n```";
        assert_eq!(minify_json(input), "```json\n{\"a\":1}\n```");
    }

    /// Not JSON, so not touched — the tag is the author's claim and it is not ours
    /// to overrule.
    #[test]
    fn a_block_that_is_not_json_is_left_alone() {
        let input = "```python\n{\"a\": 1}\n```";
        assert_eq!(minify_json(input), input);
    }

    /// The bug that would break a script: `#!/bin/bash` is not a comment.
    #[test]
    fn a_shebang_survives_compression() {
        let input = "```bash\n#!/bin/bash\n# drop me\necho hi\n\n```";
        let out = compress_code(input);

        assert!(
            out.contains("#!/bin/bash"),
            "the interpreter line stays: {out:?}"
        );
        assert!(!out.contains("# drop me"), "a real comment goes: {out:?}");
        assert!(out.contains("echo hi"));
    }

    /// `#` is a comment in Python and a preprocessor directive in C. Dropping
    /// `#include` does not shorten anything, it stops the file compiling.
    #[test]
    fn hash_is_a_comment_only_where_it_is_one() {
        let python = compress_code("```python\n# note\nx = 1\n```");
        assert!(
            !python.contains("# note"),
            "in python it is a comment: {python:?}"
        );

        let c = compress_code("```c\n#include <stdio.h>\n// note\nint x;\n```");
        assert!(c.contains("#include"), "in c it is a directive: {c:?}");
        assert!(!c.contains("// note"), "but // is still a comment: {c:?}");
    }

    #[test]
    fn each_language_family_gets_its_own_marker() {
        for (tag, marker) in [("sql", "--"), ("lua", "--"), ("go", "//"), ("sh", "#")] {
            let input = format!("```{tag}\n{marker} gone\nkept\n```");
            let out = compress_code(&input);
            assert!(!out.contains("gone"), "{tag} should drop {marker}: {out:?}");
            assert!(out.contains("kept"), "{tag} must keep real code: {out:?}");
        }
    }

    /// An untagged block is not evidence of any language, and the previous behaviour
    /// of stripping `#` from all of them is what deleted shebangs.
    #[test]
    fn an_untagged_block_is_not_stripped_at_all() {
        let input = "```\n# maybe a comment\nmaybe code\n```";
        assert_eq!(compress_code(input), input);
    }

    /// The user's own rules must survive verbatim, unless the text really is a code
    /// block and the language says it is.
    #[test]
    fn prose_around_a_block_is_untouched() {
        let input = "## Agent Rules\n- usa este ejemplo:\n```python\n# importante\nx = 1\n```\n- y nada mas\n";
        let out = compress_code(input);
        assert!(out.contains("## Agent Rules"));
        assert!(out.contains("- usa este ejemplo:"));
        assert!(out.contains("- y nada mas"));
        assert!(
            !out.contains("# importante"),
            "but the block was compressed"
        );
    }

    #[test]
    fn ansi_escapes_go_and_everything_else_stays() {
        let input = "\u{1b}[31merror\u{1b}[0m: no such file\n";
        assert_eq!(strip_ansi(input), "error: no such file\n");

        // 256-colour and true-colour forms, plus a cursor move.
        let fancy = "\u{1b}[38;2;255;0;0mrojo\u{1b}[38;5;196m\u{1b}[2J\u{1b}[1;1Hx";
        assert_eq!(strip_ansi(fancy), "rojox");
    }

    /// The property that matters for `strip_ansi`: nothing but escape sequences is
    /// removed, so any non-ASCII text comes through byte for byte.
    #[test]
    fn strip_ansi_keeps_multibyte_text_intact() {
        let input = "\u{1b}[32macentos: ñandú, 日本語, CORE\u{1b}[0m — em dash\n";
        assert_eq!(
            strip_ansi(input),
            "acentos: ñandú, 日本語, CORE — em dash\n",
            "a tool that mangled an accent would be worse than the escapes"
        );
    }

    /// An OSC sequence, which runs to BEL rather than to a final byte.
    #[test]
    fn strip_ansi_handles_osc_sequences() {
        let input = "\u{1b}]0;window title\u{7}visible text";
        assert_eq!(strip_ansi(input), "visible text");
    }

    #[test]
    fn runs_of_blank_lines_collapse_in_prose() {
        let input = "uno\n\n\n\n\ndos";
        assert_eq!(
            collapse_blanks(input),
            "uno\n\ndos",
            "four blank lines become one"
        );

        let already = "uno\n\ndos";
        assert_eq!(
            collapse_blanks(already),
            already,
            "one blank line is not a run"
        );

        let tight = "uno\ndos";
        assert_eq!(collapse_blanks(tight), tight);
    }

    /// Inside a code block a blank line is structure, not noise.
    #[test]
    fn blank_lines_inside_code_are_kept() {
        let input = "```rust\nfn a() {}\n\n\n\nfn b() {}\n```";
        assert_eq!(collapse_blanks(input), input);
    }

    #[test]
    fn dedent_removes_only_the_shared_indent() {
        let input = "```rust\n        fn a() {}\n        \n        if x {\n            y();\n        }\n```";
        assert_eq!(
            dedent_code(input),
            "```rust\nfn a() {}\n\nif x {\n    y();\n}\n```",
            "the shared eight spaces go, the nested four stay, and the blank line \
             — which was all whitespace — becomes empty rather than keeping eight \
             invisible columns"
        );
    }

    /// Indentation *is* the program in these, so guessing would break code rather
    /// than shorten it.
    #[test]
    fn dedent_refuses_languages_where_indent_is_meaning() {
        for tag in ["python", "yaml"] {
            let input = format!("```{tag}\n    if x:\n        y()\n```");
            assert_eq!(dedent_code(&input), input, "{tag} must not be dedented");
        }
        assert_eq!(
            dedent_code("```\n    maybe python\n```"),
            "```\n    maybe python\n```",
            "and an untagged block is not evidence of any language"
        );
    }

    /// Every line-based tool must leave text it has no business changing alone.
    ///
    /// This is the property that catches a tool eating a character rather than a whole
    /// line — the class of bug the first version of `compress_code` had, deleting
    /// `#!/bin/bash` and a comment inside a user's rule on its way through.
    ///
    /// `minify_json` is not here: it removes whitespace, which is its entire purpose,
    /// so "no new words" is false of it by design. It has its own property below.
    #[test]
    fn a_line_tool_only_ever_removes_whole_lines() {
        let samples = [
            "prosa normal, sin nada que ver",
            "```python\ndef f(x):\n    return x  # comentario\n```",
            "sin bloque final",
            "```json\n{\"k\": [1, 2, {\"n\": null}]}\n```",
            "acentos: ñandú, 日本語 — y `backticks` sueltos",
            "",
        ];
        fn words(s: &str) -> Vec<&str> {
            s.split_whitespace().collect()
        }
        for sample in samples {
            for tool in [
                "compress_code",
                "strip_ansi",
                "collapse_blanks",
                "dedent_code",
            ] {
                let out = apply(tool, sample);
                let (before, after) = (words(sample), words(&out));
                assert!(
                    after.iter().all(|w| before.contains(w)),
                    "{tool} invented a word on {sample:?}: {out:?}"
                );
                assert!(
                    after.len() <= before.len(),
                    "{tool} grew the text on {sample:?}: {out:?}"
                );
            }
        }
    }

    /// What `minify_json` owes instead: the value must survive. Whitespace may go,
    /// the data may not.
    #[test]
    fn minify_json_preserves_the_value() {
        let samples = [
            "{\"k\": [1, 2, {\"n\": null}]}",
            "{\"s\": \"con espacios dentro\", \"b\": true}",
            "{\"nested\": {\"deep\": {\"deeper\": []}}}",
        ];
        for sample in samples {
            let wrapped = format!("```json\n{sample}\n```");
            let out = minify_json(&wrapped);
            let body = out
                .trim_start_matches("```json\n")
                .trim_end_matches("\n```");
            let before: Value = serde_json::from_str(sample).unwrap();
            let after: Value = serde_json::from_str(body).unwrap();
            assert_eq!(before, after, "the data changed for {sample:?}: {out:?}");
        }
    }

    /// `collapse_blanks` and `strip_ansi` are the two that may leave gaps. For
    /// everything else, text with nothing to act on must come back byte-identical
    /// rather than merely equivalent.
    #[test]
    fn text_without_a_target_is_returned_unchanged() {
        let sample = "Una linea.\n\nOtra linea.\n";
        for tool in ["compress_code", "minify_json", "dedent_code"] {
            assert_eq!(
                apply(tool, sample),
                sample,
                "{} rewrote prose that contained nothing for it",
                tool
            );
        }
    }

    #[test]
    fn a_tiny_terminal_does_not_panic() {
        for input in ["", "\u{1b}", "```", "```\n", "```json\n{"] {
            for tool in TOOLS {
                let _ = apply(tool.id, input);
            }
        }
    }
}
