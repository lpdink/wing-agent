//! AskFlow — state machine for multi-question Ask events.
//!
//! When the agent sends an Ask event with multiple questions, the TUI
//! enters this flow: questions are presented one at a time, the user
//! answers each via the normal input area, and after the last question
//! a structured JSON response is sent back to the agent.

use crate::protocol::AskQuestion;

/// Multi-question answering state machine.
#[derive(Debug, Clone)]
pub struct AskFlow {
    /// Correlation id of the Ask event — echoed back when sending the
    /// final response so the gateway resolves the right feedback waiter.
    pub tool_call_id: String,
    /// All questions from the Ask event.
    pub questions: Vec<AskQuestion>,
    /// Index of the question currently being answered.
    pub current_idx: usize,
    /// Collected answers (parallel to `questions`).
    pub answers: Vec<String>,
}

impl AskFlow {
    pub fn new(tool_call_id: String, questions: Vec<AskQuestion>) -> Self {
        let len = questions.len();
        Self {
            tool_call_id,
            questions,
            current_idx: 0,
            answers: vec![String::new(); len],
        }
    }

    /// Total number of questions.
    pub fn len(&self) -> usize {
        self.questions.len()
    }

    /// Whether there are no questions.
    pub fn is_empty(&self) -> bool {
        self.questions.is_empty()
    }

    /// Current question being answered.
    pub fn current(&self) -> Option<&AskQuestion> {
        self.questions.get(self.current_idx)
    }

    /// 1-based progress display: "1/3".
    pub fn progress(&self) -> String {
        if self.questions.is_empty() {
            return "0/0".to_string();
        }
        format!("{}/{}", self.current_idx + 1, self.questions.len())
    }

    /// Submit an answer for the current question and advance.
    ///
    /// Returns `Some(json)` when all questions are answered (the final
    /// structured response to send to the agent), or `None` if there
    /// are more questions to answer.
    pub fn advance(&mut self, answer: &str) -> Option<String> {
        if self.current_idx < self.answers.len() {
            self.answers[self.current_idx] = answer.to_string();
        }
        self.current_idx += 1;

        if self.current_idx >= self.questions.len() {
            Some(self.build_response())
        } else {
            None
        }
    }

    /// Build the JSON response mapping question IDs to answers.
    fn build_response(&self) -> String {
        let map: serde_json::Map<String, serde_json::Value> = self
            .questions
            .iter()
            .zip(self.answers.iter())
            .map(|(q, a)| (q.id.clone(), serde_json::Value::String(a.clone())))
            .collect();
        serde_json::to_string(&map).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_questions(n: usize) -> Vec<AskQuestion> {
        (0..n)
            .map(|i| AskQuestion {
                id: format!("q{}", i + 1),
                question: format!("Question {}?", i + 1),
                choices: vec![],
            })
            .collect()
    }

    #[test]
    fn test_single_question() {
        let mut flow = AskFlow::new("tc".into(), make_questions(1));
        assert_eq!(flow.progress(), "1/1");
        let result = flow.advance("yes");
        assert!(result.is_some());
        let json = result.unwrap();
        assert_eq!(json, r#"{"q1":"yes"}"#);
    }

    #[test]
    fn test_multi_question() {
        let mut flow = AskFlow::new("tc".into(), make_questions(3));
        assert_eq!(flow.progress(), "1/3");

        assert!(flow.advance("answer1").is_none());
        assert_eq!(flow.progress(), "2/3");
        assert_eq!(flow.current().unwrap().id, "q2");

        assert!(flow.advance("answer2").is_none());
        assert_eq!(flow.progress(), "3/3");

        let result = flow.advance("answer3");
        assert!(result.is_some());
        let json = result.unwrap();
        assert_eq!(json, r#"{"q1":"answer1","q2":"answer2","q3":"answer3"}"#);
    }

    #[test]
    fn test_answers_with_special_chars() {
        let mut flow = AskFlow::new("tc".into(), make_questions(1));
        let result = flow.advance(r#"say "hello" and \ stuff"#);
        let json = result.unwrap();
        // Should be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["q1"], r#"say "hello" and \ stuff"#);
    }

    #[test]
    fn test_empty_flow() {
        let flow = AskFlow::new("tc".into(), vec![]);
        assert!(flow.is_empty());
        assert_eq!(flow.current(), None);
    }
}
