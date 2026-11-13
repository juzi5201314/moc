use super::Group;
use super::GroupPath;
use super::Row;
use super::RowKind;
use super::SettingsPanel;
use super::TABS;
use super::controls;
use super::get;
use super::matches_filter;
use super::server_address;
use super::tool_subgroup_title;
use super::tool_title;
use moc_protocol::ConfigField;
use serde_json::Value;

impl SettingsPanel {
    pub fn rebuild(&mut self) {
        let Some(data) = &self.data else {
            return;
        };
        // `effective` 是服务端按完整配置优先级计算出的结果；编辑期间只由
        // set/remove_value 修改对应路径，不能再用 document 整体覆盖它。
        self.effective = if data.effective.is_null()
            || (data
                .effective
                .as_object()
                .is_some_and(|value| value.is_empty())
                && data
                    .inherited
                    .as_object()
                    .is_some_and(|value| !value.is_empty()))
        {
            // 兼容旧回执未提供 effective 的场景；正常服务端回执始终优先。
            data.inherited.clone()
        } else {
            data.effective.clone()
        };
        moc_core::config::catalog::complete_view(&mut self.effective);
        // 服务端目录包含插件声明的字段（即便插件尚未加载成功也能修复配置）；
        // 不能用本地值推断结果覆盖它，否则动态字段会丢失且描述/类型会漂移。
        // 核心目录仍作增量补充，以便本次编辑刚新增的 provider/model 立即可见。
        self.fields = data.fields.clone();
        for field in moc_core::config::catalog::fields(&self.effective) {
            if !self
                .fields
                .iter()
                .any(|existing| existing.path == field.path)
            {
                self.fields.push(field);
            }
        }
        self.fields.extend(crate::config::fields());
        self.controls = self.fields.iter().map(controls::control).collect();
        self.effective["tui"] = self.tui.clone();
        self.rebuild_rows();
    }

    /// 行集重建后收紧选中项；被钳动过才重新显露，普通重绘不拉回手动滚动的位置。
    pub(super) fn clamp_selected(&mut self) {
        let selected = self.selected.min(self.rows.len().saturating_sub(1));
        if selected != self.selected {
            self.scroll.reveal_selection = true;
        }
        self.selected = selected;
        self.offset = self.offset.min(self.rows.len().saturating_sub(1));
    }

    pub(super) fn rebuild_rows(&mut self) {
        if self.provider_page() {
            self.sync_provider();
            self.rows = self.provider_rows();
            self.clamp_selected();
            return;
        }
        let filter = self.filter.to_lowercase();
        let searching = !filter.is_empty();
        let mut rows = Vec::new();
        let mut open: Vec<String> = Vec::new();
        for (index, field) in self.fields.iter().enumerate() {
            if searching {
                if !matches_filter(field, &filter) {
                    continue;
                }
            } else if !self.matches_tab(field) {
                continue;
            }
            let chain = self.group_chain(&field.path);
            let common = chain
                .iter()
                .zip(&open)
                .take_while(|(group, key)| group.0 == **key)
                .count();
            for (depth, group) in chain.iter().enumerate().skip(common) {
                let detail = self.group_detail(&group.2);
                rows.push(Row {
                    depth: u8::try_from(depth).unwrap_or(u8::MAX),
                    kind: RowKind::Group(Group {
                        key: group.0.clone(),
                        title: group.1.clone(),
                        detail,
                        grouping: group.2.clone(),
                        path: group.3.clone(),
                    }),
                });
            }
            open = chain.iter().map(|group| group.0.clone()).collect();
            let visible = searching
                || chain
                    .iter()
                    .enumerate()
                    .all(|(depth, group)| self.is_expanded(&group.0, depth));
            if visible {
                rows.push(Row {
                    depth: u8::try_from(chain.len()).unwrap_or(u8::MAX),
                    kind: RowKind::Field(index),
                });
            }
        }
        self.rows = rows;
        self.clamp_selected();
    }

    pub(super) fn matches_tab(&self, field: &ConfigField) -> bool {
        let tab = TABS[self.tab];
        if self.tab == 0 {
            return field.tab == "模型"
                && field.path.first().is_some_and(|root| root != "providers")
                && (!field.advanced || self.advanced);
        }
        // 插件字段由插件声明，设置页应始终可见，不能被核心“高级”开关隐藏。
        if tab == "插件" {
            return field.tab == tab;
        }
        if !field.advanced {
            return field.tab == tab;
        }
        tab == "高级" || (self.advanced && field.tab == tab)
    }

