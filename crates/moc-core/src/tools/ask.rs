//! 通过前端反向请求完成交互式提问。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::{AskOption, AskParams, AskQuestion, SessionRequestAsk};
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskArgs {
    questions: Vec<QuestionInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionInput {
    id: String,
    question: String,
    header: Option<String>,
    multi: Option<bool>,
    recommended: Option<u32>,
    options: Vec<OptionInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionInput {
    label: String,
    description: Option<String>,
    preview: Option<String>,
}

pub struct AskTool;

impl Tool for AskTool {
    fn name(&self) -> &'static str {
        "ask"
    }
    fn description(&self) -> &'static str {
        "Ask the user one or more decision questions. Use only when the answer materially affects the implementation. Options may include a preview: the frontend shows it beside the focused option for visual comparison of ASCII layouts, code, or configuration (single-select only)."
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "ask",
            schema(
                serde_json::json!({
                    "questions":{"type":"array","minItems":1,"items":{
                        "type":"object",
                        "properties":{
                            "id":{"type":"string"},
                            "question":{"type":"string"},
                            "header":{"type":"string"},
                            "multi":{"type":"boolean"},
                            "recommended":{"type":"integer","minimum":0},
                            "options":{"type":"array","minItems":2,"items":{
                                "type":"object","properties":{
                                    "label":{"type":"string","description":"Display text, 1-5 words, briefly naming the choice"},
                                    "description":{"type":"string","description":"Meaning or consequence of selecting this option"},
                                    "preview":{"type":"string","description":"Markdown preview shown beside the focused option in a monospace frame: ASCII layout, code, or configuration to compare visually. Preserve multiline text; put alignment-sensitive ASCII art in a ``` code block. Supported only for single-select questions (multi=false or omitted)."}
                                },"required":["label"],"additionalProperties":false
                            }}
                        },"required":["id","question","options"],"additionalProperties":false
                    }}
                }),
                &["questions"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let questions = normalize_questions(args)?;
            let request_id = crate::ids::entry_id();
            let params = AskParams {
                session_id: ctx.session_id.clone(),
                prompt_id: ctx.prompt_id.clone(),
                request_id,
                questions: questions.clone(),
            };
            let result = ctx
                .request
                .requester()
                .request::<SessionRequestAsk>(params)
                .await
                .map_err(|error| {
                    ToolError::new(
                        "E_ASK_UNAVAILABLE",
                        format!("The frontend could not handle the ask request: {error}"),
                    )
                })?;
            if result.answers.is_empty() {
                return Ok(ToolOutcome::success("The user did not answer."));
            }
            let answers: Vec<String> = questions
                .iter()
                .map(|question| {
                    let Some(answer) = result
                        .answers
                        .iter()
                        .find(|answer| answer.id == question.id)
                    else {
                        return "The user did not answer.".into();
                    };
                    let selected = answer.selected.join(", ");
                    match (selected.is_empty(), answer.custom.as_deref()) {
                        (_, Some(custom)) if !custom.is_empty() && selected.is_empty() => {
                            custom.to_owned()
                        }
                        (_, Some(custom)) if !custom.is_empty() => format!("{selected} ({custom})"),
                        (false, _) => selected,
                        _ => "The user did not answer.".into(),
                    }
                })
                .collect();
            let output = if answers.len() == 1 {
                answers[0].clone()
            } else {
                let mut output = String::from("User answers:\n");
                for (question, answer) in questions.iter().zip(answers) {
                    output.push_str(&format!("{}: {}\n", question.id, answer));
                }
                output
            };
            Ok(ToolOutcome::success(output))
        })
    }
}

/// 归一化预览文本：未提供或只有空白都视为没有预览，避免前端为
/// 空内容切到并排布局；外围空白（含首尾空行）在此剥掉。
fn preview_text(preview: &Option<String>) -> Option<String> {
    preview
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// 解析并校验 ask 载荷：逐题检查必填项、保留标签、单选预览约束，
/// 并把选项归一化成协议结构。
fn normalize_questions(args: Value) -> Result<Vec<AskQuestion>, ToolError> {
    let args: AskArgs = serde_json::from_value(args)
        .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
    if args.questions.is_empty() {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "questions must not be empty",
        ));
    }
    let mut questions = Vec::with_capacity(args.questions.len());
    for input in args.questions {
        if input.id.trim().is_empty() || input.question.trim().is_empty() || input.options.len() < 2
        {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                "each question requires id, question, and at least two options",
            ));
        }
        if input
            .options
            .iter()
            .any(|option| option.label == "Other (type your own)")
        {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                "Other (type your own) is a reserved option label",
            ));
        }
        let multi = input.multi.unwrap_or(false);
        if multi
            && input
                .options
                .iter()
                .any(|option| preview_text(&option.preview).is_some())
        {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                "preview supports single-select questions only (multi=false); multi-select has no focused option for side-by-side preview",
            ));
        }
        if input
            .recommended
            .is_some_and(|index| index as usize >= input.options.len())
        {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                "recommended must point to an existing option",
            ));
        }
        questions.push(AskQuestion {
            id: input.id,
            question: input.question,
            header: input.header,
            multi,
            recommended: input.recommended,
            options: input
                .options
                .into_iter()
                .map(|option| AskOption {
                    label: option.label,
                    description: option.description,
                    preview: preview_text(&option.preview),
                })
                .collect(),
        });
    }
    Ok(questions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 单选问题的预览原样透传给前端，空白预览被剥成「无预览」。
    #[test]
    fn preview_survives_normalization_and_blank_is_dropped() {
        let questions = normalize_questions(json!({
            "questions": [{
                "id": "layout",
                "question": "选哪个布局？",
                "options": [
                    {"label": "上中下", "preview": "\n```\n+---+\n| A |\n+---+\n```\n"},
                    {"label": "左右", "preview": "   \n"}
                ]
            }]
        }))
        .expect("载荷可归一化");

        assert_eq!(questions.len(), 1);
        assert_eq!(
            questions[0].options[0].preview.as_deref(),
            Some("```\n+---+\n| A |\n+---+\n```")
        );
        assert_eq!(questions[0].options[1].preview, None);
        assert!(!questions[0].multi);
    }

    /// 多选问题没有聚焦项，带预览直接拒绝并指明原因。
    #[test]
    fn multi_select_rejects_preview() {
        let error = normalize_questions(json!({
            "questions": [{
                "id": "features",
                "question": "开启哪些特性？",
                "multi": true,
                "options": [
                    {"label": "A", "preview": "```\nx\n```"},
                    {"label": "B"}
                ]
            }]
        }))
        .expect_err("多选带预览应被拒绝");

        assert_eq!(error.code, "E_BAD_PAYLOAD");
        assert!(
            error
                .message
                .contains("preview supports single-select questions"),
            "{}",
            error.message
        );
    }

    /// 工具 schema 暴露预览字段及其格式说明。
    #[test]
    fn schema_exposes_preview_field() {
        let schema = AskTool.def().parameters;
        let option = &schema["properties"]["questions"]["items"]["properties"]["options"]["items"];
        assert_eq!(option["properties"]["preview"]["type"], json!("string"));
        assert!(
            option["properties"]["preview"]["description"]
                .as_str()
                .is_some_and(|text| text.contains("Supported only for single-select questions")),
            "{option}"
        );
        assert!(AskTool.description().contains("preview"));
    }
}
