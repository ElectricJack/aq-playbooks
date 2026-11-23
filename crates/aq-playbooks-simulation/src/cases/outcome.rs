//! Saved playbooks, author questions and source quotations.
use super::{Case, MAX_CASES, MIN_CASES};
use serde::{Deserialize, Serialize};
use serde_json::Value;
/// The outcome file the authoring agent's `save`, `ask` and `unable` write,
/// relative to the workspace, and the most bytes it may have: 4 MiB. A
/// saved outcome holds the draft and every case, and a case spells out each
/// call with its whole input, so 40 routing cases and their draft come to
/// about 0.6 MiB (`a_forty_case_routing_save_fits_the_outcome_bound` in the
/// CLI's tests). A host reads at most this many bytes, plus one to notice a
/// longer file.
pub const OUTCOME_FILE: &str = "out/outcome.json";
pub const MAX_OUTCOME_BYTES: usize = 4_194_304;
pub const MAX_QUESTION_BYTES: usize = 4096;
pub const MAX_REASON_BYTES: usize = 4096;
/// One question names at most this many sentences.
pub const MAX_SENTENCES: usize = 16;
pub const MAX_SENTENCE_BYTES: usize = 8192;
/// The workspace files the host writes, which an `unable` may name instead
/// of a sentence when the fault is in one of them.
pub const HOST_FILES: [&str; 4] = [
    "record.json",
    "catalog.json",
    "facts.json",
    "extensions.json",
];

