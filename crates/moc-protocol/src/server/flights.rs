use super::BtwClaim;
use super::BtwFlight;
use super::PromptClaim;
use super::SKILLS_FLIGHT;
use super::ServerDispatch;
use crate::connection::value_from;
use crate::error::RpcError;
use crate::message::Request;
use crate::method::Method;
use crate::methods::BtwCancelResult;
use crate::methods::ClientInfo;
use crate::methods::PromptParams;
use crate::methods::SessionBash;
use crate::methods::SessionBtw;
use crate::methods::SessionCancelResult;
use crate::methods::SessionCompact;
use crate::methods::SessionNew;
use crate::methods::SessionPrompt;
use crate::methods::SessionRetry;
use crate::methods::SessionSteer;
use crate::methods::SkillsCompact;
use crate::service::AgentService;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::PoisonError;
use tokio_util::sync::CancellationToken;

impl<S: AgentService> ServerDispatch<S> {
    pub(super) fn prompts(&self) -> std::sync::MutexGuard<'_, HashMap<String, CancellationToken>> {
        self.prompts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn btw_flights(&self) -> std::sync::MutexGuard<'_, HashMap<String, BtwFlight>> {
        self.btw_flights
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// 在读循环里为 `session/prompt` / `session/retry` 占位提示注册表。
    ///
    /// 只从 `params` 里读 `session_id` 一个字段，不解析整个 DTO——参数里可能带着
    /// 数 MiB 的图片，读循环不该为它做一次完整反序列化。
    ///
    /// 重试必须和提示一样占位：注册表同时是 `session/cancel` 的落点，紧跟在
    /// `session/retry` 后面到达的取消报文只能靠这里的登记才找得到令牌。
    pub(super) fn claim_prompt(
        &self,
        request: &Request,
        cancel: &CancellationToken,
    ) -> PromptClaim {
        let method = request.method.as_ref();
        let skills = method == <SkillsCompact as Method>::NAME;
        let starts = skills
            || method == <SessionPrompt as Method>::NAME
            || method == <SessionRetry as Method>::NAME
            || method == <SessionBash as Method>::NAME
            || method == <SessionCompact as Method>::NAME;
        let mut prompts = self.prompts();
        if prompts.contains_key(SKILLS_FLIGHT)
            && (starts
                || method == <SessionSteer as Method>::NAME
                || method == <SessionBtw as Method>::NAME
                || method == <SessionNew as Method>::NAME)
        {
            return PromptClaim::Busy;
        }
        if !starts {
            return PromptClaim::NotPrompt;
        }
        let session_id = request
            .params
            .as_ref()
            .and_then(|params| params.get("session_id"))
            .and_then(Value::as_str);
        if skills {
            if !prompts.is_empty() {
                return PromptClaim::Busy;
            }
            prompts.insert(SKILLS_FLIGHT.to_owned(), cancel.clone());
            if let Some(id) = session_id {
                prompts.insert(id.to_owned(), cancel.clone());
            }
            return PromptClaim::Skills(session_id.unwrap_or(SKILLS_FLIGHT).to_owned());
        }
        let Some(session_id) = session_id else {
            return PromptClaim::NotPrompt;
        };
        if prompts.contains_key(session_id) {
            return PromptClaim::Busy;
        }
        prompts.insert(session_id.to_owned(), cancel.clone());
        PromptClaim::Claimed(session_id.to_owned())
    }

    /// 在读循环里为 `session/btw` 占位旁路注册表。
    ///
    /// 同一连接、同一会话同时只允许一条旁路；`request_id` 是前端生成的稳定身份，
    /// 定向取消据此匹配。占位与 `session/prompt` 完全分离：主提示在飞不影响旁路，
    /// 旁路在飞也不占用提示注册表。
    pub(super) fn claim_btw(&self, request: &Request, cancel: &CancellationToken) -> BtwClaim {
        if request.method.as_ref() != <SessionBtw as Method>::NAME {
            return BtwClaim::NotBtw;
        }
        let Some(session_id) = request
            .params
            .as_ref()
            .and_then(|params| params.get("session_id"))
            .and_then(Value::as_str)
        else {
            return BtwClaim::NotBtw;
        };
        let Some(request_id) = request
            .params
            .as_ref()
            .and_then(|params| params.get("request_id"))
            .and_then(Value::as_u64)
        else {
            return BtwClaim::NotBtw;
        };
        let mut flights = self.btw_flights();
        if flights.contains_key(session_id) {
            return BtwClaim::Busy;
        }
        flights.insert(
            session_id.to_owned(),
            BtwFlight {
                request_id,
                cancel: cancel.clone(),
            },
        );
        BtwClaim::Claimed {
            session_id: session_id.to_owned(),
            request_id,
        }
    }

    /// 定向取消一条旁路；身份不匹配时是成功的空操作，不影响任何主提示。
    pub(super) fn cancel_btw(&self, session_id: &str, request_id: u64) -> BtwCancelResult {
        let flights = self.btw_flights();
        match flights.get(session_id) {
            Some(flight) if flight.request_id == request_id => {
                flight.cancel.cancel();
                BtwCancelResult { cancelled: true }
            }
            _ => BtwCancelResult { cancelled: false },
        }
    }

    /// 跑一次提示：进 core；结束后由 `dispatch` 的占位守卫释放注册表。
    pub(super) async fn prompt(
        self: &Arc<Self>,
        client: ClientInfo,
        params: PromptParams,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        let result = self
            .server
            .service
            .session_prompt(self.context(client, cancel), params)
            .await;
        value_from(result?)
    }

    /// 取消该会话在飞的提示；没有进行中的提示时是成功的空操作。
    pub(super) fn cancel_prompt(&self, session_id: &str) -> SessionCancelResult {
        match self.prompts().get(session_id) {
            Some(token) => {
                token.cancel();
                SessionCancelResult { cancelled: true }
            }
            None => SessionCancelResult { cancelled: false },
        }
    }
}
