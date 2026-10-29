//! ask 工具：向前端发反向请求，向用户提问。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::{
    AskOption, AskParams, AskQuestion, AskResult, RpcCallError, RpcErrorCode, SessionRequestAsk,
};
use serde::Deserialize;
use serde_json::Value;

/// 无人应答的兜底等待：到点按「未回答」继续，别让提问在无人值守的终端上
/// 永久阻塞整轮任务（对齐 Grok Build 的 ask_user_question 默认超时）。
const ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

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
    #[serde(default)]
    recommended: Vec<u32>,
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
        "Ask the user decision questions; use only when the answer changes the implementation. Set recommended to 0-based option indexes to mark the model's preferred choices; single-select questions accept at most one index, multi-select questions may include multiple recommended indexes. Options may carry a preview, a monospace frame shown beside the focused option for visual comparison (single-select only)."
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
                            "recommended":{"type":"array","items":{"type":"integer","minimum":0},"uniqueItems":true,"description":"Recommended option indexes, 0-based; single-select questions accept at most one, multi-select questions may include multiple indexes"},
                            "options":{"type":"array","minItems":2,"items":{
                                "type":"object","properties":{
                                    "label":{"type":"string","description":"Display text, 1-5 words"},
                                    "description":{"type":"string","description":"What choosing this option means"},
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
            // Don't Ask 及以上：不打扰用户——有推荐项选推荐项，没有就把决定
            // 交回模型；答复都标明用户没有参与。
            let autonomy = ctx.state.lock().await.session.autonomy;
            if autonomy >= moc_protocol::SessionAutonomy::DontAsk {
                return Ok(ToolOutcome::success(auto_answers(&questions)));
            }
            let request_id = crate::ids::entry_id();
            let params = AskParams {
                session_id: ctx.session_id.clone(),
                prompt_id: ctx.prompt_id.clone(),
                request_id,
                questions: questions.clone(),
            };
            let requester = ctx.request.requester();
            let result = requester.request::<SessionRequestAsk>(params);
            // 用户主动不回答是正常收场而不是前端故障：回明确语义，模型不会
            // 收到错误，也就不会为「提问失败」道歉或重试。
            let result = match tokio::time::timeout(ASK_TIMEOUT, result).await {
                Ok(result) => ask_settled(result)?,
                Err(_elapsed) => {
                    return Ok(ToolOutcome::success(
                        "The user did not answer within the time limit; continue without an answer.",
                    ));
                }
            };
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

/// Don't Ask 自主程度下的自动代答：有推荐项选推荐项并注明自动代答，没有推荐项
/// 就让模型按最佳判断自行决定并在结果里说明。返回给模型的文本一律英文。
fn auto_answers(questions: &[AskQuestion]) -> String {
    let render = |question: &AskQuestion| -> String {
        if question.recommended.is_empty() {
            "The user is unavailable; decide yourself using best judgment and state the choice you made.".into()
        } else {
            let labels = question
                .recommended
                .iter()
                .filter_map(|index| question.options.get(*index as usize))
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{labels} (auto-selected recommended option; the user was not involved)")
        }
    };
    if questions.len() == 1 {
        render(&questions[0])
    } else {
        let mut output = String::from("Auto-answers (the user was not involved):\n");
        for question in questions {
            output.push_str(&format!("{}: {}\n", question.id, render(question)));
        }
        output
    }
}

/// 规范化预览文本：没提供或只有空白都算没有预览，免得前端为空内容切到并排布局；
/// 首尾的空白和空行在这里去掉。
fn preview_text(preview: &Option<String>) -> Option<String> {
    preview
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// 反向请求的收场映射：用户取消（本地取消或前端回 `-32001`）按「未回答」
/// 成功收场——空答案在 `run` 里输出统一的未回答文本；其余错误才是前端故障。
fn ask_settled(result: Result<AskResult, RpcCallError>) -> Result<AskResult, ToolError> {
    match result {
        Ok(result) => Ok(result),
        Err(RpcCallError::Cancelled) => Ok(AskResult {
            answers: Vec::new(),
        }),
        Err(RpcCallError::Rpc(error)) if error.code == RpcErrorCode::RequestCancelled.code() => {
            Ok(AskResult {
                answers: Vec::new(),
            })
        }
        Err(error) => Err(ToolError::new(
            "E_ASK_UNAVAILABLE",
            format!("The frontend could not handle the ask request: {error}"),
        )),
    }
}

/// 解析并校验 ask 参数：逐题检查必填项、保留标签、「预览只用于单选」与推荐
/// 下标的限制，再把选项转成协议结构。
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
            .iter()
            .any(|&index| index as usize >= input.options.len())
        {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                "recommended must point to existing options",
            ));
        }
        // 推荐下标不得重复：重复项会让前端的默认选中出现重复标签。
        let mut seen = vec![false; input.options.len()];
        for &index in &input.recommended {
            let slot = &mut seen[index as usize];
            if *slot {
                return Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    "recommended must not repeat an option index",
                ));
            }
            *slot = true;
        }
        if !multi && input.recommended.len() > 1 {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                "single-select questions accept at most one recommended index",
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

    /// 用户取消（本地取消与前端回 `-32001`）按未回答收场，其余错误才是前端故障。
    #[test]
    fn cancellation_settles_as_unanswered_and_other_errors_fail() {
        let cancelled = ask_settled(Err(RpcCallError::Cancelled)).expect("本地取消不算失败");
        assert!(cancelled.answers.is_empty());

        let dismissed = ask_settled(Err(RpcCallError::Rpc(moc_protocol::RpcError::of(
            RpcErrorCode::RequestCancelled,
        ))))
        .expect("前端回 -32001 同样按未回答收场");
        assert!(dismissed.answers.is_empty());

        let error = ask_settled(Err(RpcCallError::Closed)).expect_err("连接关闭才是故障");
        assert_eq!(error.code, "E_ASK_UNAVAILABLE");
    }

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

    /// 多选问题的多个推荐下标会原样透传给协议层。
    #[test]
    fn multiple_recommendations_survive_normalization() {
        let questions = normalize_questions(json!({
            "questions": [{
                "id": "features",
                "question": "开启哪些特性？",
                "multi": true,
                "recommended": [0, 2],
                "options": [
                    {"label": "缓存"},
                    {"label": "日志"},
                    {"label": "监控"}
                ]
            }]
        }))
        .expect("载荷可归一化");

        assert_eq!(questions[0].recommended, vec![0, 2]);
    }

    /// 推荐下标越界时拒绝整个 ask 载荷。
    #[test]
    fn recommendations_reject_out_of_range_index() {
        let error = normalize_questions(json!({
            "questions": [{
                "id": "choice",
                "question": "选哪个？",
                "multi": true,
                "recommended": [0, 2],
                "options": [{"label": "A"}, {"label": "B"}]
            }]
        }))
        .expect_err("越界推荐下标应被拒绝");

        assert_eq!(error.code, "E_BAD_PAYLOAD");
        assert!(
            error
                .message
                .contains("recommended must point to existing options")
        );
    }

    /// 单选问题最多一个推荐下标：多个推荐会让前端预选多个答案并整包提交。
    #[test]
    fn single_select_rejects_multiple_recommendations() {
        let error = normalize_questions(json!({
            "questions": [{
                "id": "choice",
                "question": "选哪个？",
                "recommended": [0, 1],
                "options": [{"label": "A"}, {"label": "B"}]
            }]
        }))
        .expect_err("单选问题多个推荐下标应被拒绝");

        assert_eq!(error.code, "E_BAD_PAYLOAD");
        assert!(
            error
                .message
                .contains("single-select questions accept at most one"),
            "{}",
            error.message
        );
    }

    /// 重复的推荐下标会让默认选中出现重复标签，整包拒绝。
    #[test]
    fn recommendations_reject_duplicate_indexes() {
        let error = normalize_questions(json!({
            "questions": [{
                "id": "features",
                "question": "开启哪些特性？",
                "multi": true,
                "recommended": [1, 0, 1],
                "options": [{"label": "A"}, {"label": "B"}]
            }]
        }))
        .expect_err("重复推荐下标应被拒绝");

        assert_eq!(error.code, "E_BAD_PAYLOAD");
        assert!(
            error.message.contains("recommended must not repeat"),
            "{}",
            error.message
        );
    }

    /// Don't Ask 代答：有推荐项选推荐项并注明用户未参与；无推荐项给英文兜底指令。
    #[test]
    fn auto_answers_pick_recommended_or_delegate_to_model() {
        let questions = vec![
            AskQuestion {
                id: "choice".into(),
                question: "选哪个？".into(),
                header: None,
                multi: false,
                recommended: vec![1],
                options: vec![
                    AskOption {
                        label: "甲".into(),
                        description: None,
                        preview: None,
                    },
                    AskOption {
                        label: "乙".into(),
                        description: None,
                        preview: None,
                    },
                ],
            },
            AskQuestion {
                id: "open".into(),
                question: "怎么做？".into(),
                header: None,
                multi: false,
                recommended: Vec::new(),
                options: vec![
                    AskOption {
                        label: "A".into(),
                        description: None,
                        preview: None,
                    },
                    AskOption {
                        label: "B".into(),
                        description: None,
                        preview: None,
                    },
                ],
            },
        ];
        let single = auto_answers(&questions[..1]);
        assert!(single.contains("乙"), "{single}");
        assert!(single.contains("auto-selected"), "{single}");
        let both = auto_answers(&questions);
        assert!(both.contains("choice: 乙"), "{both}");
        assert!(both.contains("open: The user is unavailable"), "{both}");
        assert!(both.contains("decide yourself"), "{both}");
    }

    /// 工具 schema 暴露推荐下标数组、预览字段及其格式说明。
    #[test]
    fn schema_exposes_recommendations_and_preview_fields() {
        let schema = AskTool.def().parameters;
        let question = &schema["properties"]["questions"]["items"];
        let recommended = &question["properties"]["recommended"];
        assert_eq!(recommended["type"], json!("array"));
        assert_eq!(recommended["items"]["type"], json!("integer"));
        assert_eq!(recommended["uniqueItems"], json!(true));
        assert!(
            recommended["description"]
                .as_str()
                .is_some_and(|text| text.contains("0-based")
                    && text.contains("single-select")
                    && text.contains("multiple indexes"))
        );
        let option = &question["properties"]["options"]["items"];
        assert_eq!(option["properties"]["preview"]["type"], json!("string"));
        assert!(
            option["properties"]["preview"]["description"]
                .as_str()
                .is_some_and(|text| text.contains("Supported only for single-select questions")),
            "{option}"
        );
        assert!(AskTool.description().contains("recommended"));
        assert!(AskTool.description().contains("preview"));
    }
}
