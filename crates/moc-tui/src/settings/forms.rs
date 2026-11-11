use super::*;

/// 文本框的初始文本：对象按缩进 JSON，其余按原值。
pub(super) fn format_value(value: &Value, kind: &FieldKind) -> String {
    if value.is_null() {
        return String::new();
    }
    match kind {
        FieldKind::Object => serde_json::to_string_pretty(value).expect("JSON 值"),
        _ => value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned),
    }
}

/// 文本框内容按字段类型解析；`None` 表示留空，删除该项回到默认值。
pub(super) fn parse(text: &str, kind: &FieldKind) -> anyhow::Result<Option<Value>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(match kind {
            FieldKind::Text | FieldKind::Secret | FieldKind::Multiline => Some(json!("")),
            _ => None,
        });
    }
    Ok(Some(match kind {
        FieldKind::Integer => {
            let s = trimmed.replace(',', "");
            let (multiplier, s) = if s.ends_with('k') || s.ends_with('K') {
                (1_000u64, &s[..s.len() - 1])
            } else if s.ends_with('m') || s.ends_with('M') {
                (1_000_000u64, &s[..s.len() - 1])
            } else {
                (1u64, s.as_str())
            };
            let n = s
                .trim()
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("请输入有效整数（支持 200k、1m 或千分位）"))?;
            let val = n
                .checked_mul(multiplier)
                .ok_or_else(|| anyhow::anyhow!("数值超出支持范围"))?;
            json!(val)
        }
        FieldKind::Number => {
            let s = trimmed.replace(',', "");
            let (multiplier, s) = if s.ends_with('k') || s.ends_with('K') {
                (1_000.0f64, &s[..s.len() - 1])
            } else if s.ends_with('m') || s.ends_with('M') {
                (1_000_000.0f64, &s[..s.len() - 1])
            } else {
                (1.0f64, s.as_str())
            };
            let number = s
                .trim()
                .parse::<f64>()
                .map_err(|_| anyhow::anyhow!("请输入有效数字"))?;
            let number = number * multiplier;
            anyhow::ensure!(number.is_finite() && number >= 0.0, "请输入有限的非负数");
            json!(number)
        }
        FieldKind::Object => {
            let value: Value =
                serde_json::from_str(text).map_err(|e| anyhow::anyhow!("JSON 格式有误：{e}"))?;
            anyhow::ensure!(value.is_object(), "请输入 JSON 对象");
            value
        }
        _ => json!(text),
    }))
}

pub(super) fn key(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    mods: KeyModifiers,
    code: KeyCode,
) -> bool {
    if panel.edit.is_some() {
        if matches!(code, KeyCode::Tab | KeyCode::BackTab)
            && panel.edit.as_ref().is_some_and(|edit| !edit.popup)
        {
            edit_key(panel, app, connection, KeyModifiers::NONE, KeyCode::Enter);
            if panel.edit.is_none() {
                if code == KeyCode::BackTab || mods.contains(KeyModifiers::SHIFT) {
                    panel.selected = panel.selected.saturating_sub(1);
                } else {
                    panel.selected = (panel.selected + 1).min(panel.rows.len().saturating_sub(1));
                }
                panel.scroll.reveal_selection = true;
            }
            return false;
        }
        return edit_key(panel, app, connection, mods, code);
    }
    if matches!(panel.view, View::Fields)
        && panel.provider_page()
        && providers::key(panel, app, connection, mods, code)
    {
        return false;
    }
    match &panel.view {
        View::Fields => fields_key(panel, app, connection, mods, code),
        View::Confirm { .. } => confirm_key(panel, app, connection, code),
        View::Maintenance { .. } => maintenance_key(panel, code),
    }
}

/// 编辑态按键：交给编辑器，再按结果写入、报错或关闭。
fn edit_key(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    mods: KeyModifiers,
    code: KeyCode,
) -> bool {
    let Some(mut edit) = panel.edit.take() else {
        return false;
    };
    let result = match edit.editor.key(mods, code) {
        Outcome::Stay => {
            if matches!(edit.target, EditTarget::ModelFilter)
                && let Editor::Text(editor) = &edit.editor
                && let Some(discovery) = &mut panel.providers.discovery
            {
                discovery.query = editor.input.text.clone();
                panel.rebuild_rows();
                panel.select_action(ProviderAction::Search);
            }
            panel.edit = Some(edit);
            return false;
        }
        Outcome::Cancel => {
            panel.error = None;
            return false;
        }
        Outcome::Error(message) => {
            panel.error = Some(message);
            panel.edit = Some(edit);
            return false;
        }
        Outcome::Azure => {
            if let EditTarget::Field(path) = edit.target {
                panel.begin_azure(path);
            }
            return false;
        }
        Outcome::Submit { kind, text } => apply_edit(panel, app, connection, &edit, &kind, &text),
        Outcome::Value(value) => apply_value(panel, app, connection, &edit, value),
    };
    match result {
        Ok(()) => panel.error = None,
        Err(error) => {
            panel.error = Some(error.to_string());
            panel.edit = Some(edit);
        }
    }
    false
}

