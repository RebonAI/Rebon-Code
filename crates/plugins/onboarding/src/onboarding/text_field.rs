//! A single-line text field with a byte cursor, and the character-boundary
//! arithmetic it moves by.
//!
//! Editing a UTF-8 string by byte offset is the part of a text input that
//! has nothing to do with drawing one: every method here is a pure edit of
//! `value` and `cursor`, and the boundary helpers are the reason a
//! multi-byte character deletes as one character rather than one byte.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextField {
    pub value: String,
    pub cursor: usize,
}

impl TextField {
    pub fn insert(&mut self, ch: char) {
        self.value.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn insert_str(&mut self, value: &str) {
        self.value.insert_str(self.cursor, value);
        self.cursor += value.len();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = prev_char_boundary(&self.value, self.cursor);
        self.value.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        let end = next_char_boundary(&self.value, self.cursor);
        self.value.drain(self.cursor..end);
    }

    pub fn left(&mut self) {
        self.cursor = prev_char_boundary(&self.value, self.cursor);
    }

    pub fn right(&mut self) {
        self.cursor = next_char_boundary(&self.value, self.cursor);
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.value.len();
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    pub fn set(&mut self, value: &str) {
        self.value = value.to_string();
        self.cursor = self.value.len();
    }
}

pub fn normalize_single_line_paste(text: &str) -> String {
    text.trim()
        .chars()
        .filter(|ch| !matches!(ch, '\r' | '\n'))
        .collect()
}

pub fn prev_char_boundary(input: &str, cursor: usize) -> usize {
    input[..cursor]
        .char_indices()
        .last()
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

pub fn next_char_boundary(input: &str, cursor: usize) -> usize {
    if cursor >= input.len() {
        return input.len();
    }
    let mut iter = input[cursor..].char_indices();
    let Some((_, ch)) = iter.next() else {
        return input.len();
    };
    cursor + ch.len_utf8()
}
