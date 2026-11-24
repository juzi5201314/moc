use moc_i18n::t;

use super::App;
use super::ENGINES;
use super::EngineMenu;
use super::PermissionMenu;
use super::initial_fusion_draft;
use crate::modal::Modal;
use crate::modal::SessionScope;
use moc_protocol::ModelInfo;
use moc_protocol::SessionEngine;
use ratatui::layout::Rect;

impl App {
    /// 打开普通会话模型选择器。
    pub fn open_model_picker(&mut self) {
        let current = self
            .session
            .as_ref()
            .map_or_else(String::new, |session| session.model.clone());
        self.open_model_picker_for(current, &[], crate::modal::ModelTarget::Session);
        if self.read_only
            && let Some(Modal::Model { picker, .. }) = self.modal.as_mut()
        {
            picker.error = Some(t!("tui.status.read_only").into_owned());
        }
    }

    /// 打开 `/config` 的模型角色选择器：与 `Ctrl+P` 共用候选、搜索与思考等级。
    ///
    /// `specials` 是探索模型的继承选项，排在模型前面；
    /// 调用方必须已经确认 `models` 非空，否则这个字段应当退回文本编辑。
    pub fn open_config_model_picker(
        &mut self,
        path: Vec<String>,
        label: String,
        current: String,
        specials: &[(&str, String)],
    ) {
        self.open_model_picker_for(
            current,
            specials,
            crate::modal::ModelTarget::Config { path, label },
        );
    }

    pub(crate) fn open_model_picker_for(
        &mut self,
        current: String,
        specials: &[(&str, String)],
        target: crate::modal::ModelTarget,
    ) {
        if self.models.is_empty() {
            self.set_notice(t!("tui.event.no_models_hint").into_owned());
            return;
        }
        // 配置字段不列组合入口；会话与新会话模型都可以选 Fusion。
        let fusion_entry = matches!(
            target,
            crate::modal::ModelTarget::Session | crate::modal::ModelTarget::PlanExecution
        );
        let mut entries = Self::picker_entries(&self.models, specials, fusion_entry);
        let models_start = specials.len() + usize::from(fusion_entry);
        entries[models_start..].sort_by(|a, b| {
            let rank = |entry: &crate::modal::ModelEntry| {
                self.recent_models
                    .rank(&entry.selector)
                    .unwrap_or(usize::MAX)
            };
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.provider().cmp(b.provider()))
                .then_with(|| a.name().cmp(b.name()))
                .then_with(|| a.selector.cmp(&b.selector))
        });

