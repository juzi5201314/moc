use super::*;

pub(super) fn hint_for(kind: &FieldKind) -> &'static str {
    match kind {
        FieldKind::Choice(_) => "↑↓ 选择 · Enter 应用 · Esc 取消",
        FieldKind::Strings => "每行一项 · Ctrl+Enter 应用 · Esc 取消",
        FieldKind::Object => "JSON 格式 · Ctrl+Enter 应用 · Esc 取消",
        FieldKind::Multiline => "Ctrl+Enter 应用 · Esc 取消",
        FieldKind::Secret => "F6 显示或隐藏 · Enter 应用 · Esc 取消",
        _ => "Enter 应用 · Esc 取消",
    }
}

pub(super) fn format_value(value: &Value, kind: &FieldKind) -> String {
    if value.is_null() {
        return String::new();
    }
    match kind {
        FieldKind::Strings => value
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .map(|value| value.as_str().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        FieldKind::Object => serde_json::to_string_pretty(value).expect("JSON 值"),
        _ => value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string()),
    }
}

/// 编辑框里的初始文本：候选值显示标签，其余按原值。
pub(super) fn edit_text(value: &Value, kind: &FieldKind) -> String {
    if let (FieldKind::Choice(options), Some(current)) = (kind, value.as_str())
        && let Some(option) = options.iter().find(|option| option.value == current)
    {
        return option.label.clone();
    }
    format_value(value, kind)
}

fn parse(text: &str, kind: &FieldKind) -> anyhow::Result<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(match kind {
            FieldKind::Strings => json!([]),
            FieldKind::Text | FieldKind::Secret | FieldKind::Multiline => json!(""),
            _ => Value::Null,
        });
    }
    Ok(match kind {
        FieldKind::Bool => json!(trimmed.parse::<bool>()?),
        FieldKind::Integer => json!(trimmed.parse::<u64>()?),
        FieldKind::Number => {
            let number = trimmed.parse::<f64>()?;
            anyhow::ensure!(number.is_finite() && number >= 0.0, "请输入有限的非负数");
            json!(number)
        }
        FieldKind::Strings => json!(
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
        ),
        FieldKind::Object => {
            let value: Value = serde_json::from_str(text)?;
            anyhow::ensure!(
                value.is_object() || value.is_string(),
                "请输入 JSON 对象；端点风格也可为字符串"
            );
            value
        }
        FieldKind::Choice(options) => {
            let chosen = options
                .iter()
                .find(|option| option.value == trimmed || option.label == trimmed)
                .map(|option| option.value.as_str());
            let value = chosen.ok_or_else(|| anyhow::anyhow!("请从列出的候选值中选择"))?;
            json!(value)
        }
        _ => json!(text),
    })
}

pub(super) fn key(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    mods: KeyModifiers,
    code: KeyCode,
) -> bool {
    if panel.edit.is_some() {
        return edit_key(panel, app, connection, mods, code);
    }
    match &panel.view {
        View::Fields => fields_key(panel, app, connection, mods, code),
        View::Confirm { .. } => confirm_key(panel, app, connection, code),
        View::Models { .. } => models_key(panel, app, connection, code),
        View::Workspaces { .. } => workspaces_key(panel, code),
        View::Maintenance(_) => maintenance_key(panel, code),
    }
}

/// 编辑态按键：Enter 保存、Esc 取消，候选值用 ↑↓ 切换。
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
    if code == KeyCode::Esc {
        return false;
    }
    if code == KeyCode::F(6) {
        edit.reveal = !edit.reveal;
        panel.edit = Some(edit);
        return false;
    }
    let multiline = matches!(
        edit.kind,
        FieldKind::Multiline | FieldKind::Strings | FieldKind::Object
    );
    let submit = code == KeyCode::Enter && (mods.contains(KeyModifiers::CONTROL) || !multiline);
    if submit {
        match apply_edit(
            panel,
            app,
            connection,
            &edit.target,
            &edit.kind,
            &edit.input.text,
            &edit.title,
        ) {
            Ok(()) => panel.error = None,
            Err(error) => {
                panel.error = Some(error.to_string());
                panel.edit = Some(edit);
            }
        }
        return false;
    }
    match (&edit.kind, code) {
        (FieldKind::Choice(options), KeyCode::Up | KeyCode::Down) if !options.is_empty() => {
            let current = options
                .iter()
                .position(|option| option.label == edit.input.text)
                .unwrap_or(0);
            let next = if code == KeyCode::Up {
                current.checked_sub(1).unwrap_or(options.len() - 1)
            } else {
                (current + 1) % options.len()
            };
            edit.input.text = options[next].label.clone();
            edit.input.cursor = edit.input.text.len();
        }
        _ => edit_input(&mut edit.input, mods, code),
    }
    panel.edit = Some(edit);
    false
}

