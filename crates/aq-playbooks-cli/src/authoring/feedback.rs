use super::*;

pub(super) fn ask(
    root: &Path,
    line: Vec<u32>,
    lines: Vec<String>,
    question: String,
) -> Result<(bool, Value)> {
    let named = line.len() + lines.len();
    anyhow::ensure!(
        (1..=MAX_SENTENCES).contains(&named),
        "--line and --lines name {named} sentences; a question names 1 to {MAX_SENTENCES} sentences of source.md. Name only the sentences the question is about."
    );
    bounded("the question", &question, MAX_QUESTION_BYTES)?;
    let mut ranges: Vec<(u32, Option<u32>)> = line.iter().map(|n| (*n, None)).collect();
    for range in &lines {
        ranges.push(parse_range(range)?);
    }
    ranges.sort();
    for pair in ranges.windows(2) {
        let ((a, a_end), (b, b_end)) = (pair[0], pair[1]);
        anyhow::ensure!(
            b > a_end.unwrap_or(a),
            "{} and {} overlap; name each sentence once, by lines of its own.",
            shown(a, a_end),
            shown(b, b_end)
        );
    }
    let source = read_inside(root, SOURCE_FILE, MAX_SOURCE_BYTES)?;
    let source = SentenceSource::new(&source);
    let sentences = ranges
        .into_iter()
        .map(|(line, end)| source.quote(line, end))
        .collect::<Result<Vec<_>>>()?;
    not_ended(root)?;
    finish(
        root,
        Outcome::Question {
            question,
            sentences,
        },
    )
}

pub(super) struct Citation {
    pub(super) line: Option<u32>,
    pub(super) lines: Option<String>,
    pub(super) file: Option<String>,
}
impl Citation {
    pub(super) fn unable(self, root: &Path, reason: String) -> Result<(bool, Value)> {
        let Self { line, lines, file } = self;

        bounded("the reason", &reason, MAX_REASON_BYTES)?;
        if let Some(file) = file {
            anyhow::ensure!(
                line.is_none() && lines.is_none(),
                "--file names a fault in one of the host's files; do not name a sentence too."
            );
            anyhow::ensure!(
                HOST_FILES.contains(&file.as_str()),
                "--file {file}: name one of the host's files ({}); a fault in your draft or cases is yours to fix.",
                HOST_FILES.join(", ")
            );
            not_ended(root)?;
            return finish(
                root,
                Outcome::Unable {
                    reason,
                    line: None,
                    end_line: None,
                    sentence: None,
                    file: Some(file),
                },
            );
        }
        let (line, end) = match (line, lines) {
            (Some(line), None) => (line, None),
            (None, Some(range)) => parse_range(&range)?,
            _ => bail!(
                "name the sentence once: --line N for one line, or --lines A-B for a sentence over lines A to B (or --file for a fault in one of the host's files)."
            ),
        };
        let source = read_inside(root, SOURCE_FILE, MAX_SOURCE_BYTES)?;
        let Sentence {
            line,
            end_line,
            text,
        } = SentenceSource::new(&source).quote(line, end)?;
        not_ended(root)?;
        finish(
            root,
            Outcome::Unable {
                reason,
                line: Some(line),
                end_line,
                sentence: Some(text),
                file: None,
            },
        )
    }
}

fn bounded(what: &str, text: &str, max: usize) -> Result<()> {
    anyhow::ensure!(
        !text.trim().is_empty(),
        "{what} is empty; write it in the owner's words."
    );
    anyhow::ensure!(
        text.len() <= max,
        "{what} is {} bytes; keep it to {max}.",
        text.len()
    );
    Ok(())
}
/// `A-B`: the first and last line of a sentence (`None` when they are one).
fn parse_range(range: &str) -> Result<(u32, Option<u32>)> {
    let refused = || {
        anyhow!(
            "--lines {range}: write the first and last line of the sentence as A-B, such as --lines 21-22, with A no greater than B; use --line N for one line."
        )
    };
    let (a, b) = range.split_once('-').ok_or_else(refused)?;
    let (a, b): (u32, u32) = (
        a.trim().parse().map_err(|_| refused())?,
        b.trim().parse().map_err(|_| refused())?,
    );
    anyhow::ensure!(a >= 1 && a <= b, refused());
    Ok((a, (b != a).then_some(b)))
}
fn shown(line: u32, end: Option<u32>) -> String {
    match end {
        Some(end) => format!("lines {line}-{end}"),
        None => format!("line {line}"),
    }
}

fn ends(text: &str) -> bool {
    let text = text
        .trim_end()
        .trim_end_matches(['"', '\'', '\u{201d}', '\u{2019}', ')', ']', '*', '_']);
    text.ends_with(['.', '!', '?', ':'])
}
fn starts_block(text: &str) -> bool {
    let text = text.trim_start();
    let numbered = text.split_once(['.', ')']).is_some_and(|(n, rest)| {
        !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && rest.starts_with(' ')
    });
    numbered
        || ["- ", "* ", "+ ", "#", ">", "|"]
            .iter()
            .any(|p| text.starts_with(p))
}
struct SentenceSource<'a> {
    source: &'a str,
    lines: Vec<&'a str>,
}
impl<'a> SentenceSource<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            lines: source.lines().collect(),
        }
    }
    /// The lines named hold whole sentences: the line before them is blank, or
    /// ends a sentence, or they start a new block (a list item or heading); and
    /// their last line ends a sentence, or the next line is blank or starts a
    /// new block. So a quote never stops in the middle of a sentence.
    fn quote(&self, line: u32, end: Option<u32>) -> Result<Sentence> {
        let sentence = quote(self.source, line, end).map_err(anyhow::Error::msg)?;
        let first = line as usize - 1;
        let last = end.unwrap_or(line) as usize - 1;
        let shown = |a: usize, b: usize| {
            if a == b {
                format!("--line {}", a + 1)
            } else {
                format!("--lines {}-{}", a + 1, b + 1)
            }
        };
        // The sentence's whole extent, which the suggestion names, so that it is
        // itself accepted.
        let (mut start, mut finish) = (first, last);
        while start > 0 && self.continued_from(start) {
            start -= 1;
        }
        while finish + 1 < self.lines.len() && self.continues_into(finish) {
            finish += 1;
        }
        if start < first {
            bail!(
                "line {}: the sentence starts on line {}, which it goes on from. Name the whole sentence, from its first line, such as {}.",
                first + 1,
                start + 1,
                shown(start, finish)
            );
        }
        if finish > last {
            bail!(
                "line {}: the sentence goes on in line {}. Name the whole sentence, to its last line, such as {}.",
                last + 1,
                last + 2,
                shown(start, finish)
            );
        }
        Ok(sentence)
    }
    fn continued_from(&self, current: usize) -> bool {
        let previous = self.lines[current - 1];
        !previous.trim().is_empty() && !ends(previous) && !starts_block(self.lines[current])
    }

    fn continues_into(&self, current: usize) -> bool {
        let next = self.lines[current + 1];
        !next.trim().is_empty() && !starts_block(next) && !ends(self.lines[current])
    }
}