/// A sentence of `source.md`: its first line, its last line when it spans
/// more than one (absent for a single line), and those lines as they stand,
/// joined with newlines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sentence {
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    pub text: String,
}
/// A sentence spans at most this many lines.
pub const MAX_SENTENCE_LINES: u32 = 8;
/// Quotes lines `line` to `end_line` (or `line` alone) of `markdown`: every
/// line must exist and hold text, at most [`MAX_SENTENCE_LINES`] of them.
/// The tools quote with it, and a reader re-quotes an outcome's sentences
/// from its own copy of the document with it.
pub fn quote(markdown: &str, line: u32, end_line: Option<u32>) -> Result<Sentence, String> {
    let lines: Vec<&str> = markdown.lines().collect();
    let end = end_line.unwrap_or(line);
    let range = if end == line {
        format!("line {line}")
    } else {
        format!("lines {line}-{end}")
    };
    if line == 0 || end < line {
        return Err(format!(
            "{range}: name a sentence by its line N, or by its lines A-B with A no greater than B, counted from 1."
        ));
    }
    if end - line + 1 > MAX_SENTENCE_LINES {
        return Err(format!(
            "{range}: a sentence spans at most {MAX_SENTENCE_LINES} lines; name the lines of one sentence."
        ));
    }
    if end as usize > lines.len() {
        return Err(format!(
            "{range}: source.md has lines 1 to {}; name the lines that hold the sentence.",
            lines.len()
        ));
    }
    let quoted = &lines[line as usize - 1..end as usize];
    if let Some(blank) = quoted.iter().position(|l| l.trim().is_empty()) {
        return Err(format!(
            "{range}: line {} of source.md is blank; a sentence does not cross a blank line. Name the lines that hold it.",
            line as usize + blank
        ));
    }
    let text = quoted.join("\n");
    if text.len() > MAX_SENTENCE_BYTES {
        return Err(format!(
            "{range}: the sentence is longer than {MAX_SENTENCE_BYTES} bytes."
        ));
    }
    Ok(Sentence {
        line,
        end_line: (end != line).then_some(end),
        text,
    })
}
/// The lines a sentence names must be a range [`quote`] accepts.
fn lines_named(line: u32, end_line: Option<u32>) -> Result<u32, String> {
    match end_line {
        _ if line == 0 => Err("a sentence's line is zero".into()),
        None => Ok(line),
        Some(end) if end <= line => Err(format!(
            "end_line {end} is not after line {line}; leave it out for a one-line sentence"
        )),
        Some(end) if end - line + 1 > MAX_SENTENCE_LINES => Err(format!(
            "lines {line}-{end} span more than {MAX_SENTENCE_LINES} lines"
        )),
        Some(end) => Ok(end),
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    Saved,
    Question,
    Unable,
}
/// `out/outcome.json`: how an authoring run ended. Data for the host to
/// check again, never an instruction. **Every field is the agent's claim**
/// (the agent can write this file, and the files the tools read, itself).
/// A reader re-derives each field and never stores the agent's value as
/// fact: it re-seals `draft` from its own record, catalog, facts and
/// document and compares the digest with `artifact`; re-runs `cases` on the
/// artifact it sealed; recomputes `facts_digest` from the facts it holds;
/// and re-quotes each sentence's `text` (or `sentence`) from its own copy of
/// the document with [`quote`], keeping only the line numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    /// The draft and its cases; the digest of the artifact the agent's tools
    /// sealed and of the facts it was authored with.
    Saved {
        draft: Value,
        cases: Vec<Case>,
        artifact: String,
        facts_digest: String,
    },
    /// A question for the owner, about these sentences of the document.
    Question {
        question: String,
        sentences: Vec<Sentence>,
    },
    /// Why the document could not be turned into a playbook: either the
    /// sentence it is about (`line`, `end_line` as in [`Sentence`], and
    /// `sentence`), or, for a fault in one of the host's own files, that
    /// file ([`HOST_FILES`]) and no sentence.
    Unable {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_line: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sentence: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
    },
}
impl Outcome {
    pub fn kind(&self) -> OutcomeKind {
        match self {
            Self::Saved { .. } => OutcomeKind::Saved,
            Self::Question { .. } => OutcomeKind::Question,
            Self::Unable { .. } => OutcomeKind::Unable,
        }
    }
    /// Every bound the outcome must meet, except its total size.
    pub fn check(&self) -> Result<(), String> {
        match self {
            Self::Saved {
                cases,
                artifact,
                facts_digest,
                ..
            } => {
                if !(MIN_CASES..=MAX_CASES).contains(&cases.len()) {
                    return Err(format!(
                        "a save has {} cases; at least {MIN_CASES} and at most {MAX_CASES}",
                        cases.len()
                    ));
                }
                digest("artifact", artifact)?;
                digest("facts_digest", facts_digest)
            }
            Self::Question {
                question,
                sentences,
            } => {
                text("question", question, MAX_QUESTION_BYTES)?;
                check_sentences(sentences)
            }
            Self::Unable {
                reason,
                line,
                end_line,
                sentence,
                file,
            } => {
                text("reason", reason, MAX_REASON_BYTES)?;
                match (line, sentence, file) {
                    (Some(line), Some(sentence), None) => {
                        lines_named(*line, *end_line)?;
                        text("sentence", sentence, MAX_SENTENCE_BYTES)
                    }
                    (None, None, Some(file)) if end_line.is_none() => {
                        if HOST_FILES.contains(&file.as_str()) {
                            Ok(())
                        } else {
                            Err(format!(
                                "file {file:?} is not one of the host's files ({})",
                                HOST_FILES.join(", ")
                            ))
                        }
                    }
                    _ => Err(
                        "an unable names either a sentence (line and sentence) or a host file, not both and not neither".into(),
                    ),
                }
            }
        }
    }
    /// The file's bytes: checked, and at most [`MAX_OUTCOME_BYTES`].
    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        self.check().map_err(anyhow::Error::msg)?;
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        anyhow::ensure!(
            bytes.len() <= MAX_OUTCOME_BYTES,
            "the outcome is {} bytes; {OUTCOME_FILE} may be at most {MAX_OUTCOME_BYTES}. Shorten the draft or the cases.",
            bytes.len()
        );
        Ok(bytes)
    }
    /// Reads an outcome file: at most [`MAX_OUTCOME_BYTES`], strict, checked.
    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            bytes.len() <= MAX_OUTCOME_BYTES,
            "the outcome is {} bytes; at most {MAX_OUTCOME_BYTES}",
            bytes.len()
        );
        let outcome: Self = serde_json::from_slice(bytes)?;
        outcome.check().map_err(anyhow::Error::msg)?;
        Ok(outcome)
    }
}

fn text(field: &str, value: &str, max: usize) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{field} is empty"))
    } else if value.len() > max {
        Err(format!("{field} is {} bytes; at most {max}", value.len()))
    } else {
        Ok(())
    }
}
fn digest(field: &str, value: &str) -> Result<(), String> {
    if value.starts_with("sha256:") && value.len() == 71 {
        Ok(())
    } else {
        Err(format!("{field} is not a sha256: digest"))
    }
}
fn check_sentences(sentences: &[Sentence]) -> Result<(), String> {
    if sentences.is_empty() || sentences.len() > MAX_SENTENCES {
        return Err(format!(
            "a question names 1 to {MAX_SENTENCES} sentences; it names {}",
            sentences.len()
        ));
    }
    // In document order, and apart.
    let mut after = 0;
    for s in sentences {
        let end = lines_named(s.line, s.end_line)?;
        if s.line <= after {
            return Err(format!(
                "the sentence at line {} is not after the one before it (sentences are in document order and do not overlap)",
                s.line
            ));
        }
        after = end;
        text("sentence text", &s.text, MAX_SENTENCE_BYTES)?;
    }

    Ok(())
}
