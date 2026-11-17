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
use rust_i18n::t;
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
        // 供应商目录按当前 API 重建，避免本地切换协议后旧字段与候选值继续残留。
        // 其他目录仍增量补充，保留服务端插件声明的类型与说明。
        self.fields = data
            .fields
            .iter()
            .filter(|field| !field.path.first().is_some_and(|part| part == "providers"))
            .cloned()
            .collect();
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
        let mut tree = GroupNode::default();
        for (index, field) in self.fields.iter().enumerate() {
            if field.path.len() >= 2 && field.path[0] == "models" && field.path[1] == "fusion" {
                continue;
            }
            if searching {
                if !matches_filter(field, &filter) {
                    continue;
                }
            } else if !self.matches_tab(field) {
                continue;
            }
            let chain = self.group_chain(&field.path);
            insert_into_tree(&mut tree, &chain, index, field.advanced);
        }
        let mut rows = Vec::new();
        self.emit_node(&tree, 0, searching, &mut rows);
        self.rows = rows;
        self.clamp_selected();
    }

    fn emit_node(&self, node: &GroupNode, depth: usize, searching: bool, rows: &mut Vec<Row>) {
        let child_depth = if let Some(group) = &node.group {
            rows.push(Row {
                depth: u8::try_from(depth).unwrap_or(u8::MAX),
                kind: RowKind::Group(Group {
                    key: group.0.clone(),
                    title: group.1.clone(),
                    detail: self.group_detail(&group.2),
                    grouping: group.2.clone(),
                    path: group.3.clone(),
                }),
            });
            if !searching && !self.is_expanded(&group.0, depth) {
                return;
            }
            depth + 1
        } else {
            depth
        };
        for &index in &node.fields {
            rows.push(Row {
                depth: u8::try_from(child_depth).unwrap_or(u8::MAX),
                kind: RowKind::Field(index),
            });
        }
        for child in &node.children {
            self.emit_node(child, child_depth, searching, rows);
        }
        if !node.advanced_fields.is_empty() {
            if searching {
                for &index in &node.advanced_fields {
                    rows.push(Row {
                        depth: u8::try_from(child_depth).unwrap_or(u8::MAX),
                        kind: RowKind::Field(index),
                    });
                }
            } else {
                let (adv_key, grouping) = match &node.group {
                    Some(group) => {
                        let mut grouping = group.2.clone();
                        grouping.push("__advanced".into());
                        (format!("{}/__advanced", group.0), grouping)
                    }
                    None => {
                        let key = format!("__tab_{}/__advanced", self.tab);
                        (
                            key,
                            vec![format!("__tab_{}", self.tab), "__advanced".into()],
                        )
                    }
                };
                let expanded = self.is_expanded(&adv_key, child_depth);
                rows.push(Row {
                    depth: u8::try_from(child_depth).unwrap_or(u8::MAX),
                    kind: RowKind::Group(Group {
                        key: adv_key,
                        title: t!("tui.settings.advanced_title").into_owned(),
                        detail: t!(
                            "tui.settings.item_count",
                            count = node.advanced_fields.len()
                        )
                        .into_owned(),
                        grouping,
                        path: None,
                    }),
                });
                if expanded {
                    for &index in &node.advanced_fields {
                        rows.push(Row {
                            depth: u8::try_from(child_depth + 1).unwrap_or(u8::MAX),
                            kind: RowKind::Field(index),
                        });
                    }
                }
            }
        }
    }

    pub(super) fn matches_tab(&self, field: &ConfigField) -> bool {
        if field.path.len() >= 2 && field.path[0] == "models" && field.path[1] == "fusion" {
            return false;
        }
        TABS[self.tab] == super::tab_of(&field.path)
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
                t!("tui.nav.models_roles").into_owned(),
                vec!["models".into(), "roles".into()],
                None,
            )),
            Some("agents") => chain.push((
                "agents".into(),
                t!("tui.nav.agents").into_owned(),
                vec!["agents".into()],
                None,
            )),
            Some("defaults") if path.len() >= 3 => {
                let Some(pattern) = path.get(2) else {
                    return chain;
                };
                chain.push((
                    format!("defaults/models/{pattern}"),
                    t!("tui.nav.default_fmt", pattern = pattern.as_str()).into_owned(),
                    vec!["defaults".into(), "models".into(), pattern.clone()],
                    Some(vec!["defaults".into(), "models".into(), pattern.clone()]),
                ));
            }
            Some("context_files") => chain.push((
                "context_files".into(),
                "AGENTS.md".into(),
                vec!["context_files".into()],
                None,
            )),
            Some("system_prompt") => chain.push((
                "system_prompt".into(),
                "System Prompt".into(),
                vec!["system_prompt".into()],
                None,
            )),
            Some("skills") => chain.push((
                "skills".into(),
                "Skills".into(),
                vec!["skills".into()],
                None,
            )),
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
            Some("web_fetch") => chain.push((
                "web_fetch".into(),
                t!("tui.nav.web_fetch").into_owned(),
                vec!["web_fetch".into()],
                None,
            )),
            Some("workspace") => chain.push((
                "workspace".into(),
                t!("tui.nav.workspace").into_owned(),
                vec!["workspace".into()],
                None,
            )),
            Some("sandbox") => chain.push((
                "sandbox".into(),
                t!("tui.nav.sandbox").into_owned(),
                vec!["sandbox".into()],
                None,
            )),
            Some("compaction") => {
                chain.push((
                    "compaction".into(),
                    t!("tui.nav.compaction").into_owned(),
                    vec!["compaction".into()],
                    None,
                ));
                if second == Some("shake") && path.len() >= 3 {
                    chain.push((
                        "compaction/shake".into(),
                        t!("tui.nav.shake").into_owned(),
                        vec!["compaction".into(), "shake".into()],
                        None,
                    ));
                }
            }
            Some("session") => {
                chain.push((
                    "session".into(),
                    t!("tui.nav.session").into_owned(),
                    vec!["session".into()],
                    None,
                ));
                if second == Some("retention") && path.len() >= 3 {
                    chain.push((
                        "session/retention".into(),
                        t!("tui.nav.retention").into_owned(),
                        vec!["session".into(), "retention".into()],
                        None,
                    ));
                }
            }
            Some("features") => chain.push((
                "features".into(),
                t!("tui.nav.features").into_owned(),
                vec!["features".into()],
                None,
            )),
            Some("limits") => chain.push((
                "limits".into(),
                t!("tui.nav.limits").into_owned(),
                vec!["limits".into()],
                None,
            )),
            Some("workflow") => chain.push((
                "workflow".into(),
                "Workflow".into(),
                vec!["workflow".into()],
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
            Some("mcp") => chain.push((
                "mcp".into(),
                t!("tui.nav.mcp").into_owned(),
                vec!["mcp".into()],
                None,
            )),
            Some("plugins") if path.len() >= 4 && second.is_some() => {
                let plugin = path[1].clone();
                chain.push((
                    format!("plugins/{plugin}"),
                    plugin.clone(),
                    vec!["plugins".into(), plugin.clone()],
                    None,
                ));
            }
            Some("plugins") => chain.push((
                "plugins".into(),
                t!("tui.nav.plugins").into_owned(),
                vec!["plugins".into()],
                None,
            )),
            Some("tui") if second == Some("keybindings") => chain.push((
                "tui/keybindings".into(),
                t!("tui.nav.keybindings").into_owned(),
                vec!["tui".into(), "keybindings".into()],
                None,
            )),
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
                    t!("tui.nav.models_count", count = count).into_owned()
                } else {
                    t!("tui.nav.no_models").into_owned()
                }
            }
            [root, provider, models, model] if root == "providers" && models == "models" => {
                self.model_summary(provider, model)
            }
            [root, roles] if root == "models" && roles == "roles" => String::new(),
            [root, servers, _] if root == "mcp" && servers == "servers" => {
                match self.value(group).map(server_address) {
                    Some(address) if !address.is_empty() => address,
                    _ => String::new(),
                }
            }
            [root, ..] if root == "defaults" => t!("tui.nav.model_defaults").into_owned(),
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
            parts
                .push(t!("tui.nav.ctx_fmt", v = crate::render::format_tokens(window)).into_owned());
        }
        if let Some(output) = value.get("max_output_tokens").and_then(Value::as_u64) {
            parts
                .push(t!("tui.nav.out_fmt", v = crate::render::format_tokens(output)).into_owned());
        }
        if value
            .get("reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            parts.push(t!("tui.nav.thinking").into_owned());
        }
        parts.join(&t!("tui.nav.sep"))
    }

    pub fn is_expanded(&self, key: &str, depth: usize) -> bool {
        if !self.filter.is_empty() {
            return true;
        }
        if depth == 0 && !key.ends_with("/__advanced") {
            !self.collapsed.contains(key)
        } else {
            self.expanded.contains(key)
        }
    }

    pub(super) fn toggle_group(&mut self, key: &str, depth: u8) {
        let key = key.to_owned();
        if depth == 0 && !key.ends_with("/__advanced") {
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
        for _ in 0..4 {
            let mut changed = false;
            for row in &self.rows {
                if let RowKind::Group(group) = &row.kind {
                    changed |= self.collapsed.remove(&group.key);
                    changed |= self.expanded.insert(group.key.clone());
                }
            }
            if !changed {
                break;
            }
            self.rebuild_rows();
        }
        self.scroll.reveal_selection = true;
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

#[derive(Default)]
struct GroupNode {
    group: Option<GroupPath>,
    fields: Vec<usize>,
    advanced_fields: Vec<usize>,
    children: Vec<GroupNode>,
}

fn insert_into_tree(node: &mut GroupNode, chain: &[GroupPath], field_index: usize, advanced: bool) {
    if let Some((head, tail)) = chain.split_first() {
        let child = if let Some(pos) = node
            .children
            .iter()
            .position(|child| child.group.as_ref().is_some_and(|group| group.0 == head.0))
        {
            &mut node.children[pos]
        } else {
            node.children.push(GroupNode {
                group: Some(head.clone()),
                ..Default::default()
            });
            node.children.last_mut().expect("刚插入节点")
        };
        insert_into_tree(child, tail, field_index, advanced);
    } else if advanced {
        node.advanced_fields.push(field_index);
    } else {
        node.fields.push(field_index);
    }
}