    /// 字段所属分组链：`(分组键, 标题, 分组路径, 可删除路径)`。
    pub(super) fn group_chain(&self, path: &[String]) -> Vec<GroupPath> {
        let mut chain = Vec::new();
        let second = path.get(1).map(String::as_str);
        match path.first().map(String::as_str) {
            Some("providers") => {
                let Some(provider) = path.get(1) else {
                    return chain;
                };
                chain.push((
                    format!("providers/{provider}"),
                    provider.clone(),
                    vec!["providers".into(), provider.clone()],
                    Some(vec!["providers".into(), provider.clone()]),
                ));
                if path.get(2).is_some_and(|part| part == "models")
                    && let Some(model) = path.get(3)
                {
                    chain.push((
                        format!("providers/{provider}/models/{model}"),
                        model.clone(),
                        vec![
                            "providers".into(),
                            provider.clone(),
                            "models".into(),
                            model.clone(),
                        ],
                        Some(vec![
                            "providers".into(),
                            provider.clone(),
                            "models".into(),
                            model.clone(),
                        ]),
                    ));
                }
            }
            Some("models") if second == Some("roles") => chain.push((
                "models/roles".into(),
                "模型角色".into(),
                vec!["models".into(), "roles".into()],
                None,
            )),
            Some("models") if second == Some("fusion") => chain.push((
                "models/fusion".into(),
                "Fusion 组合".into(),
                vec!["models".into(), "fusion".into()],
                None,
            )),
            Some("defaults") if path.len() >= 3 => {
                let Some(pattern) = path.get(2) else {
                    return chain;
                };
                chain.push((
                    format!("defaults/models/{pattern}"),
                    format!("默认 {pattern}"),
                    vec!["defaults".into(), "models".into(), pattern.clone()],
                    Some(vec!["defaults".into(), "models".into(), pattern.clone()]),
                ));
            }
            // 工具按子命令分组，避免同名设置（搜索超时、单文件上限）互相混淆。
            Some("tools") if path.len() >= 3 => {
                let tool = path[1].clone();
                if let Some(title) = tool_title(&tool) {
                    chain.push((
                        format!("tools/{tool}"),
                        title.into(),
                        vec!["tools".into(), tool.clone()],
                        None,
                    ));
                    if let Some(sub) = path.get(2)
                        && let Some(subtitle) = tool_subgroup_title(sub)
                    {
                        chain.push((
                            format!("tools/{tool}/{sub}"),
                            subtitle.into(),
                            vec!["tools".into(), tool.clone(), sub.clone()],
                            None,
                        ));
                    }
                }
            }
            Some("compaction") if path.len() >= 3 => {
                let title = match second {
                    Some("shake") => "本地修剪",
                    _ => return chain,
                };
                let method = path[1].clone();
                chain.push((
                    format!("compaction/{method}"),
                    title.into(),
                    vec!["compaction".into(), method],
                    None,
                ));
            }
            Some("session") if second == Some("retention") && path.len() >= 3 => chain.push((
                "session/retention".into(),
                "会话保留".into(),
                vec!["session".into(), "retention".into()],
                None,
            )),
            Some("mcp") if second == Some("servers") && path.len() >= 3 => {
                let Some(name) = path.get(2) else {
                    return chain;
                };
                chain.push((
                    format!("mcp/servers/{name}"),
                    name.clone(),
                    vec!["mcp".into(), "servers".into(), name.clone()],
                    Some(vec!["mcp".into(), "servers".into(), name.clone()]),
                ));
            }
            Some("plugins") if path.len() >= 4 && second.is_some() => {
                let plugin = path[1].clone();
                chain.push((
                    format!("plugins/{plugin}"),
                    plugin.clone(),
                    vec!["plugins".into(), plugin.clone()],
                    None,
                ));
            }
            _ => {}
        }
        chain
    }

