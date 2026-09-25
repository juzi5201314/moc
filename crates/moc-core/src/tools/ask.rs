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
}

pub struct AskTool;

impl Tool for AskTool {
    fn name(&self) -> &'static str {
        "ask"
    }
    fn description(&self) -> &'static str {
        "向用户询问一个或多个决策问题；只在用户答案会实质影响实现时调用。"
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
                                    "label":{"type":"string"},
                                    "description":{"type":"string"}
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
            let args: AskArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            if args.questions.is_empty() {
                return Err(ToolError::new("E_BAD_PAYLOAD", "questions 不能为空"));
            }
            let mut questions = Vec::with_capacity(args.questions.len());
            for input in args.questions {
                if input.id.trim().is_empty()
                    || input.question.trim().is_empty()
                    || input.options.len() < 2
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
                    multi: input.multi.unwrap_or(false),
                    recommended: input.recommended,
                    options: input
                        .options
                        .into_iter()
                        .map(|option| AskOption {
                            label: option.label,
                            description: option.description,
                        })
                        .collect(),
                });
            }
            let request_id = crate::ids::entry_id();
            let params = AskParams {
                session_id: ctx.session_id.clone(),
                turn_id: ctx.turn_id.clone(),
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
