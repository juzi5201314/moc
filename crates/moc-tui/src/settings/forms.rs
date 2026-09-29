use super::*;

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
            anyhow::ensure!(
                options.iter().any(|option| option == trimmed),
                "请从列出的可用选项中选择"
            );
            json!(trimmed)
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
    let view = std::mem::replace(&mut panel.view, View::Fields);
    match view {
        View::Edit {
            target,
            kind,
            mut input,
            mut reveal,
        } => {
            if code == KeyCode::Esc {
                return false;
            }
            if code == KeyCode::F(6) {
                reveal = !reveal;
            }
            let multiline = matches!(
                kind,
                FieldKind::Multiline | FieldKind::Strings | FieldKind::Object
            );
            let submit =
                code == KeyCode::Enter && (mods.contains(KeyModifiers::CONTROL) || !multiline);
            if submit {
                let result =
                    apply_edit(panel, &target, &kind, &input.text, connection, &app.events);
                match result {
                    Ok(()) => {
                        panel.error = None;
                        return false;
                    }
                    Err(error) => panel.error = Some(error.to_string()),
                }
            } else {
                match (&kind, code) {
                    (FieldKind::Choice(options), KeyCode::Up | KeyCode::Down)
                        if !options.is_empty() =>
                    {
                        let current = options
                            .iter()
                            .position(|option| option == &input.text)
                            .unwrap_or(0);
                        let next = if code == KeyCode::Up {
                            current.checked_sub(1).unwrap_or(options.len() - 1)
                        } else {
                            (current + 1) % options.len()
                        };
                        input.text = options[next].clone();
                        input.cursor = input.text.len();
                    }
                    _ => edit_key(&mut input, mods, code),
                }
            }
            panel.view = View::Edit {
                target,
                kind,
                input,
                reveal,
            };
        }
        View::Confirm { text, action } => {
            if matches!(code, KeyCode::Esc | KeyCode::Char('n')) {
                return false;
            }
            if code == KeyCode::Char('y') {
                match action {
                    ConfirmAction::Close => return true,
                    ConfirmAction::Remove(path) => {
                        if path.first().is_some_and(|part| part == "tui") {
                            remove(&mut panel.tui, &path[1..]);
                        } else if let Some(data) = &mut panel.data {
                            remove(&mut data.document, &path);
                        }
                        panel.rebuild();
                    }
                    ConfirmAction::Run(action) => {
                        panel.request(connection, &app.events, action, false)
                    }
                }
            } else {
                panel.view = View::Confirm { text, action };
            }
        }
        View::Models {
            provider,
            rows,
            mut checked,
            mut selected,
            mut offset,
        } => {
            if code == KeyCode::Esc {
                return false;
            }
            match code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => selected = (selected + 1).min(rows.len().saturating_sub(1)),
                KeyCode::PageUp => selected = selected.saturating_sub(10),
                KeyCode::PageDown => selected = (selected + 10).min(rows.len().saturating_sub(1)),
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
                        panel.error =
                            Some("请先用 Space 或鼠标勾选要添加的模型，不会默认全部加入".into());
                    } else {
                        for id in &checked {
                            if panel
                                .value(&[
                                    "providers".into(),
                                    provider.clone(),
                                    "models".into(),
                                    id.clone(),
                                ])
                                .is_none()
                                && let Err(error) = panel.add_model(&provider, id)
                            {
                                panel.error = Some(error.to_string());
                            }
                        }
                        panel.message = Some(format!(
                            "已将 {} 个勾选模型加入草稿；Ctrl+S 保存",
                            checked.len()
                        ));
                        panel.select_tab(0);
                        return false;
                    }
                }
                _ => {}
            }
            offset = offset.min(selected);
            panel.view = View::Models {
                provider,
                rows,
                checked,
                selected,
                offset,
            };
        }
        View::Workspaces {
            rows,
            mut selected,
            offset,
        } => {
            if code == KeyCode::Esc {
                return false;
            }
            match code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => selected = (selected + 1).min(rows.len().saturating_sub(1)),
                KeyCode::Enter => {
                    if let Some(binding) = rows.get(selected) {
                        panel.begin_named(EditTarget::Rebind(binding.clone()));
                        return false;
                    }
                }
                KeyCode::Delete => {
                    if let Some(binding) = rows.get(selected) {
                        panel.view = View::Confirm {
                            text: if binding.shared {
                                format!(
                                    "解除路径 {} 的绑定？此知识域还有其他路径使用，远程记忆会保留。",
                                    binding.path
                                )
                            } else {
                                format!(
                                    "删除 {} 的专属知识域 {}？共享记忆不会删除，此操作不可撤销。",
                                    binding.path, binding.peer_id
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
            panel.view = View::Workspaces {
                rows,
                selected,
                offset,
            };
        }
        View::Maintenance(report) => {
            if code == KeyCode::Esc {
                return false;
            }
            if code == KeyCode::Enter && !report.applied {
                panel.view = View::Confirm {
                    text: format!(
                        "重新扫描并执行维护？预计归档 {} 个冷会话、删除 {} 个未引用产物；运行中或保留名单内的会话不处理。",
                        report.archived.len(),
                        report.removed.len()
                    ),
                    action: ConfirmAction::Run(ConfigAction::Maintenance { apply: true }),
                };
            } else {
                if code == KeyCode::Down {
                    panel.offset =
                        (panel.offset + 1).min(report.archived.len() + report.removed.len());
                }
                if code == KeyCode::Up {
                    panel.offset = panel.offset.saturating_sub(1);
                }
                panel.view = View::Maintenance(report);
            }
        }
        View::Fields => {
            if code == KeyCode::Esc {
                if panel.dirty() {
                    panel.view = View::Confirm {
                        text: "放弃尚未保存的配置草稿并关闭？".into(),
                        action: ConfirmAction::Close,
                    };
                } else {
                    return true;
                }
            } else if mods == KeyModifiers::CONTROL && code == KeyCode::Char('s') {
                if TABS[panel.tab] == "TUI" {
                    let result = (|| {
                        let mut preferences: TuiConfig = serde_json::from_value(panel.tui.clone())?;
                        preferences.statusline = preferences
                            .statusline
                            .filter(|text| !text.trim().is_empty());
                        let keys = KeyMap::compile(&preferences)?;
                        app.tui_store
                            .as_mut()
                            .ok_or_else(|| anyhow::anyhow!("TUI 存储尚未初始化"))?
                            .save(&preferences)?;
                        app.show_thinking = preferences.show_thinking;
                        app.tools_collapsed = !preferences.expand_tools;
                        app.statusline_template = preferences.statusline.clone();
                        panel.tui = serde_json::to_value(&preferences)?;
                        app.preferences = preferences;
                        app.keymap = keys;
                        app.view_gen = app.view_gen.wrapping_add(1);
                        Ok::<_, anyhow::Error>(())
                    })();
                    match result {
                        Ok(()) => {
                            panel.tui_original = panel.tui.clone();
                            panel.message = Some("TUI 偏好已保存并应用，不写入 core 配置".into());
                        }
                        Err(error) => panel.error = Some(error.to_string()),
                    }
                } else if let Some(data) = &panel.data {
                    panel.request(
                        connection,
                        &app.events,
                        ConfigAction::Save {
                            scope: panel.scope,
                            cwd: panel.cwd.clone(),
                            document: data.document.clone(),
                            revision: data.revision.clone(),
                        },
                        true,
                    );
                }
            } else if code == KeyCode::F(2) {
                if panel
                    .data
                    .as_ref()
                    .is_some_and(|data| data.document != panel.core_original)
                {
                    panel.error = Some("请先保存或重置 core 草稿再切换配置来源".into());
                } else {
                    panel.scope = match panel.scope {
                        ConfigScope::Global => ConfigScope::Project,
                        ConfigScope::Project => ConfigScope::Global,
                    };
                    panel.request(
                        connection,
                        &app.events,
                        ConfigAction::Read {
                            scope: panel.scope,
                            cwd: panel.cwd.clone(),
                        },
                        false,
                    );
                }
            } else if code == KeyCode::F(4) {
                panel.advanced = !panel.advanced;
                panel.filter_fields();
            } else if code == KeyCode::Tab || code == KeyCode::BackTab {
                let previous = mods.contains(KeyModifiers::SHIFT) || code == KeyCode::BackTab;
                panel.select_tab(if previous {
                    panel.tab.checked_sub(1).unwrap_or(TABS.len() - 1)
                } else {
                    (panel.tab + 1) % TABS.len()
                });
            } else if !panel.busy {
                match code {
                    KeyCode::Up => panel.selected = panel.selected.saturating_sub(1),
                    KeyCode::Down => {
                        panel.selected =
                            (panel.selected + 1).min(panel.visible.len().saturating_sub(1))
                    }
                    KeyCode::PageUp => panel.selected = panel.selected.saturating_sub(10),
                    KeyCode::PageDown => {
                        panel.selected =
                            (panel.selected + 10).min(panel.visible.len().saturating_sub(1))
                    }
                    KeyCode::Enter => panel.begin_edit(),
                    KeyCode::Char(' ') => {
                        if let Some(field) = panel.field().cloned() {
                            if matches!(field.kind, FieldKind::Bool) {
                                let value = !panel
                                    .value(&field.path)
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false);
                                panel.change(&field.path, json!(value));
                            } else {
                                panel.begin_edit();
                            }
                        }
                    }
                    KeyCode::Char('a') => panel.add(),
                    KeyCode::Char('f') => panel.fetch(connection, &app.events),
                    KeyCode::Char('w') if TABS[panel.tab] == "OpenViking" => {
                        panel.request(connection, &app.events, ConfigAction::Workspaces, false)
                    }
                    KeyCode::Char('g') if TABS[panel.tab] == "会话" => panel.request(
                        connection,
                        &app.events,
                        ConfigAction::Maintenance { apply: false },
                        false,
                    ),
                    KeyCode::Char('d') if mods == KeyModifiers::CONTROL => {
                        panel.begin_named(EditTarget::ModelDefaults)
                    }
                    KeyCode::Char('/') => panel.begin_named(EditTarget::Filter),
                    KeyCode::Char('r') => {
                        if let Some(field) = panel.field().cloned() {
                            if field.path[0] == "tui" {
                                let defaults =
                                    serde_json::to_value(TuiConfig::default()).expect("默认设置");
                                panel.change(
                                    &field.path,
                                    get(&defaults, &field.path[1..])
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                );
                            } else if let Some(data) = &mut panel.data {
                                remove(&mut data.document, &field.path);
                                panel.rebuild();
                            }
                        }
                    }
                    KeyCode::Delete => {
                        if let Some(field) = panel.field() {
                            let mut path = field.path.clone();
                            if path.first().is_some_and(|part| part == "providers") {
                                path.truncate(
                                    if path.get(2).is_some_and(|part| part == "models") {
                                        4
                                    } else {
                                        2
                                    },
                                );
                            } else if path.starts_with(&["mcp".into(), "servers".into()]) {
                                path.truncate(3);
                            }
                            panel.view = View::Confirm {
                                text: format!(
                                    "删除当前文件中的 {}？继承项不会从其他文件中删除。",
                                    path.join(" / ")
                                ),
                                action: ConfirmAction::Remove(path),
                            };
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    false
}

fn apply_edit(
    panel: &mut SettingsPanel,
    target: &EditTarget,
    kind: &FieldKind,
    text: &str,
    connection: &EmbeddedConnection,
    events: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
) -> anyhow::Result<()> {
    match target {
        EditTarget::Field(path) => {
            let kind = if path.starts_with(&["models".into(), "roles".into()]) {
                &FieldKind::Text
            } else {
                kind
            };
            let value = parse(text, kind)?;
            if value.is_null() {
                if path.first().is_some_and(|part| part == "tui") {
                    remove(&mut panel.tui, &path[1..]);
                } else if let Some(data) = &mut panel.data {
                    remove(&mut data.document, path);
                }
                panel.rebuild();
            } else {
                panel.change(path, value);
            }
        }
        EditTarget::Provider => {
            let name = text.trim();
            anyhow::ensure!(
                !name.is_empty() && !name.contains(['/', '\n']),
                "供应商标识不能为空，也不能包含斜线或换行"
            );
            anyhow::ensure!(
                panel
                    .effective
                    .get("providers")
                    .and_then(|providers| providers.get(name))
                    .is_none(),
                "供应商 {name} 已存在"
            );
            panel.change(
                &["providers".into(), name.into()],
                json!({"api":"openai_chat","base_url":"","api_key":"","models":{}}),
            );
            let index = panel.visible.iter().position(|index| {
                panel.fields[*index]
                    .path
                    .get(1)
                    .is_some_and(|part| part == name)
            });
            if let Some(index) = index {
                panel.selected = index;
            }
            panel.message =
                Some("供应商已加入草稿；填写地址和密钥后按 F 获取模型，或到模型页手动添加".into());
        }
        EditTarget::Model(provider) => {
            panel.add_model(provider, text.trim())?;
        }
        EditTarget::Mcp => {
            let name = text.trim();
            moc_mcp_name(name)?;
            let path = vec!["mcp".into(), "servers".into(), name.into()];
            anyhow::ensure!(panel.value(&path).is_none(), "同名 MCP 已存在");
            panel.change(&path,json!({"type":"stdio","command":"","args":[],"env":{},"headers":{},"enabled":true,"tools":{"allow":[],"deny":[]}}));
        }
        EditTarget::ModelDefaults => {
            let pattern = text.trim();
            anyhow::ensure!(!pattern.is_empty(), "模型匹配模式不能为空");
            panel.change(
                &["defaults".into(), "models".into(), pattern.into()],
                json!({"context_window":200000,"max_output_tokens":32000,"reasoning":false}),
            );
            panel.select_tab(TABS.len() - 1);
        }
        EditTarget::Rebind(binding) => panel.request(
            connection,
            events,
            ConfigAction::RebindWorkspace {
                url: binding.url.clone(),
                path: binding.path.clone(),
                new_path: text.trim().into(),
            },
            false,
        ),
        EditTarget::Filter => {
            panel.filter = text.trim().into();
            panel.filter_fields();
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

fn edit_key(input: &mut PromptState, mods: KeyModifiers, code: KeyCode) {
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