/// 弹窗控件选定的值写回字段。
fn apply_value(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    edit: &EditState,
    value: Option<Value>,
) -> anyhow::Result<()> {
    let EditTarget::Field(path) = &edit.target else {
        unreachable!("只有字段编辑会产出控件值");
    };
    panel.commit(&edit.title, app, connection, |panel| {
        panel.put(path, value);
        Ok(Store::of(path))
    })
}

fn fields_key(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    mods: KeyModifiers,
    code: KeyCode,
) -> bool {
    if code == KeyCode::Esc {
        if !panel.filter.is_empty() {
            // 如果搜索中光标停在某个字段上，顺带切到该字段所属的选项卡
            if let Some(RowKind::Field(idx)) = panel.row()
                && let Some(field) = panel.fields.get(*idx)
                && let Some(pos) = TABS.iter().position(|t| *t == field.tab)
            {
                panel.tab = pos;
            }
            panel.filter.clear();
            panel.rebuild_rows();
            return false;
        }
        // 改动都是即时保存的，直接返回即可。
        return true;
    }
    // 数字键 1-9, 0, - 直达前 11 个选项卡；插件页通过 [] 或 Tab 切换
    let tab_jump = match code {
        KeyCode::Char('1') => Some(0),
        KeyCode::Char('2') => Some(1),
        KeyCode::Char('3') => Some(2),
        KeyCode::Char('4') => Some(3),
        KeyCode::Char('5') => Some(4),
        KeyCode::Char('6') => Some(5),
        KeyCode::Char('7') => Some(6),
        KeyCode::Char('8') => Some(7),
        KeyCode::Char('9') => Some(8),
        KeyCode::Char('0') => Some(9),
        KeyCode::Char('-') => Some(10),
        _ => None,
    };
    if let Some(target_tab) = tab_jump
        && target_tab < TABS.len()
    {
        panel.select_tab(target_tab);
        return false;
    }
    // 方括号 [ 和 ] 快速切页
    if code == KeyCode::Char('[') {
        let prev = panel.tab.checked_sub(1).unwrap_or(TABS.len() - 1);
        panel.select_tab(prev);
        return false;
    }
    if code == KeyCode::Char(']') {
        let next = (panel.tab + 1) % TABS.len();
        panel.select_tab(next);
        return false;
    }
    if code == KeyCode::F(2) || code == KeyCode::Char('s') {
        switch_scope(panel, app, connection);
        return false;
    }
    if code == KeyCode::F(4) || code == KeyCode::Char('v') {
        panel.advanced = !panel.advanced;
        panel.rebuild_rows();
        return false;
    }
    if code == KeyCode::Char('e') {
        panel.expand_all();
        return false;
    }
    if code == KeyCode::Char('c') {
        panel.collapse_all();
        return false;
    }
    if code == KeyCode::Tab || code == KeyCode::BackTab {
        let previous = mods.contains(KeyModifiers::SHIFT) || code == KeyCode::BackTab;
        panel.select_tab(if previous {
            panel.tab.checked_sub(1).unwrap_or(TABS.len() - 1)
        } else {
            (panel.tab + 1) % TABS.len()
        });
        return false;
    }
    if panel.busy {
        return false;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        let shift = mods.contains(KeyModifiers::SHIFT);
        match code {
            KeyCode::Char('y' | 'Y') => {
                panel.redo(app, connection);
                return false;
            }
            KeyCode::Char('z' | 'Z') if shift => {
                panel.redo(app, connection);
                return false;
            }
            KeyCode::Char('z' | 'Z') => {
                panel.undo(app, connection);
                return false;
            }
            _ => {}
        }
    }
    let last = panel.rows.len().saturating_sub(1);
    let before = panel.selected;
    match code {
        KeyCode::Up | KeyCode::Char('k') => panel.selected = panel.selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => panel.selected = (panel.selected + 1).min(last),
        KeyCode::PageUp => panel.selected = panel.selected.saturating_sub(10),
        KeyCode::PageDown => panel.selected = (panel.selected + 10).min(last),
        KeyCode::Home | KeyCode::Char('g') => panel.selected = 0,
        KeyCode::End | KeyCode::Char('G') => panel.selected = last,
        KeyCode::Char('m') if TABS[panel.tab] == "会话" => panel.request(
            connection,
            &app.events,
            ConfigAction::Maintenance { apply: false },
            Job::Maintenance,
        ),
        KeyCode::Char(' ') => {
            if panel.cycle(app, connection) {
                return false;
            }
            panel.begin_edit(app, connection);
        }
        KeyCode::Enter => panel.begin_edit(app, connection),
        KeyCode::Left | KeyCode::Char('h') => collapse(panel, false),
        KeyCode::Right | KeyCode::Char('l') => collapse(panel, true),
        KeyCode::Char('a') => panel.add(),
        KeyCode::Char('d') if mods == KeyModifiers::CONTROL => panel.begin_named(
            EditTarget::ModelDefaults,
            "添加模型默认设置",
            "填写模型名匹配，例如 gpt-*",
            "",
        ),
        KeyCode::Char('/') => {
            let current = panel.filter.clone();
            panel.begin_named(
                EditTarget::Filter,
                "搜索设置项",
                "输入名称或关键字，留空显示全部",
                &current,
            )
        }
        KeyCode::Char('r') => reset_field(panel, app, connection),
        KeyCode::Delete => {
            if panel.group().is_some() {
                delete_group(panel);
            } else if panel.field().is_some() {
                reset_field(panel, app, connection);
            }
        }
        _ => {}
    }
    // 选中项移动过（含层级导航跳转）才重新显露，手动滚动的位置不丢。
    panel.scroll.reveal_selection |= panel.selected != before;
    false
}

