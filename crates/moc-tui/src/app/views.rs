use rust_i18n::t;

use super::App;
use crate::transcript::Block;
use crate::transcript::DetailLevel;
use crate::transcript::TableChartState;
use moc_protocol::methods::TodoPhase;

impl App {
    /// 切换一张表格的图表形态：已展开则还原成表格，否则就地分析后展开。
    ///
    /// 分析与渲染共用同一次 markdown 解析（`render_markdown_with` 交出的纯文本
    /// 快照），因此「屏幕上的表格」与「图表吃到的数据」不会出现两套口径。
    /// 只处理助手正文的**顶层表格**：引用块 / 列表内嵌表格没有独立命中区。
    pub fn toggle_table_chart(&mut self, entry: usize, table: usize) {
        let Some(block) = self.blocks.get(entry) else {
            return;
        };
        if block.table_chart(table).is_some() {
            if let Some(block) = self.blocks.get_mut(entry) {
                block.set_table_chart(table, None);
            }
            return;
        }
        let theme = self.theme;
        let width = usize::from(self.scrollback_area.width)
            .saturating_sub(crate::render::PROSE_INDENT)
            .max(8);
        let prepared: Result<moc_core::chart::ChartSpec, String> = {
            let Some(entry_block) = self.blocks.get(entry) else {
                return;
            };
            let Block::Assistant { text } = &entry_block.block else {
                return;
            };
            let rendered = crate::render::markdown::render_markdown_with(
                text,
                width,
                theme,
                crate::render::markdown::RenderViews {
                    charts: &[],
                    graphics: None,
                    streaming: false,
                    ..crate::render::markdown::RenderViews::default()
                },
            );
            match rendered.raw.get(table) {
                None => Err(t!("tui.err.table_changed").into_owned()),
                Some(raw) => {
                    let analysis = moc_core::chart::analyze(raw);
                    moc_core::chart::plan(&analysis)
                        .map(|plan| moc_core::chart::build(&analysis, &plan))
                        .ok_or_else(|| t!("tui.err.table_not_chartable").into_owned())
                }
            }
        };
        let spec = match prepared {
            Ok(spec) => spec,
            Err(text) => {
                self.set_notice(text);
                return;
            }
        };
        // 编号必须与内联图片共用同一个空间：kitty 里用已存在的 `i=` 重传会
        // 释放旧图像、清掉它的全部放置，两套自增器各自从 1 起必然互相覆盖。
        let image_id = self.images.allocate_image_id();
        let immediate_layout = self.graphics.and_then(|support| {
            let cols = u16::try_from(width).unwrap_or(u16::MAX);
            crate::render::chart::layout(&spec, cols, support.cell).map(|l| (support.proto, l))
        });
        let rendering = immediate_layout
            .as_ref()
            .map(|(_, l)| (l.px, std::time::Instant::now()));
        if let Some((proto, chart_layout)) = immediate_layout
            && let Err(error) = crate::render::graphics::spawn_chart_encode(
                self.events.clone(),
                table,
                image_id,
                spec.clone(),
                chart_layout,
                proto,
                theme,
            )
        {
            self.set_notice(t!("tui.err.chart_start_fmt", err = error).into_owned());
            return;
        }
        if let Some(block) = self.blocks.get_mut(entry) {
            block.set_table_chart(
                table,
                Some(TableChartState {
                    spec,
                    payload: None,
                    px: (0, 0),
                    image_id,
                    transmitted: false,
                    rendering,
                }),
            );
        }
    }

    /// 回填后台完成的图表渲染载荷；若该图表已被收起、替换或尺寸已过期则静默丢弃。
    pub fn complete_chart_encode(
        &mut self,
        table: usize,
        image_id: u32,
        px: (u32, u32),
        payload: std::sync::Arc<crate::render::graphics::encode::Payload>,
    ) -> bool {
        // 历史分页会前插 blocks；图像 ID 在进程内唯一，不依赖启动时的条目下标。
        let Some(state) = self
            .blocks
            .iter_mut()
            .filter_map(|block| block.table_chart_mut(table))
            .find(|state| state.image_id == image_id)
        else {
            return false;
        };
        if !state
            .rendering
            .is_some_and(|(target_px, _)| target_px == px)
        {
            return false;
        }
        state.payload = Some(payload);
        state.px = px;
        state.transmitted = false;
        state.rendering = None;
        true
    }

