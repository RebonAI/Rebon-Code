//! Parsing the transcript text an answered `AskUserQuestion` leaves behind.
//!
//! The answer reaches the model as an ordinary user message, because that is
//! what it is: the user said something and the turn resumed. But it reads
//! nothing like a prompt someone typed, so the transcript renders it as its own
//! card, and a card needs the questions and answers apart rather than as one
//! blob of text.
//!
//! The text comes from `format_ask_user_question_answer_for_transcript` in
//! `rebon-core`, which is where the shape below is decided and pinned by test.
//! Nothing else may be handed to this parser: anything that does not match is
//! rejected whole, so a prompt that merely opens with the same line keeps the
//! ordinary user-message rendering instead of half-filling a card.

/// First line of the text this module parses.
pub const ANSWERED_QUESTIONS_HEADER: &str = "Answered questions:";

const QUESTION_PREFIX: &str = "- ";
const ANSWER_PREFIX: &str = "  Answer: ";
const PREVIEW_HEADER: &str = "  Selected preview:";
const PREVIEW_INDENT: &str = "    ";
const NOTES_PREFIX: &str = "  Notes: ";

/// One answered question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsweredQuestion {
    /// The question as it was asked.
    pub question: String,
    /// The option the user picked, or the text they wrote instead.
    pub answer: String,
    /// The preview attached to the picked option, if it had one.
    pub preview: Option<String>,
    /// Free-text notes the user added to the selection.
    pub notes: Option<String>,
}

/// Parse an `AskUserQuestion` answer message, or `None` if this is not one.
///
/// `None` is the signal to render the message as ordinary user text, so every
/// deviation returns it: a missing header, a question with no answer line, or
/// a line the format does not account for.
pub fn parse_answered_questions(text: &str) -> Option<Vec<AnsweredQuestion>> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != ANSWERED_QUESTIONS_HEADER {
        return None;
    }

    let mut answered: Vec<AnsweredQuestion> = Vec::new();
    let mut lines = lines.peekable();
    while let Some(line) = lines.next() {
        let question = line.strip_prefix(QUESTION_PREFIX)?;
        let answer = lines.next()?.strip_prefix(ANSWER_PREFIX)?;

        let mut preview: Option<String> = None;
        if lines.next_if(|line| *line == PREVIEW_HEADER).is_some() {
            let mut preview_lines: Vec<&str> = Vec::new();
            while let Some(line) = lines.next_if(|line| line.starts_with(PREVIEW_INDENT)) {
                preview_lines.push(&line[PREVIEW_INDENT.len()..]);
            }
            // A preview header with nothing under it is not something the
            // formatter emits, so it means this is not our text after all.
            if preview_lines.is_empty() {
                return None;
            }
            preview = Some(preview_lines.join("\n"));
        }

        let notes = lines
            .next_if(|line| line.starts_with(NOTES_PREFIX))
            .map(|line| line[NOTES_PREFIX.len()..].to_string());

        answered.push(AnsweredQuestion {
            question: question.to_string(),
            answer: answer.to_string(),
            preview,
            notes,
        });
    }

    (!answered.is_empty()).then_some(answered)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact text `rebon-core` produces, as its own tests pin it.
    const FULL: &str = "Answered questions:\n\
         - Which UI should we build?\n  \
           Answer: Cards\n  \
           Selected preview:\n    \
             <div>Cards mockup</div>\n  \
           Notes: Prefer dense layout\n\
         - Which database should we use?\n  \
           Answer: Postgres";

    #[test]
    fn parses_every_field_the_formatter_can_emit() {
        let answered = parse_answered_questions(FULL).expect("parses");
        assert_eq!(
            answered,
            vec![
                AnsweredQuestion {
                    question: "Which UI should we build?".into(),
                    answer: "Cards".into(),
                    preview: Some("<div>Cards mockup</div>".into()),
                    notes: Some("Prefer dense layout".into()),
                },
                AnsweredQuestion {
                    question: "Which database should we use?".into(),
                    answer: "Postgres".into(),
                    preview: None,
                    notes: None,
                },
            ]
        );
    }

    #[test]
    fn keeps_a_multi_line_preview_whole() {
        let answered = parse_answered_questions(
            "Answered questions:\n- Q?\n  Answer: A\n  Selected preview:\n    one\n    two",
        )
        .expect("parses");
        assert_eq!(answered[0].preview.as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn rejects_anything_that_is_not_this_format() {
        for text in [
            "",
            "Answered questions:",
            "Answered questions",
            "Answered questions:\n- Which database should we use?",
            "Answered questions:\n  Answer: Postgres",
            "Answered questions:\n- Q?\n  Answer: A\n  Selected preview:",
            "Answered questions:\n- Q?\n  Answer: A\nstray trailing line",
            "Answered questions: and then a sentence the user actually typed",
        ] {
            assert!(
                parse_answered_questions(text).is_none(),
                "should not have parsed: {text:?}"
            );
        }
    }

    #[test]
    fn an_answer_may_carry_the_prefixes_it_likes() {
        let answered =
            parse_answered_questions("Answered questions:\n- - dashes -\n  Answer: - Answer: x")
                .expect("parses");
        assert_eq!(answered[0].question, "- dashes -");
        assert_eq!(answered[0].answer, "- Answer: x");
    }
}