/// 分组行的展开与收起；在字段行上按向左可直接跳至父级分组。
fn collapse(panel: &mut SettingsPanel, expand: bool) {
    let Some(row) = panel.rows.get(panel.selected) else {
        return;
    };
    match &row.kind {
        RowKind::Group(group) => {
            let depth = row.depth;
            let key = group.key.clone();
            let is_expanded = panel.is_expanded(&key, usize::from(depth));
            if expand {
                if !is_expanded {
                    panel.toggle_group(&key, depth);
                } else if panel.selected + 1 < panel.rows.len() {
                    panel.selected += 1;
                }
            } else if is_expanded {
                panel.toggle_group(&key, depth);
            } else if depth > 0
                && let Some(parent_idx) = find_parent_group(panel, panel.selected, depth)
            {
                panel.selected = parent_idx;
            }
        }
        RowKind::Action(_) => {}
        RowKind::Field(_) => {
            if !expand {
                let field_depth = row.depth;
                if let Some(parent_idx) = find_parent_group(panel, panel.selected, field_depth) {
                    panel.selected = parent_idx;
                }
            }
        }
    }
}

fn find_parent_group(
    panel: &SettingsPanel,
    current_index: usize,
    current_depth: u8,
) -> Option<usize> {
    for idx in (0..current_index).rev() {
        if let Some(r) = panel.rows.get(idx)
            && r.depth < current_depth
            && matches!(r.kind, RowKind::Group(_))
        {
            return Some(idx);
        }
    }
    None
}

/// R：删除该项在当前文件中的设置，回到默认值。
fn reset_field(panel: &mut SettingsPanel, app: &mut App, connection: &EmbeddedConnection) {
    let Some(field) = panel.field().cloned() else {
        return;
    };
    let label = format!("恢复默认：{}", field.label);
    let path = field.path.clone();
    let _ = panel.commit(&label, app, connection, |panel| {
        panel.remove_value(&path);
        Ok(Store::of(&path))
    });
}

/// Delete：删除选中的整个分组（供应商、模型、MCP 服务或默认设置）。
fn delete_group(panel: &mut SettingsPanel) {
    let Some(group) = panel.group() else {
        return;
    };
    let Some(path) = group.path.clone() else {
        panel.error = Some("这一组不能整组删除；可逐项按 R 恢复默认值".into());
        return;
    };
    panel.view = View::Confirm {
        text: format!("删除“{}”？按 Ctrl+Z 可以撤回。", group.title),
        action: ConfirmAction::Remove(path),
    };
}

/// F2：在全局配置与项目配置之间切换。
fn switch_scope(panel: &mut SettingsPanel, app: &mut App, connection: &EmbeddedConnection) {
    if app.saving_config() {
        panel.message = Some("正在保存改动，请稍候再切换范围".into());
        return;
    }
    let scope = match panel.scope {
        ConfigScope::Global => ConfigScope::Project,
        ConfigScope::Project => ConfigScope::Global,
    };
    panel.set_scope(scope, app, connection);
}

fn confirm_key(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    code: KeyCode,
) -> bool {
    let View::Confirm { text, action } = std::mem::replace(&mut panel.view, View::Fields) else {
        return false;
    };
    if matches!(code, KeyCode::Esc | KeyCode::Char('n' | 'N')) {
        return false;
    }
    if !matches!(code, KeyCode::Char('y' | 'Y') | KeyCode::Enter) {
        panel.view = View::Confirm { text, action };
        return false;
    }
    match action {
        ConfirmAction::Remove(path) => {
            let label = format!("删除 {}", path.last().cloned().unwrap_or_default());
            let _ = panel.commit(&label, app, connection, |panel| {
                panel.remove_value(&path);
                Ok(Store::of(&path))
            });
        }
        ConfirmAction::Run(action) => panel.request(connection, &app.events, action, Job::Update),
    }
    false
}

