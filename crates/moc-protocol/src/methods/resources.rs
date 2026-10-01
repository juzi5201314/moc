//! `resource/complete`：统一内部资源 URI 输入补全。

use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceComplete;

impl Method for ResourceComplete {
    const NAME: &'static str = "resource/complete";
    type Params = ResourceCompleteParams;
    type Result = ResourceCompleteResult;
}

impl ClientRequest for ResourceComplete {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceCompleteParams {
    pub session_id: String,
    pub prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceCandidate {
    pub uri: String,
    pub label: String,
    pub description: String,
    pub kind: String,
    pub append_space: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceCompleteResult {
    pub candidates: Vec<ResourceCandidate>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;

    #[test]
    fn method_name_and_wire_shape_are_stable() {
        assert_eq!(<ResourceComplete as Method>::NAME, "resource/complete");
        let params = serde_json::to_value(ResourceCompleteParams {
            session_id: "ses_1".into(),
            prefix: "local:/".into(),
        })
        .expect("参数可序列化");
        assert_eq!(
            params,
            serde_json::json!({"session_id":"ses_1","prefix":"local:/"})
        );
        assert!(
            serde_json::from_value::<ResourceCompleteParams>(
                serde_json::json!({"session_id":"ses_1","prefix":"local:/","extra":true})
            )
            .is_err()
        );
    }
}
