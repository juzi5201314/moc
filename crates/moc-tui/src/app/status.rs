use super::App;
use super::engine_label;
use super::model_cost;
use super::model_selector_label;
use crate::transcript::Block;
use moc_protocol::SessionUsageResult;
use moc_protocol::Usage;

impl App {
    /// 计费与上下文窗口对应的模型 id：普通会话是 `provider/model`；
    /// Fusion 会话真实发请求的是 lead 侧，取其模型并去等级。
    pub(super) fn billed_model_id(&self) -> Option<&str> {
        let model = self.session.as_ref()?.model.as_str();
        let lead = crate::modal::parse_fusion(model).map_or(model, |(lead, _)| lead);
        lead.split(':').next()
    }

    /// 当前模型的定价（从模型目录按 selector 匹配）。
    pub fn current_pricing(&self) -> Option<&moc_protocol::methods::ModelPricing> {
        let base = self.billed_model_id()?;
        self.models
            .iter()
            .find(|candidate| candidate.id == base)
            .and_then(|info| info.pricing.as_ref())
    }

    /// 当前模型的上下文窗口。
    pub fn context_window(&self) -> Option<u64> {
        let base = self.billed_model_id()?;
        self.models
            .iter()
            .find(|candidate| candidate.id == base)
            .and_then(|info| info.context_window)
    }

    /// 当前上下文占用：请求实测值优先，未请求时使用 core 估算值。
    pub fn context_used(&self) -> u64 {
        if self.usage.input_tokens > 0 {
            self.usage.input_tokens
        } else {
            self.context_usage
                .as_ref()
                .map_or(0, |usage| usage.total_tokens)
        }
    }

    /// 应用一次请求级 usage 刷新：费用累计与上下文占用读它。
    /// 成本只在快照（`apply_session_usage`）与本函数累计之间换算。
    pub fn apply_usage(&mut self, usage: Usage) {
        let previous = &self.usage;
        let delta_in = usage.input_tokens.saturating_sub(previous.input_tokens);
        let delta_out = usage.output_tokens.saturating_sub(previous.output_tokens);
        let delta_cached = usage
            .cached_input_tokens
            .unwrap_or(0)
            .saturating_sub(previous.cached_input_tokens.unwrap_or(0));
        if (delta_in > 0 || delta_out > 0)
            && let Some(pricing) = self.current_pricing()
        {
            self.cost_usd += model_cost(delta_in, delta_cached, delta_out, pricing);
        }
        self.usage = usage;
        // reasoning_tokens 属于本次响应：没有思考块的响应（上游可能报 0）
        // 不能按「最近一块」写回，否则会把上一响应的 token 数抹掉。
        if self.thinking_has_block
            && let Some(reasoning_tokens) = usage.reasoning_tokens
            && reasoning_tokens > 0
            && let Some(entry) = self
                .blocks
                .iter_mut()
                .rev()
                .find(|entry| matches!(entry.block, Block::Thinking { .. }))
            && let Block::Thinking {
                reasoning_tokens: stored,
                ..
            } = &mut entry.block
            && *stored != Some(reasoning_tokens)
        {
            *stored = Some(reasoning_tokens);
            entry.touch();
        }
    }

    /// 发出一次会话用量刷新，返回请求序号；回执带回序号以丢弃迟到的旧结果。
    pub fn begin_usage_request(&mut self) -> u64 {
        self.usage_request = self.usage_request.wrapping_add(1);
        self.usage_request
    }

    /// 应用一次会话用量回执：比已应用结果更旧的（乱序或跨会话迟到）直接丢弃。
    /// 成本按快照整体重算——每一行（含 sidekick/子代理）按各自模型定价折算，
    /// 无定价信息的行按 0 计；在飞请求仍由 `apply_usage` 继续累加，
    /// 下一次快照到来时再整体覆盖。
    pub fn apply_session_usage(&mut self, request: u64, usage: SessionUsageResult) {
        if request <= self.usage_applied {
            return;
        }
        self.usage_applied = request;
        self.cost_usd = usage
            .models
            .iter()
            .map(|summary| {
                let base = summary.model.split(':').next().unwrap_or_default();
                self.models
                    .iter()
                    .find(|model| model.id == base)
                    .and_then(|info| info.pricing.as_ref())
                    .map_or(0.0, |pricing| {
                        model_cost(
                            summary.input_tokens,
                            summary.cached_input_tokens.unwrap_or(0),
                            summary.output_tokens,
                            pricing,
                        )
                    })
            })
            .sum();
        self.session_usage = Some(usage);
    }

    /// 会话累计输入／输出 token；统计还没回来时按 0 处理。
    pub fn session_tokens(&self) -> (u64, u64) {
        self.session_usage.as_ref().map_or((0, 0), |usage| {
            (usage.total.input_tokens, usage.total.output_tokens)
        })
    }

    /// 会话累计输出速率（tok/s），与 `/usage` 同源：输出 token 总量除以生成阶段
    /// 耗时（首 token 等待与工具执行不计入）。没有可拆分数据（旧会话）时退回
    /// 整段请求用时，与升级前的口径一致。
    pub fn session_tokens_per_second(&self) -> f64 {
        let Some(usage) = &self.session_usage else {
            return 0.0;
        };
        let total = &usage.total;
        if total.output_tokens == 0 {
            return 0.0;
        }
        let ms = if total.gen_ms > 0 {
            total.gen_ms
        } else if total.ttft_calls == 0 {
            total.active_ms
        } else {
            return 0.0;
        };
        if ms == 0 {
            0.0
        } else {
            total.output_tokens as f64 * 1000.0 / ms as f64
        }
    }

    /// todo 完成进度 `(done, total)`。
    pub fn todo_progress(&self) -> (usize, usize) {
        let total: usize = self.todos.iter().map(|phase| phase.items.len()).sum();
        let done = self
            .todos
            .iter()
            .flat_map(|phase| phase.items.iter())
            .filter(|item| item.status == moc_protocol::methods::TodoStatus::Completed)
            .count();
        (done, total)
    }

    /// 图例与状态栏展示的状态标签：会话引擎 + plan 叠加态 + 本次提示的临时模式 + 非默认自主程度。
    pub fn status_label(&self) -> String {
        let Some(session) = self.session.as_ref() else {
            return "idle".to_owned();
        };
        let mut label = engine_label(session.engine).to_owned();
        if session.plan_active {
            label.push_str("+plan");
        }
        if self.transient_mode.is_some() {
            label.push_str("+ask");
        }
        match session.autonomy {
            moc_protocol::SessionAutonomy::Default => {}
            moc_protocol::SessionAutonomy::DontAsk => label.push_str("+dont_ask"),
            moc_protocol::SessionAutonomy::Unattended => label.push_str("+unattended"),
        }
        label
    }

    /// 当前模型展示名；未连接时显示 `idle`。
    pub fn model_display(&self) -> String {
        self.session.as_ref().map_or_else(
            || "idle".to_owned(),
            |session| model_selector_label(&session.model),
        )
    }
}