    /// 分组行右侧的摘要：供应商显示模型数量，模型显示关键设置，其余显示子项数量。
    pub(super) fn group_detail(&self, group: &[String]) -> String {
        match group {
            [root, provider] if root == "providers" => {
                let count = self
                    .effective
                    .get("providers")
                    .and_then(|providers| providers.get(provider))
                    .and_then(|provider| provider.get("models"))
                    .and_then(Value::as_object)
                    .map_or(0, serde_json::Map::len);
                if count > 0 {
                    format!("{count} 个模型")
                } else {
                    "尚未添加模型".into()
                }
            }
            [root, provider, models, model] if root == "providers" && models == "models" => {
                self.model_summary(provider, model)
            }
            [root, roles] if root == "models" && (roles == "roles" || roles == "fusion") => {
                String::new()
            }
            [root, servers, _] if root == "mcp" && servers == "servers" => {
                match self.value(group).map(server_address) {
                    Some(address) if !address.is_empty() => address,
                    _ => String::new(),
                }
            }
            [root, ..] if root == "defaults" => "模型默认值".into(),
            _ => String::new(),
        }
    }

    /// 模型行的摘要：上下文长度、回复上限与思考能力。
    pub(super) fn model_summary(&self, provider: &str, model: &str) -> String {
        let Some(value) = self
            .effective
            .get("providers")
            .and_then(|providers| providers.get(provider))
            .and_then(|provider| provider.get("models"))
            .and_then(|models| models.get(model))
        else {
            return String::new();
        };
        let mut parts = Vec::new();
        if let Some(window) = value.get("context_window").and_then(Value::as_u64) {
            parts.push(format!("上下文 {}", crate::render::format_tokens(window)));
        }
        if let Some(output) = value.get("max_output_tokens").and_then(Value::as_u64) {
            parts.push(format!("回复 {}", crate::render::format_tokens(output)));
        }
        if value
            .get("reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            parts.push("支持思考".into());
        }
        parts.join(" · ")
    }

    pub fn is_expanded(&self, key: &str, depth: usize) -> bool {
        if !self.filter.is_empty() {
            return true;
        }
        if depth == 0 {
            !self.collapsed.contains(key)
        } else {
            self.expanded.contains(key)
        }
    }

    pub(super) fn toggle_group(&mut self, key: &str, depth: u8) {
        let key = key.to_owned();
        if depth == 0 {
            if !self.collapsed.remove(&key) {
                self.collapsed.insert(key);
            }
        } else if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        self.scroll.reveal_selection = true;
        self.rebuild_rows();
    }

    pub fn expand_all(&mut self) {
        for row in &self.rows {
            if let RowKind::Group(group) = &row.kind {
                self.collapsed.remove(&group.key);
                self.expanded.insert(group.key.clone());
            }
        }
        self.scroll.reveal_selection = true;
        self.rebuild_rows();
    }

    pub fn collapse_all(&mut self) {
        for row in &self.rows {
            if let RowKind::Group(group) = &row.kind {
                self.collapsed.insert(group.key.clone());
                self.expanded.remove(&group.key);
            }
        }
        self.scroll.reveal_selection = true;
        self.rebuild_rows();
    }

    pub fn row(&self) -> Option<&RowKind> {
        self.rows.get(self.selected).map(|row| &row.kind)
    }

    pub fn field(&self) -> Option<&ConfigField> {
        match self.row()? {
            RowKind::Field(index) => self.fields.get(*index),
            RowKind::Group(_) | RowKind::Action(_) => None,
        }
    }

    pub fn group(&self) -> Option<&Group> {
        match self.row()? {
            RowKind::Group(group) => Some(group),
            RowKind::Field(_) | RowKind::Action(_) => None,
        }
    }

    pub fn value(&self, path: &[String]) -> Option<&Value> {
        get(&self.effective, path)
    }

    /// 字段是否由当前配置文件显式设置（界面偏好没有继承概念）。
    pub fn explicit(&self, path: &[String]) -> bool {
        self.data
            .as_ref()
            .is_some_and(|data| get(&data.document, path).is_some())
    }

    pub(super) fn select_tab(&mut self, index: usize) {
        self.tab = index;
        self.selected = 0;
        self.offset = 0;
        self.scroll.reveal_selection = true;
        self.edit = None;
        self.rebuild_rows();
    }

    /// 选中指定路径对应的行，必要时展开其分组。
    pub(super) fn select_path(&mut self, path: &[String]) {
        for (index, row) in self.rows.iter().enumerate() {
            let matched = match &row.kind {
                RowKind::Field(field) => self.fields[*field].path == path,
                RowKind::Group(group) => group.path.as_deref() == Some(path),
                RowKind::Action(_) => false,
            };
            if matched {
                self.selected = index;
                self.scroll.reveal_selection = true;
                return;
            }
        }
    }
}
