//! PTC 的 `state.*`：显式保存会话级 JSON 状态，配额按当前状态计算。
use super::wire::{Operation, ScriptError};
use crate::{
    session::{
        entries::{EntryPayload, PtcStateChange},
        state::ptc_value_bytes,
    },
    tools::ToolCtx,
};
use serde_json::{Value, json};

pub(super) fn error(code: &str, message: impl ToString) -> ScriptError {
    ScriptError {
        code: code.into(),
        message: message.to_string(),
        result: None,
    }
}

pub(super) async fn execute(ctx: &ToolCtx, operation: Operation) -> Result<Value, ScriptError> {
    let key = match &operation {
        Operation::Store { key, .. } | Operation::Load { key } | Operation::Remove { key } => {
            Some(key)
        }
        _ => None,
    };
    if key.is_some_and(|key| key.is_empty() || key.len() > 128) {
        return Err(error("E_STATE_KEY", "key must be 1..128 UTF-8 bytes"));
    }
    let (change, result) = {
        let state = ctx.state.lock().await;
        match operation {
            Operation::Load { key } => {
                return Ok(match state.ptc_state.get(&key) {
                    Some(value) => json!({"found":true,"value":value.as_ref()}),
                    None => json!({"found":false}),
                });
            }
            Operation::Keys => return Ok(json!(state.ptc_state.keys().collect::<Vec<_>>())),
            Operation::Store { key, value } => {
                let previous = state
                    .ptc_state
                    .get(&key)
                    .map_or(0, |old| ptc_value_bytes(&key, old));
                let total = state.ptc_state_bytes - previous + ptc_value_bytes(&key, &value);
                if total > ctx.config.tools.ptc.max_state_bytes && total > state.ptc_state_bytes {
                    return Err(error(
                        "E_STATE_LIMIT",
                        "named state exceeds the total limit",
                    ));
                }
                (PtcStateChange::Put { key, value }, Value::Null)
            }
            Operation::Remove { key } => {
                if !state.ptc_state.contains_key(&key) {
                    return Ok(Value::Bool(false));
                }
                (PtcStateChange::Delete { key }, Value::Bool(true))
            }
            _ => return Err(error("E_PTC_PROTOCOL", "not a state operation")),
        }
    };
    ctx.sessions
        .append(
            &ctx.session_id,
            EntryPayload::PtcState {
                parent_tool_call_id: ctx.tool_call_id.clone(),
                change,
            },
        )
        .await
        .map_err(|err| error("E_STATE_WRITE", err))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_value_size_counts_replacement_without_mutating_projection() {
        let first = serde_json::json!({"n": 1});
        let second = serde_json::json!({"n": 2, "text": "x"});
        assert!(ptc_value_bytes("x", &second) > ptc_value_bytes("x", &first));
    }
}
