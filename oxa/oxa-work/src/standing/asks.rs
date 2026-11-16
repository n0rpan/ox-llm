use super::{types, Rule, Scope, Source};
use crate::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub label: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub scope: Scope,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub recommended: String,
    /// Unknown stays None; no default action or authority is inferred from prose here.
    pub meanwhile: Option<String>,
    pub source: Source,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Choice {
    Option(String),
    Text(String),
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub revision: u64,
    pub question_revision: u64,
    pub choice: Choice,
    pub at: u64,
    pub source: Source,
    pub said: String,
}
pub struct AnswerRequest {
    pub ask_id: String,
    pub question_revision: u64,
    pub choice: Choice,
    pub source: Source,
    pub said: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub rule: Rule,
    pub accept_label: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    pub id: String,
    pub revision: u64,
    pub scope: Scope,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub recommended: String,
    pub meanwhile: Option<String>,
    pub at: u64,
    pub source: Source,
    pub answer: Option<Answer>,
    pub proposal: Option<Proposal>,
}
impl Question {
    pub(super) fn validate(&self) -> Result<()> {
        self.scope.validate()?;
        self.source.validate()?;
        types::text(&self.question, 512)?;
        types::text(&self.recommended, 64)?;
        if let Some(value) = &self.meanwhile {
            types::text(value, 512)?;
        }
        if !(2..=4).contains(&self.options.len()) {
            return Err("ask requires 2 to 4 options".into());
        }
        let mut labels = std::collections::HashSet::new();
        for option in &self.options {
            types::text(&option.label, 64)?;
            types::text(&option.detail, 512)?;
            if !labels.insert(&option.label) {
                return Err("ask option labels must be unique".into());
            }
        }
        if !labels.contains(&self.recommended) {
            return Err("ask recommendation must name an option".into());
        }
        Ok(())
    }
}
impl Ask {
    pub(super) fn validate_choice(&self, choice: &Choice) -> Result<()> {
        match choice {
            Choice::Option(label) if self.options.iter().any(|option| option.label == *label) => Ok(()),
            Choice::Option(_) => Err("answer does not name an ask option".into()),
            Choice::Text(value) => types::text(value, 1024),
        }
    }
    pub(super) fn validate(&self) -> Result<()> {
        types::id(&self.id)?;
        if self.revision == 0 {
            return Err("ask revision must be positive".into());
        }
        Question {
            scope: self.scope.clone(),
            question: self.question.clone(),
            options: self.options.clone(),
            recommended: self.recommended.clone(),
            meanwhile: self.meanwhile.clone(),
            source: self.source.clone(),
        }
        .validate()?;
        if let Some(answer) = &self.answer {
            if answer.question_revision != self.revision {
                return Err("answer names a different question revision".into());
            }
            answer.source.validate()?;
            types::text(&answer.said, 1024)?;
            self.validate_choice(&answer.choice)?;
        }
        if let Some(proposal) = &self.proposal {
            proposal.rule.validate()?;
            if proposal.rule.scope != self.scope
                || !self
                    .options
                    .iter()
                    .any(|option| option.label == proposal.accept_label)
            {
                return Err("proposal is not bound to this question".into());
            }
        }
        Ok(())
    }
    pub(super) fn fact(&self) -> oxa_proto::work::Ask {
        let subject = match &self.scope {
            Scope::PullRequest(pr) => oxa_proto::work::Subject::PullRequest(*pr),
            Scope::Task(task) => oxa_proto::work::Subject::Task(task.clone()),
            Scope::Host(host) => oxa_proto::work::Subject::Host(host.clone()),
            Scope::Repository | Scope::Branch(_) | Scope::Path(_) => oxa_proto::work::Subject::Repository,
        };
        oxa_proto::work::Ask {
            id: self.id.clone(),
            subject,
            question: self.question.clone(),
        }
    }
}