        // 当前值本身是规范 Fusion selector 时不能 rsplit：末尾的等级分隔符在
        // sidekick 侧，整体应高亮 Fusion 特殊项而不是某个模型。
        let current_id = if crate::modal::parse_fusion(&current).is_some() {
            crate::modal::FUSION_ENTRY_SELECTOR
        } else {
            current
                .rsplit_once(':')
                .map_or(current.as_str(), |(id, _)| id)
        };
        let selected = entries
            .iter()
            .position(|entry| entry.selector == current_id)
            .unwrap_or(0);
        let mut level = crate::modal::LevelState::default();
        // 当前值已经写明等级、且该条目仍支持它时沿用；否则用高亮模型的最高等级。
        if crate::modal::parse_fusion(&current).is_none()
            && let Some((_, level_text)) = current.rsplit_once(':')
            && let Some(entry) = entries.get(selected)
            && entry.levels.iter().any(|candidate| candidate == level_text)
        {
            level.manual = Some(level_text.to_owned());
        }
        let picker = crate::modal::PickerState {
            selected,
            ..Default::default()
        };
        // Session 目标有 Fusion 特殊项：草稿沿用当前组合或配置默认。
        let detail = crate::modal::DetailState {
            fusion: if fusion_entry {
                initial_fusion_draft(self, &current)
            } else {
                Default::default()
            },
            ..Default::default()
        };
        self.set_modal(Some(Modal::Model {
            entries,
            current,
            picker,
            level,
            detail,
            target,
        }));
    }

    /// 选择器候选：Fusion 特殊项与角色伪值排在模型前面，
    /// 模型带上各自的思考等级与上下文窗口。
    pub(super) fn picker_entries(
        models: &[ModelInfo],
        specials: &[(&str, String)],
        fusion_entry: bool,
    ) -> Vec<crate::modal::ModelEntry> {
        let mut entries = Vec::with_capacity(models.len() + specials.len() + 1);
        if fusion_entry {
            entries.push(crate::modal::ModelEntry {
                selector: crate::modal::FUSION_ENTRY_SELECTOR.to_owned(),
                label: "Fusion".to_owned(),
                display_name: t!("tui.picker.fusion_name").into_owned(),
                levels: Vec::new(),
                context_window: None,
                max_output_tokens: None,
                inputs: Vec::new(),
                pricing: None,
                fast_mode: false,
            });
        }
        for (selector, label) in specials {
            entries.push(crate::modal::ModelEntry {
                selector: (*selector).to_owned(),
                label: label.clone(),
                display_name: String::new(),
                levels: Vec::new(),
                context_window: None,
                max_output_tokens: None,
                inputs: Vec::new(),
                pricing: None,
                fast_mode: false,
            });
        }
        entries.extend(models.iter().map(|model| crate::modal::ModelEntry {
            selector: model.id.clone(),
            label: model.id.clone(),
            display_name: model.display_name.clone(),
            levels: model.thinking_levels.clone().unwrap_or_else(|| {
                crate::modal::DEFAULT_LEVELS
                    .iter()
                    .map(|level| (*level).to_owned())
                    .collect()
            }),
            context_window: model.context_window,
            max_output_tokens: model.max_output_tokens,
            inputs: model.inputs.clone(),
            pricing: model.pricing,
            fast_mode: model.fast_mode,
        }));
        entries
    }

    /// 打开就地引擎菜单：高亮当前引擎，并让出可能叠在输入框上方的补全菜单。
    /// 会话跑过第一条提示后引擎锁定，菜单不再提供。
    pub fn open_engine_menu(&mut self) {
        let Some(session) = self.session.as_ref() else {
            self.set_notice(t!("tui.common.no_session").into_owned());
            return;
        };
        if session.engine_locked {
            self.set_notice(t!("tui.event.engine_locked_notice").into_owned());
            return;
        }

        self.completion = None;
        self.permission_menu = None;
        let engines: Vec<SessionEngine> = self.engines().collect();
        let current = self.session.as_ref().map(|session| session.engine);
        self.engine_menu = Some(EngineMenu {
            selected: current
                .and_then(|engine| engines.iter().position(|candidate| *candidate == engine))
                .unwrap_or(0),
            items: engines,
            area: Rect::default(),
        });
    }

    /// 打开就地权限菜单：高亮当前权限模式，候选来自平台探测下发的 `sandbox_modes`。
    pub fn open_permission_menu(&mut self) {
        let Some(permission) = self.session.as_ref().map(|session| session.permission) else {
            self.set_notice(t!("tui.common.no_session").into_owned());
            return;
        };
        if self.sandbox_modes.is_empty() {
            self.set_notice(t!("tui.event.no_perm_modes").into_owned());
            return;
        }
        self.completion = None;
        self.engine_menu = None;
        let modes = self.sandbox_modes.clone();
        self.permission_menu = Some(PermissionMenu {
            selected: modes
                .iter()
                .position(|mode| *mode == permission)
                .unwrap_or(0),
            items: modes,
            area: Rect::default(),
        });
    }

    /// 特性 flag 允许使用的引擎清单（[`ENGINES`] 顺序）。
    pub fn engines(&self) -> impl Iterator<Item = SessionEngine> + '_ {
        ENGINES
            .iter()
            .copied()
            .filter(|engine| self.features.engine_available(*engine))
    }

    /// 打开会话恢复选择器（列表由调用方异步填充后更新）。
    pub fn open_session_picker(&mut self) {
        self.set_modal(Some(Modal::Sessions {
            sessions: Vec::new(),
            cwd: self.startup_cwd.clone(),
            scope: SessionScope::default(),
            picker: crate::modal::PickerState {
                loading: true,
                ..Default::default()
            },
        }));
    }

    /// 打开消息回退选择器；数据由调用方异步加载。
    pub fn open_rewind(&mut self) {
        self.set_modal(Some(Modal::Rewind {
            state: crate::modal::RewindState::default(),
        }));
    }

    /// 记录已生效模型，保存失败时显示反馈。
    pub fn remember_model(&mut self, selector: &str) {
        if let Err(error) = self.recent_models.record(selector) {
            self.set_error_notice(
                t!("tui.event.recent_save_failed_fmt", err = error.to_string()).into_owned(),
            );
        }
    }
}