    /// 是否仍有正在后台渲染的表格图表（用于驱动事件循环加载动画 tick）。
    pub fn has_rendering_charts(&self) -> bool {
        self.blocks.iter().any(|entry| {
            entry
                .charts
                .iter()
                .flatten()
                .any(TableChartState::is_rendering)
        })
    }

    /// 切换正文末尾参考链接的折叠态：展开与收起共用同一个入口。
    pub fn toggle_links(&mut self, entry: usize) {
        if let Some(block) = self.blocks.get_mut(entry) {
            block.links_expanded = !block.links_expanded;
            block.touch();
        }
    }

    /// 让含指定图片键的工具块失效；返回是否命中。
    ///
    /// 图片从「解码中」变成真像素时，只有含它的块需要重渲染。`view_gen` 是全局
    /// 失效，长会话里会让最近 64 块把所有 markdown 重解析一遍，图片一多就成了
    /// 肉眼可见的卡顿。markdown 图片的键在渲染期才算出，反查不到，由调用方
    /// 回落到全局失效。
    pub fn touch_blocks_with_image(&mut self, key: &crate::render::image::ImageKey) -> bool {
        let mut hit = false;
        for entry in &mut self.blocks {
            let matches = matches!(
                &entry.block,
                Block::Tool(tool) if tool.images.iter().any(|image| image.key == *key)
            );
            if matches {
                entry.touch();
                hit = true;
            }
        }
        hit
    }

    /// 思考块档位循环一档并使视图缓存失效。
    ///
    /// 手动展开的单块也要交还档位控制：否则全局切到隐藏／简略后，
    /// 它仍停在完整档，看起来像快捷键失灵。
    pub fn cycle_thinking_detail(&mut self) {
        self.thinking_detail = self.thinking_detail.next();
        for entry in &mut self.blocks {
            if matches!(entry.block, Block::Thinking { .. }) {
                entry.clear_level_override();
            }
        }
        self.view_gen += 1;
    }

    /// 工具全局只循环隐藏／简略，并收回单块的手动展开。
    pub fn cycle_tool_detail(&mut self) {
        self.tool_detail = match self.tool_detail {
            DetailLevel::Hidden => DetailLevel::Brief,
            DetailLevel::Brief | DetailLevel::Full => DetailLevel::Hidden,
        };
        for entry in &mut self.blocks {
            if matches!(
                entry.block,
                Block::Tool(_) | Block::AgentResult { .. } | Block::JobNotice { .. }
            ) {
                entry.clear_level_override();
            }
        }
        self.view_gen += 1;
    }

    /// 块的层档位：思考块看 `thinking_detail`、工具和结果卡片看 `tool_detail`，
    /// 其余块（IRC 卡片等）固定简略预览。
    pub fn block_level(&self, block: &Block) -> DetailLevel {
        match block {
            Block::Thinking { .. } => self.thinking_detail,
            Block::Tool(_) | Block::AgentResult { .. } | Block::JobNotice { .. } => {
                self.tool_detail
            }
            _ => DetailLevel::Brief,
        }
    }

    /// 子代理侧栏是否可见：无代理恒为否；自动模式下有任一运行中的代理才显示。
    pub fn subagent_sidebar_visible(&self) -> bool {
        !self.subagents.is_empty()
            && self
                .subagent_sidebar
                .unwrap_or_else(|| self.subagents.values().any(|view| view.running))
    }

    /// 名册或运行态变化后同步侧栏显隐：running 从无到有时清掉手动覆盖
    /// （回到自动模式），并记下这次的 running 状态。
    pub fn sync_subagent_sidebar(&mut self) {
        let running = self.subagents.values().any(|view| view.running);
        if running && !self.subagent_running_seen {
            self.subagent_sidebar = None;
        }
        self.subagent_running_seen = running;
    }

    /// Todo 快照更新：只维护 HUD 数据；转写里的任务树由每次 todo 调用在原位输出。
    pub fn update_todos(&mut self, phases: Vec<TodoPhase>) {
        self.todos = phases;
        if self.todos.is_empty() {
            self.todo_hud.reset();
        }
    }
}
