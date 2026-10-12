//! `question` (`core/src/tool/plugin/question.ts`).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::host::{FormField, FormOption, FormRequest, FormResponse};
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};

pub const DESCRIPTION: &str = "Use this tool when you need to ask the user questions during execution. This allows you to:\n1. Gather user preferences or requirements\n2. Clarify ambiguous instructions\n3. Get decisions on implementation choices as you work\n4. Offer choices to the user about what direction to take.\n\nUsage notes:\n- A \"Type your own answer\" option is added automatically; don't include a separate option for free form answers\n- Set `multiple: true` to allow selecting more than one option\n- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label";

#[derive(Deserialize, Clone)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Deserialize, Clone)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub multiple: Option<bool>,
}

#[derive(Deserialize)]
struct Input {
    questions: Vec<Question>,
}

pub fn to_model_content(questions: &[Question], answers: &[Vec<String>]) -> String {
    let formatted = questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let answer = answers.get(i).filter(|a| !a.is_empty()).map(|a| a.join(", ")).unwrap_or_else(|| "Unanswered".into());
            format!("\"{}\"=\"{answer}\"", q.question)
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("User has answered your questions: {formatted}. You can now continue with the user's answers in mind.")
}

fn to_field(question: &Question, index: usize) -> FormField {
    FormField {
        key: format!("q{index}"),
        title: Some(question.header.clone()),
        description: Some(question.question.clone()),
        kind: if question.multiple == Some(true) { "multiselect".into() } else { "string".into() },
        required: None,
        options: question
            .options
            .iter()
            .map(|o| FormOption { value: o.label.clone(), label: o.label.clone(), description: Some(o.description.clone()) })
            .collect(),
        custom: true,
    }
}

fn answer_list(value: Option<&Value>) -> Vec<String> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(other) => vec![other.to_string()],
    }
}

pub struct QuestionTool;

#[async_trait]
impl Tool for QuestionTool {
    fn name(&self) -> &str {
        "question"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("question", DESCRIPTION, include_str!("../../schemas/question.json"))
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(input)?;
        super::assert(&ctx, "question", vec!["*".into()], None, None).await.map_err(super::perm("Permission denied: question"))?;
        let request = FormRequest {
            session_id: ctx.session_id.clone(),
            title: "Questions".into(),
            metadata: meta(vec![("kind", json!("question")), ("tool", json!({"messageID": ctx.message_id, "id": ctx.call_id}))]),
            fields: input.questions.iter().enumerate().map(|(i, q)| to_field(q, i)).collect(),
        };
        let response = ctx
            .host
            .ask_form(request)
            .await
            .map_err(|m| ToolError::with_cause("Permission denied: question", ErrorCause::Error(m)))?;
        let answer = match response {
            FormResponse::Cancelled { message: Some(message) } => return Err(ToolError::new(message)),
            FormResponse::Cancelled { message: None } => return Err(ToolError::question_dismissed()),
            FormResponse::Answered(answer) => answer,
        };
        let answers: Vec<Vec<String>> = (0..input.questions.len()).map(|i| answer_list(answer.get(&format!("q{i}")))).collect();
        Ok(ToolResult::text(to_model_content(&input.questions, &answers))
            .with_output(json!({"answers": answers}))
            .with_metadata(meta(vec![("answers", json!(answers))])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_answers() {
        let q = |t: &str| Question { question: t.into(), header: "h".into(), options: vec![], multiple: None };
        let text = to_model_content(&[q("A?"), q("B?")], &[vec!["x".into(), "y".into()], vec![]]);
        assert_eq!(text, "User has answered your questions: \"A?\"=\"x, y\", \"B?\"=\"Unanswered\". You can now continue with the user's answers in mind.");
        assert_eq!(answer_list(Some(&json!(true))), vec!["true"]);
    }
}
