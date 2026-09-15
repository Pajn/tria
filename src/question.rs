//! Draft answers for a pending agent question: one question at a time, digit
//! keys select options, a free-text answer is allowed unless the provider says
//! otherwise, and the record is submitted once every question has an answer.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::state::{PendingUserInput, Question};

#[derive(Debug, Clone, Default)]
pub struct DraftAnswer {
    pub selected: BTreeSet<usize>,
    pub custom: String,
}

#[derive(Debug, Clone)]
pub struct QuestionDraft {
    pub request_id: String,
    pub questions: Vec<Question>,
    pub index: usize,
    pub highlight: usize,
    pub answers: Vec<DraftAnswer>,
}

impl QuestionDraft {
    pub fn new(pending: &PendingUserInput) -> Self {
        let answers = vec![DraftAnswer::default(); pending.questions.len()];
        Self {
            request_id: pending.request_id.clone(),
            questions: pending.questions.clone(),
            index: 0,
            highlight: 0,
            answers,
        }
    }

    pub fn current(&self) -> &Question {
        &self.questions[self.index]
    }

    pub fn current_answer(&self) -> &DraftAnswer {
        &self.answers[self.index]
    }

    pub fn is_last(&self) -> bool {
        self.index + 1 >= self.questions.len()
    }

    /// Select (single) or toggle (multi) the option at `option`. Clears any custom text.
    pub fn choose(&mut self, option: usize) {
        let multi = self.current().multi_select;
        if option >= self.current().options.len() {
            return;
        }
        let answer = &mut self.answers[self.index];
        answer.custom.clear();
        if multi {
            if !answer.selected.remove(&option) {
                answer.selected.insert(option);
            }
        } else {
            answer.selected.clear();
            answer.selected.insert(option);
        }
        self.highlight = option;
    }

    pub fn set_custom(&mut self, text: String) {
        let answer = &mut self.answers[self.index];
        if !text.trim().is_empty() {
            answer.selected.clear();
        }
        answer.custom = text;
    }

    pub fn move_highlight(&mut self, delta: isize) {
        let len = self.current().options.len();
        if len == 0 {
            return;
        }
        self.highlight = (self.highlight as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// The wire value for one question, or `None` while unanswered.
    pub fn resolve(&self, index: usize) -> Option<Value> {
        let question = &self.questions[index];
        let answer = &self.answers[index];
        let custom = answer.custom.trim();
        if question.allow_custom && !custom.is_empty() {
            return Some(Value::String(custom.to_string()));
        }
        let values: Vec<Value> = answer
            .selected
            .iter()
            .filter_map(|i| question.options.get(*i))
            .map(|o| Value::String(o.value.clone()))
            .collect();
        if values.is_empty() {
            return None;
        }
        if question.multi_select {
            Some(Value::Array(values))
        } else {
            values.into_iter().next()
        }
    }

    pub fn is_answered(&self, index: usize) -> bool {
        self.resolve(index).is_some()
    }

    /// Advance to the next unanswered question, or return the full answer record.
    pub fn advance(&mut self) -> Option<Value> {
        if !self.is_answered(self.index) {
            return None;
        }
        if let Some(next) = (self.index + 1..self.questions.len()).find(|i| !self.is_answered(*i)) {
            self.index = next;
            self.highlight = 0;
            return None;
        }
        if (0..self.questions.len()).all(|i| self.is_answered(i)) {
            let mut record = Map::new();
            for (i, question) in self.questions.iter().enumerate() {
                record.insert(question.id.clone(), self.resolve(i).unwrap_or(json!("")));
            }
            return Some(Value::Object(record));
        }
        if let Some(first) = (0..self.questions.len()).find(|i| !self.is_answered(*i)) {
            self.index = first;
            self.highlight = 0;
        }
        None
    }

    pub fn back(&mut self) {
        if self.index > 0 {
            self.index -= 1;
            self.highlight = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::QuestionOption;

    fn option(label: &str) -> QuestionOption {
        QuestionOption {
            label: label.into(),
            description: String::new(),
            value: label.into(),
        }
    }

    fn pending() -> PendingUserInput {
        PendingUserInput {
            request_id: "r".into(),
            dismissible: false,
            questions: vec![
                Question {
                    id: "Color?".into(),
                    header: "Color".into(),
                    text: "Color?".into(),
                    options: vec![option("Red"), option("Blue")],
                    allow_custom: true,
                    multi_select: false,
                },
                Question {
                    id: "Toppings?".into(),
                    header: "Toppings".into(),
                    text: "Toppings?".into(),
                    options: vec![option("Cheese"), option("Olives"), option("Ham")],
                    allow_custom: false,
                    multi_select: true,
                },
            ],
        }
    }

    #[test]
    fn single_then_multi_select_builds_record() {
        let mut draft = QuestionDraft::new(&pending());
        assert!(draft.advance().is_none());
        draft.choose(1);
        assert!(draft.advance().is_none());
        assert_eq!(draft.index, 1);
        draft.choose(0);
        draft.choose(2);
        draft.choose(0);
        let record = draft.advance().expect("complete");
        assert_eq!(record["Color?"], json!("Blue"));
        assert_eq!(record["Toppings?"], json!(["Ham"]));
    }

    #[test]
    fn custom_text_overrides_selection_when_allowed() {
        let mut draft = QuestionDraft::new(&pending());
        draft.choose(0);
        draft.set_custom("Green".into());
        assert_eq!(draft.resolve(0), Some(json!("Green")));
        draft.index = 1;
        draft.set_custom("Pineapple".into());
        assert_eq!(draft.resolve(1), None);
    }
}