fn fields_key(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    mods: KeyModifiers,
    code: KeyCode,
) -> bool {
    if code == KeyCode::Esc {
        // 改动都是即时保存的，直接返回即可。
        return true;
    }
    if code == KeyCode::F(2) {
        switch_scope(panel, app, connection);
        return false;
    }
    if code == KeyCode::F(4) {
        panel.advanced = !panel.advanced;
        panel.rebuild_rows();
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
    match code {
        KeyCode::Up => panel.selected = panel.selected.saturating_sub(1),
        KeyCode::Down => panel.selected = (panel.selected + 1).min(last),
        KeyCode::PageUp => panel.selected = panel.selected.saturating_sub(10),
        KeyCode::PageDown => panel.selected = (panel.selected + 10).min(last),
        KeyCode::Home => panel.selected = 0,
        KeyCode::End => panel.selected = last,
        KeyCode::Enter | KeyCode::Char(' ') => panel.begin_edit(app, connection),
        KeyCode::Left => collapse(panel, false),
        KeyCode::Right => collapse(panel, true),
        KeyCode::Char('a') => panel.add(),
        KeyCode::Char('f') => panel.fetch(connection, &app.events),
        KeyCode::Char('w') if TABS[panel.tab] == "OpenViking" => panel.request(
            connection,
            &app.events,
            ConfigAction::Workspaces,
            Job::Workspaces,
        ),
        KeyCode::Char('g') if TABS[panel.tab] == "会话" => panel.request(
            connection,
            &app.events,
            ConfigAction::Maintenance { apply: false },
            Job::Maintenance,
        ),
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
        KeyCode::Delete => delete_group(panel),
        _ => {}
    }
    false
}

/// 分组行的展开与收起；字段行不动。
fn collapse(panel: &mut SettingsPanel, expand: bool) {
    let Some((key, depth)) = panel
        .rows
        .get(panel.selected)
        .and_then(|row| match &row.kind {
            RowKind::Group(group) => Some((group.key.clone(), row.depth)),
            RowKind::Field(_) => None,
        })
    else {
        return;
    };
    if panel.is_expanded(&key, usize::from(depth)) != expand {
        panel.toggle_group(&key, depth);
    }
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
    if matches!(code, KeyCode::Esc | KeyCode::Char('n')) {
        return false;
    }
    if code != KeyCode::Char('y') {
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

fn models_key(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    code: KeyCode,
) -> bool {
    let View::Models {
        provider,
        rows,
        mut checked,
        mut selected,
    } = std::mem::replace(&mut panel.view, View::Fields)
    else {
        return false;
    };
    if code == KeyCode::Esc {
        return false;
    }
    let last = rows.len().saturating_sub(1);
    match code {
        KeyCode::Up => selected = selected.saturating_sub(1),
        KeyCode::Down => selected = (selected + 1).min(last),
        KeyCode::PageUp => selected = selected.saturating_sub(10),
        KeyCode::PageDown => selected = (selected + 10).min(last),
        KeyCode::Char(' ') => {
            if let Some(row) = rows.get(selected) {
                let path = vec![
                    "providers".into(),
                    provider.clone(),
                    "models".into(),
                    row.id.clone(),
                ];
                if panel.value(&path).is_none() && !checked.remove(&row.id) {
                    checked.insert(row.id.clone());
                }
            }
        }
        KeyCode::Enter if !panel.busy => {
            if checked.is_empty() {
                panel.error = Some("请先用 Space 勾选要添加的模型".into());
            } else {
                let added: Vec<String> = checked
                    .iter()
                    .filter(|id| {
                        panel
                            .value(&[
                                "providers".into(),
                                provider.clone(),
                                "models".into(),
                                (*id).clone(),
                            ])
                            .is_none()
                    })
                    .cloned()
                    .collect();
                let label = if added.len() == 1 {
                    format!("添加模型 {}", added[0])
                } else {
                    format!("添加 {} 个模型", added.len())
                };
                let target = provider.clone();
                if let Err(error) = panel.commit(&label, app, connection, |panel| {
                    for id in &added {
                        panel.insert_model(&target, id)?;
                    }
                    Ok(Store::Core)
                }) {
                    panel.error = Some(error.to_string());
                }
                panel.message = Some(format!("已添加 {} 个模型，可继续调整设置", added.len()));
                panel.select_tab(0);
                if let Some(id) = added.first() {
                    panel.select_path(&["providers".into(), provider, "models".into(), id.clone()]);
                }
                return false;
            }
        }
        _ => {}
    }
    panel.view = View::Models {
        provider,
        rows,
        checked,
        selected,
    };
    false
}

fn workspaces_key(panel: &mut SettingsPanel, code: KeyCode) -> bool {
    let View::Workspaces { rows, mut selected } = std::mem::replace(&mut panel.view, View::Fields)
    else {
        return false;
    };
    if code == KeyCode::Esc {
        return false;
    }
    match code {
        KeyCode::Up => selected = selected.saturating_sub(1),
        KeyCode::Down => selected = (selected + 1).min(rows.len().saturating_sub(1)),
        KeyCode::Enter => {
            if let Some(binding) = rows.get(selected) {
                panel.begin_named(
                    EditTarget::Rebind(binding.clone()),
                    "改绑工作区目录",
                    "填写新的目录路径",
                    &binding.path,
                );
                return false;
            }
        }
        KeyCode::Delete => {
            if let Some(binding) = rows.get(selected) {
                panel.view = View::Confirm {
                    text: if binding.shared {
                        format!(
                            "解除 {} 的记忆绑定？这个目录与其他工作区共享记忆，记忆本身会保留。",
                            binding.path
                        )
                    } else {
                        format!(
                            "删除 {} 的专属记忆？共享记忆不会删除，此操作不可撤销。",
                            binding.path
                        )
                    },
                    action: ConfirmAction::Run(ConfigAction::DeleteWorkspace {
                        url: binding.url.clone(),
                        path: binding.path.clone(),
                        confirmed_peer: binding.peer_id.clone(),
                    }),
                };
                return false;
            }
        }
        _ => {}
    }
    panel.view = View::Workspaces { rows, selected };
    false
}

fn maintenance_key(panel: &mut SettingsPanel, code: KeyCode) -> bool {
    let View::Maintenance(report) = std::mem::replace(&mut panel.view, View::Fields) else {
        return false;
    };
    if code == KeyCode::Esc {
        return false;
    }
    if code == KeyCode::Enter && !report.applied {
        panel.view = View::Confirm {
            text: format!(
                "执行维护？预计归档 {} 个会话、删除 {} 个临时文件；正在使用的会话不会处理。",
                report.archived.len(),
                report.removed.len()
            ),
            action: ConfirmAction::Run(ConfigAction::Maintenance { apply: true }),
        };
        return false;
    }
    if code == KeyCode::Down {
        panel.offset = (panel.offset + 1).min(report.archived.len() + report.removed.len());
    }
    if code == KeyCode::Up {
        panel.offset = panel.offset.saturating_sub(1);
    }
    panel.view = View::Maintenance(report);
    false
}

fn apply_edit(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    target: &EditTarget,
    kind: &FieldKind,
    text: &str,
    label: &str,
) -> anyhow::Result<()> {
    match target {
        EditTarget::Field(path) => {
            let value = parse(text, kind)?;
            let path = path.clone();
            panel.commit(label, app, connection, |panel| {
                if value.is_null() {
                    panel.remove_value(&path);
                } else {
                    panel.set_value(&path, value);
                }
                Ok(Store::of(&path))
            })?;
        }
        EditTarget::Provider => {
            let name = text.trim().to_owned();
            anyhow::ensure!(
                !name.is_empty() && !name.contains(['/', '\n']),
                "供应商标识不能为空，也不能包含斜线或换行"
            );
            anyhow::ensure!(
                panel
                    .effective
                    .get("providers")
                    .and_then(|providers| providers.get(&name))
                    .is_none(),
                "供应商 {name} 已存在"
            );
            let path = vec!["providers".into(), name];
            let target = path.clone();
            panel.commit(label, app, connection, |panel| {
                panel.set_value(
                    &target,
                    json!({"api":"openai_chat","base_url":"","api_key":"","models":{}}),
                );
                Ok(Store::Core)
            })?;
            panel.select_path(&path);
            panel.message = Some("已添加供应商，请填写服务地址与密钥".into());
        }
        EditTarget::Model(provider) => {
            let id = text.trim().to_owned();
            anyhow::ensure!(!id.is_empty(), "模型 ID 不能为空");
            let path = vec![
                "providers".into(),
                provider.clone(),
                "models".into(),
                id.clone(),
            ];
            let target = provider.clone();
            let _ = panel.commit(label, app, connection, |panel| {
                panel.insert_model(&target, &id)?;
                Ok(Store::Core)
            });
            panel.select_tab(0);
            panel.select_path(&path);
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
        EditTarget::Rebind(binding) => panel.request(
            connection,
            &app.events,
            ConfigAction::RebindWorkspace {
                url: binding.url.clone(),
                path: binding.path.clone(),
                new_path: text.trim().into(),
            },
            Job::Update,
        ),
        EditTarget::Filter => {
            panel.filter = text.trim().into();
            panel.selected = 0;
            panel.offset = 0;
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

fn edit_input(input: &mut PromptState, mods: KeyModifiers, code: KeyCode) {
    match (code, mods) {
        (KeyCode::Enter, _) => input.insert_char('\n'),
        (KeyCode::Backspace, _) => input.backspace(),
        (KeyCode::Delete, _) => input.delete(),
        (KeyCode::Left, _) => input.left(),
        (KeyCode::Right, _) => input.right(),
        (KeyCode::Up, _) => {
            input.up();
        }
        (KeyCode::Down, _) => {
            input.down();
        }
        (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => input.move_line_start(),
        (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => input.move_line_end(),
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => input.clear(),
        (KeyCode::Char('w'), KeyModifiers::CONTROL) => input.delete_word_back(),
        (KeyCode::Char(character), KeyModifiers::NONE | KeyModifiers::SHIFT)
            if !character.is_control() =>
        {
            input.insert_char(character)
        }
        _ => {}
    }
}
