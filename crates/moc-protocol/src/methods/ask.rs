//! `session/request_ask` 反向请求：core 向前端询问用户。

use serde::{Deserialize, Serialize};

use crate::method::{Method, ServerRequest};

/// `session/request_ask` 的方法标记。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionRequestAsk;

impl Method for SessionRequestAsk {
    const NAME: &'static str = "session/request_ask";
    type Params = AskParams;
    type Result = AskResult;
}

impl ServerRequest for SessionRequestAsk {}

/// 一次或多次用户提问。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskParams {
    pub session_id: String,
    pub prompt_id: String,
    pub request_id: String,
    pub questions: Vec<AskQuestion>,
}

/// 一个问题及其候选答案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskQuestion {
    pub id: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub multi: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<u32>,
    pub options: Vec<AskOption>,
}

/// 一个用户可选答案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 聚焦该项时展示的预览内容（markdown，等宽框内渲染），用于
    /// ASCII 示意图、代码片段等需要肉眼对比的候选；仅单选问题支持。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// 前端为问题返回的答案集合。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskResult {
    pub answers: Vec<AskAnswer>,
}

/// 一个问题对应的选择或自定义输入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskAnswer {
    pub id: String,
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;

    #[test]
    fn ask_request_and_answer_shapes() {
        assert_eq!(<SessionRequestAsk as Method>::NAME, "session/request_ask");
        let params = AskParams {
            session_id: "ses_1".into(),
            prompt_id: "prompt_2".into(),
            request_id: "ask_3".into(),
            questions: vec![AskQuestion {
                id: "choice".into(),
                question: "选哪个？".into(),
                header: Some("方案".into()),
                multi: false,
                recommended: Some(1),
                options: vec![AskOption {
                    label: "A".into(),
                    description: Some("首选".into()),
                    preview: Some("```\n[ A ]\n```".into()),
                }],
            }],
        };
        assert_eq!(
            serde_json::to_value(&params).expect("参数可序列化"),
            serde_json::json!({
                "session_id": "ses_1", "prompt_id": "prompt_2", "request_id": "ask_3",
                "questions": [{
                    "id": "choice", "question": "选哪个？", "header": "方案",
                    "multi": false, "recommended": 1,
                    "options": [{
                        "label": "A", "description": "首选", "preview": "```\n[ A ]\n```"
                    }]
                }]
            })
        );
        assert_eq!(
            serde_json::to_value(AskOption {
                label: "无预览".into(),
                description: None,
                preview: None,
            })
            .expect("选项可序列化"),
            serde_json::json!({ "label": "无预览" })
        );
        let result = AskResult {
            answers: vec![AskAnswer {
                id: "choice".into(),
                selected: vec!["A".into()],
                custom: None,
            }],
        };
        assert_eq!(
            serde_json::to_value(&result).expect("答案可序列化"),
            serde_json::json!({ "answers": [{ "id": "choice", "selected": ["A"] }] })
        );
    }
}
