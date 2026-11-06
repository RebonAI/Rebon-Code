//! The router's words to the person.
//!
//! A router session talks to the person through one tool: what it passes to
//! `TellPerson` is what the person reads. Its plain text, its other tool
//! calls and the digests it reads are its work, which the person does not
//! follow. Both the engine (the reply a grant is checked against, the time of
//! the last report) and the surfaces that draw a router conversation read the
//! person's side through this module.

use serde_json::Value;

/// The router's tool for speaking to the person.
pub const TELL_PERSON_TOOL_NAME: &str = "TellPerson";

/// The field of a `TellPerson` call that holds what the person reads.
pub const TELL_PERSON_MESSAGE_FIELD: &str = "message";

/// What a tool call tells the person: the message of a `TellPerson` call,
/// trimmed, or `None` for any other call or an empty message.
pub fn tell_person_message<'a>(tool_name: &str, input: &'a Value) -> Option<&'a str> {
    (tool_name == TELL_PERSON_TOOL_NAME)
        .then(|| input.get(TELL_PERSON_MESSAGE_FIELD)?.as_str())
        .flatten()
        .map(str::trim)
        .filter(|message| !message.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_a_tell_person_call_with_words_speaks() {
        let input = json!({ "message": "  t-1 is done.  " });
        assert_eq!(
            tell_person_message("TellPerson", &input),
            Some("t-1 is done.")
        );
        assert_eq!(tell_person_message("Recall", &input), None);
        assert_eq!(
            tell_person_message("TellPerson", &json!({ "message": " " })),
            None
        );
        assert_eq!(tell_person_message("TellPerson", &json!({})), None);
    }
}
