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
        "向用户询问一个或多个决策问题；只在用户答案会实质影响实现时调用。选项可带 preview：前端聚焦该项时并排渲染 markdown 预览，用于对比 ASCII 示意图、代码片段、配置示例等候选形态（仅单选问题）。"
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
                                    "label":{"type":"string","description":"选项显示文本，1-5 个词，简洁说明选择内容"},
                                    "description":{"type":"string","description":"该项的含义或选中后的结果，用于说明取舍"},
                                    "preview":{"type":"string","description":"聚焦该项时并排展示的 markdown 预览（等宽框渲染）：ASCII 界面示意图、代码片段、配置示例等需要肉眼对比的候选形态。多行文本按原样保留；依赖对齐的 ASCII 图请放进 ``` 代码块。仅单选问题（multi 为 false 或省略）支持。"}
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
                        format!("前端无法处理 ask 请求：{error}"),
                    )
                })?;
            if result.answers.is_empty() {
                return Ok(ToolOutcome::success("用户未作答。"));
            }
            let answers: Vec<String> = questions
                .iter()
                .map(|question| {
                    let Some(answer) = result
                        .answers
                        .iter()
                        .find(|answer| answer.id == question.id)
                    else {
                        return "用户未作答。".into();
                    };
                    let selected = answer.selected.join(", ");
                    match (selected.is_empty(), answer.custom.as_deref()) {
                        (_, Some(custom)) if !custom.is_empty() && selected.is_empty() => {
                            custom.to_owned()
                        }
                        (_, Some(custom)) if !custom.is_empty() => format!("{selected} ({custom})"),
                        (false, _) => selected,
                        _ => "用户未作答。".into(),
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
        return Err(ToolError::new("E_BAD_PAYLOAD", "questions 不能为空"));
    }
    let mut questions = Vec::with_capacity(args.questions.len());
    for input in args.questions {
        if input.id.trim().is_empty() || input.question.trim().is_empty() || input.options.len() < 2
        {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                "每个问题需要 id、question 和至少两个 options",
            ));
        }
        if input
            .options
            .iter()
            .any(|option| option.label == "Other (type your own)")
        {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                "Other (type your own) 是保留选项标签",
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
                "preview 仅支持单选问题（multi=false）：多选没有聚焦项可并排预览",
            ));
        }
        if input
            .recommended
            .is_some_and(|index| index as usize >= input.options.len())
        {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                "recommended 必须指向现有 option",
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
            error.message.contains("preview 仅支持单选问题"),
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
                .is_some_and(|text| text.contains("仅单选问题")),
            "{option}"
        );
        assert!(AskTool.description().contains("preview"));
    }
}