fn maintenance_key(panel: &mut SettingsPanel, code: KeyCode) -> bool {
    let View::Maintenance { report, mut offset } = std::mem::replace(&mut panel.view, View::Fields)
    else {
        return false;
    };
    if code == KeyCode::Esc {
        return false;
    }
    if code == KeyCode::Enter && !report.applied {
        panel.view = View::Confirm {
            text: format!(
                "执行维护？预计清理 {} 个临时文件；被会话引用的文件不会处理。",
                report.removed.len()
            ),
            action: ConfirmAction::Run(ConfigAction::Maintenance { apply: true }),
        };
        return false;
    }
    let total = report.removed.len();
    match code {
        KeyCode::Down | KeyCode::Char('j') => {
            offset = (offset + 1).min(total);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            offset = offset.saturating_sub(1);
        }
        KeyCode::PageDown => {
            offset = (offset + 10).min(total);
        }
        KeyCode::PageUp => {
            offset = offset.saturating_sub(10);
        }
        KeyCode::Home | KeyCode::Char('g') => {
            offset = 0;
        }
        KeyCode::End | KeyCode::Char('G') => {
            offset = total.saturating_sub(1);
        }
        _ => {}
    }
    panel.view = View::Maintenance { report, offset };
    false
}

/// 文本框提交：字段按类型解析后写回，其余目标是新增条目、Azure 版本或搜索。
fn apply_edit(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    edit: &EditState,
    kind: &FieldKind,
    text: &str,
) -> anyhow::Result<()> {
    let label = edit.title.as_str();
    match &edit.target {
        EditTarget::Field(path) => {
            let value = parse(text, kind)?;
            panel.commit(label, app, connection, |panel| {
                panel.put(path, value);
                Ok(Store::of(path))
            })?;
        }
        EditTarget::Azure(path) => {
            let version = text.trim();
            anyhow::ensure!(
                !version.is_empty() && !version.contains(char::is_whitespace),
                "接口版本不能为空，也不能包含空白，例如 2024-10-21"
            );
            let value = json!({ "Azure": { "api_version": version } });
            panel.commit("服务端点：Azure OpenAI", app, connection, |panel| {
                panel.set_value(path, value);
                Ok(Store::Core)
            })?;
        }
        EditTarget::ModelFilter => {
            if let Some(discovery) = &mut panel.providers.discovery {
                discovery.query = text.into();
            }
            panel.rebuild_rows();
            panel.select_action(ProviderAction::Search);
        }
        EditTarget::Model(provider) => {
            let id = text.trim();
            anyhow::ensure!(!id.is_empty(), "模型 ID 不能为空");
            panel.commit(label, app, connection, |panel| {
                panel.insert_model(provider, id)?;
                Ok(Store::Core)
            })?;
            panel.open_model(provider, id);
        }
        EditTarget::Mcp => {
            let name = text.trim().to_owned();
            moc_mcp_name(&name)?;
            let path = vec!["mcp".into(), "servers".into(), name];
            anyhow::ensure!(panel.value(&path).is_none(), "同名 MCP 服务已存在");
            let target = path.clone();
            panel.commit(label, app, connection, |panel| {
                panel.set_value(&target, json!({"type":"stdio","command":"","args":[],"env":{},"headers":{},"enabled":true,"tools":{"allow":[],"deny":[]}}));
                Ok(Store::Core)
            })?;
            panel.message = Some("已添加 MCP 服务，请填写启动程序或地址".into());
        }
        EditTarget::ModelDefaults => {
            let pattern = text.trim().to_owned();
            anyhow::ensure!(!pattern.is_empty(), "模型名匹配不能为空");
            let path = vec!["defaults".into(), "models".into(), pattern];
            let target = path.clone();
            panel.commit(label, app, connection, |panel| {
                panel.set_value(
                    &target,
                    json!({"context_window":200000,"max_output_tokens":32000,"reasoning":false}),
                );
                Ok(Store::Core)
            })?;
            panel.select_tab(0);
            panel.select_path(&path);
        }
        EditTarget::Filter => {
            panel.filter = text.trim().into();
            panel.selected = 0;
            panel.offset = 0;
            panel.scroll.reveal_selection = true;
            panel.rebuild_rows();
        }
    }
    Ok(())
}

fn moc_mcp_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 64
            && !name.contains("__")
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)),
        "MCP 名使用字母、数字、-、_，不能含 __"
    );
    Ok(())
}
