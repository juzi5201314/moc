use rust_i18n::t;
use super::AllowedExecution;
use super::Authorization;
use super::PermissionAnswer;
use super::WRITE_PREVIEW_BYTES;
use super::WRITE_PREVIEW_LINES;
use crate::error::CoreError;
use crate::tools::ToolControl;
use crate::tools::ToolCtx;
use crate::tools::ToolOutcome;
use moc_protocol::methods::SessionUpdate;
use moc_protocol::methods::ToolStatus;
use serde_json::Value;

/// 授权被拒时的统一工具结果；调用方保证只传入非 `Allowed` 结果。
pub(super) fn denied_outcome(authorization: Authorization) -> ToolOutcome {
    let (code, message) = match authorization {
        Authorization::Denied => (
            "E_PERMISSION_DENIED",
            "The user denied this operation".to_owned(),
        ),
        Authorization::SandboxDenied(message) => ("E_SANDBOX_DENIED", message),
        Authorization::Unavailable => (
            "E_PERMISSION_UNAVAILABLE",
            "This frontend cannot show permission prompts; approve from an interactive frontend or switch the session permission".to_owned(),
        ),
        Authorization::Allowed(_) => unreachable!("denied_outcome 不处理允许结果"),
    };
    ToolOutcome {
        content: format!("[{code}] {message}").into(),
        is_error: true,
        structured_content: None,
        error_code: Some(code.into()),
        images: Vec::new(),
        control: ToolControl::Continue,
        status: Some(ToolStatus::Denied),
        diff: None,
    }
}

/// 危险规则命中后向用户申请确认；`allow_once` 只放本次，`allow_session` 按完整命令
/// 文本记入会话豁免。批准后命令继续走常规沙盒授权，危险确认不提升权限模式。
pub(super) async fn confirm_dangerous(
    ctx: &ToolCtx,
    tool_call_id: &str,
    command: &str,
    pattern: &str,
) -> Result<Authorization, CoreError> {
    if ctx.config.sandbox.on_denied == crate::config::OnDenied::Deny {
        return Ok(Authorization::SandboxDenied(
            "sandbox.on_denied=deny；命令命中危险规则".into(),
        ));
    }
    let body = format!(
        "命令：\n{command}\n\n命中危险命令规则：\n{pattern}\n\n批准后命令仍按当前权限模式执行。",
    );
    match ask_permission(
        ctx,
        tool_call_id,
        "bash",
        format!(
            "命令命中危险规则：{}",
            command.lines().next().unwrap_or(command)
        ),
        Some(body),
        None,
        moc_protocol::methods::command_options(None),
    )
    .await?
    {
        PermissionAnswer::Unavailable => Ok(Authorization::Unavailable),
        PermissionAnswer::Dismissed => Ok(Authorization::Denied),
        PermissionAnswer::Selected(option) if option == "allow_once" => {
            Ok(Authorization::Allowed(AllowedExecution::current(ctx)))
        }
        PermissionAnswer::Selected(option) if option == "allow_session" => {
            ctx.sandbox_grants.write().grant_dangerous(command);
            Ok(Authorization::Allowed(AllowedExecution::current(ctx)))
        }
        PermissionAnswer::Selected(_) => Ok(Authorization::Denied),
    }
}

/// 授权入口：`full` 不询问（yolo）；其余模式按沙盒策略检查读/写路径，bash 的
/// 升级由模型显式带 `sandbox_permissions` + `justification` 发起。
pub(super) async fn authorize(
    ctx: &ToolCtx,
    tool_call_id: &str,
    tool_name: &str,
    arguments: &Value,
) -> Result<Authorization, CoreError> {
    let policy = ctx.sandbox_policy();
    match tool_name {
        "bash" => authorize_bash(ctx, tool_call_id, arguments).await,
        "read" | "find" | "grep" => {
            authorize_read(ctx, tool_call_id, tool_name, arguments, &policy).await
        }
        "write" | "edit" => authorize_write(ctx, tool_call_id, tool_name, arguments, &policy).await,
        _ => Ok(Authorization::Allowed(AllowedExecution::current(ctx))),
    }
}

pub(super) async fn authorize_read(
    ctx: &ToolCtx,
    tool_call_id: &str,
    tool_name: &str,
    arguments: &Value,
    policy: &crate::sandbox::SandboxPolicy,
) -> Result<Authorization, CoreError> {
    let Some(target) = arguments.get("path").and_then(Value::as_str) else {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    };
    if crate::resources::is_resource_uri(target) {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    }
    let path = absolute_target(&ctx.cwd, std::path::Path::new(target));
    match policy.check(&path, crate::sandbox::Access::Read) {
        crate::sandbox::Decision::Allow => {
            Ok(Authorization::Allowed(AllowedExecution::current(ctx)))
        }
        crate::sandbox::Decision::Deny => Ok(Authorization::SandboxDenied(format!(
            "read access to {} is denied by sandbox.deny_read_paths",
            path.display()
        ))),
        crate::sandbox::Decision::Ask => {
            request_paths(
                ctx,
                tool_call_id,
                tool_name,
                vec![path],
                crate::sandbox::Access::Read,
                t!("core.err.sandbox_read", target = target).into_owned(),
                None,
                None,
            )
            .await
        }
    }
}

pub(super) fn absolute_target(
    cwd: &std::path::Path,
    target: &std::path::Path,
) -> std::path::PathBuf {
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        cwd.join(target)
    }
}

/// 找出 write/edit 的全部目标；解析失败留给工具自己返回 `E_BAD_PAYLOAD`。
pub(super) fn write_targets(
    cwd: &std::path::Path,
    tool_name: &str,
    arguments: &Value,
) -> Result<Vec<std::path::PathBuf>, crate::tools::ToolError> {
    let targets: Vec<String> = if tool_name == "write" {
        vec![crate::tools::write::parse_args(arguments.clone())?.path]
    } else {
        let request = crate::tools::edit::parse_requests(arguments.clone())?;
        std::iter::once(request.path)
            .chain(
                request
                    .move_lines
                    .into_iter()
                    .map(|movement| movement.target_path),
            )
            .collect()
    };
    Ok(targets
        .into_iter()
        .filter(|target| !crate::resources::is_resource_uri(target))
        .map(|target| absolute_target(cwd, std::path::Path::new(&target)))
        .collect())
}

pub(super) async fn authorize_write(
    ctx: &ToolCtx,
    tool_call_id: &str,
    tool_name: &str,
    arguments: &Value,
    policy: &crate::sandbox::SandboxPolicy,
) -> Result<Authorization, CoreError> {
    let targets = match write_targets(&ctx.cwd, tool_name, arguments) {
        Ok(targets) => targets,
        Err(_) => return Ok(Authorization::Allowed(AllowedExecution::current(ctx))),
    };
    let mut outside = Vec::new();
    for target in targets {
        match policy.check(&target, crate::sandbox::Access::Write) {
            crate::sandbox::Decision::Allow => {}
            crate::sandbox::Decision::Deny => {
                return Ok(Authorization::SandboxDenied(format!(
                    "write access to {} is denied by sandbox.deny_write_paths",
                    target.display()
                )));
            }
            crate::sandbox::Decision::Ask => outside.push(target),
        }
    }
    if outside.is_empty() {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    }
    let paths = outside
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let (diff, content) = match tool_name {
        "write" => match crate::tools::write::parse_args(arguments.clone()) {
            Ok(args) => (
                crate::tools::write::preview_arguments(ctx, &args).await,
                Some(args.content),
            ),
            Err(_) => (None, None),
        },
        _ => (crate::tools::edit::preview_arguments(ctx, arguments), None),
    };
    let mut body = format!(
        "以下写入目标在沙盒允许范围之外：\n{paths}\n\n当前权限：{}。允许后本次写入会真实落盘。",
        ctx.permission.as_str()
    );
    if let Some(content) = &content {
        body.push_str(&format!(
            "\n\n将写入的内容（前 {WRITE_PREVIEW_LINES} 行，共 {} 字节）：\n{}",
            content.len(),
            write_preview(content)
        ));
    }
    let title = format!(
        "允许在工作区外写入：{}",
        outside
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    request_paths(
        ctx,
        tool_call_id,
        tool_name,
        outside,
        crate::sandbox::Access::Write,
        title,
        Some(body),
        diff,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn request_paths(
    ctx: &ToolCtx,
    tool_call_id: &str,
    tool_name: &str,
    targets: Vec<std::path::PathBuf>,
    access: crate::sandbox::Access,
    title: String,
    body: Option<String>,
    diff: Option<moc_protocol::methods::ToolDiff>,
) -> Result<Authorization, CoreError> {
    if ctx.config.sandbox.on_denied == crate::config::OnDenied::Deny {
        return Ok(Authorization::SandboxDenied(format!(
            "sandbox.on_denied=deny；已拒绝以下路径的访问：\n{}",
            targets
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        )));
    }
    match ask_permission(
        ctx,
        tool_call_id,
        tool_name,
        title,
        body,
        diff,
        moc_protocol::methods::path_options(),
    )
    .await?
    {
        PermissionAnswer::Unavailable => Ok(Authorization::Unavailable),
        PermissionAnswer::Dismissed => Ok(Authorization::Denied),
        PermissionAnswer::Selected(option) if option == "allow_once" => {
            let mut grants = crate::sandbox::SandboxGrants::default();
            for target in &targets {
                grants.grant_path(target, access);
            }
            Ok(Authorization::Allowed(AllowedExecution {
                permission: ctx.permission,
                call_grants: grants,
            }))
        }
        PermissionAnswer::Selected(option) if option == "allow_session" => {
            let mut grants = ctx.sandbox_grants.write();
            for target in &targets {
                grants.grant_path(target, access);
            }
            Ok(Authorization::Allowed(AllowedExecution::current(ctx)))
        }
        PermissionAnswer::Selected(_) => Ok(Authorization::Denied),
    }
}

pub(super) async fn authorize_bash(
    ctx: &ToolCtx,
    tool_call_id: &str,
    arguments: &Value,
) -> Result<Authorization, CoreError> {
    let Some(command) = arguments.get("command").and_then(Value::as_str) else {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    };
    let target = arguments
        .get("sandbox_permissions")
        .and_then(Value::as_str)
        .and_then(parse_permission);
    let Some(target) = target else {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    };
    let Some(justification) = arguments.get("justification").and_then(Value::as_str) else {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    };
    if justification.trim().is_empty() || justification.len() > 500 {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    }
    // 非法或变窄的目标交给 bash 工具返回 E_BAD_PAYLOAD，不在授权层重复报错。
    if target == moc_protocol::SessionPermission::WorkspaceOnly
        || target.width() <= ctx.permission.width()
    {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    }
    if ctx.config.sandbox.on_denied == crate::config::OnDenied::Deny {
        return Ok(Authorization::SandboxDenied(format!(
            "sandbox.on_denied=deny；命令不能升级到 {} 执行",
            target.as_str()
        )));
    }
    if target == moc_protocol::SessionPermission::Full && !ctx.config.sandbox.allow_unsandboxed {
        return Ok(Authorization::SandboxDenied(
            "sandbox.allow_unsandboxed=false；不能申请沙盒外执行".into(),
        ));
    }
    if let Some(granted) = ctx.sandbox_grants.read().mode_for_command(command)
        && granted.width() >= target.width()
    {
        return Ok(Authorization::Allowed(AllowedExecution {
            permission: target,
            call_grants: crate::sandbox::SandboxGrants::default(),
        }));
    }

    let prefix = crate::sandbox::command_prefix(command);
    let body = format!(
        "命令：\n{command}\n\n理由：\n{justification}\n\n当前权限：{}；申请权限：{}。",
        ctx.permission.as_str(),
        target.as_str()
    );
    match ask_permission(
        ctx,
        tool_call_id,
        "bash",
        format!(
            "允许以 {} 执行命令：{}",
            target.as_str(),
            command.lines().next().unwrap_or(command)
        ),
        Some(body),
        None,
        moc_protocol::methods::command_options(prefix.as_deref()),
    )
    .await?
    {
        PermissionAnswer::Unavailable => Ok(Authorization::Unavailable),
        PermissionAnswer::Dismissed => Ok(Authorization::Denied),
        PermissionAnswer::Selected(option) if option == "allow_once" => {
            Ok(Authorization::Allowed(AllowedExecution {
                permission: target,
                call_grants: crate::sandbox::SandboxGrants::default(),
            }))
        }
        PermissionAnswer::Selected(option) if option == "allow_session" => {
            ctx.sandbox_grants
                .write()
                .grant_command(command, false, target);
            Ok(Authorization::Allowed(AllowedExecution {
                permission: target,
                call_grants: crate::sandbox::SandboxGrants::default(),
            }))
        }
        PermissionAnswer::Selected(option)
            if option == "allow_session_prefix" && prefix.is_some() =>
        {
            ctx.sandbox_grants.write().grant_command(
                prefix.as_deref().unwrap_or_default(),
                true,
                target,
            );
            Ok(Authorization::Allowed(AllowedExecution {
                permission: target,
                call_grants: crate::sandbox::SandboxGrants::default(),
            }))
        }
        PermissionAnswer::Selected(_) => Ok(Authorization::Denied),
    }
}

pub(super) fn parse_permission(value: &str) -> Option<moc_protocol::SessionPermission> {
    match value {
        "full" => Some(moc_protocol::SessionPermission::Full),
        "workspace" => Some(moc_protocol::SessionPermission::Workspace),
        "workspace_only" | "workspace-only" => Some(moc_protocol::SessionPermission::WorkspaceOnly),
        _ => None,
    }
}

/// 生成写入内容预览：取开头若干行，超出上限时注明已截断。
pub(super) fn write_preview(content: &str) -> String {
    let mut preview = String::new();
    let mut bytes = 0usize;
    for (line_index, line) in content.lines().enumerate() {
        let width = line.len() + 1;
        if line_index >= WRITE_PREVIEW_LINES || bytes + width > WRITE_PREVIEW_BYTES {
            preview.push_str("\n[...]");
            break;
        }
        preview.push_str(line);
        preview.push('\n');
        bytes += width;
    }
    preview
}

/// 发起一次权限询问；前端不支持时返回 Unavailable，用户关闭弹窗按 Dismissed。
pub(super) async fn ask_permission(
    ctx: &ToolCtx,
    tool_call_id: &str,
    tool_name: &str,
    title: String,
    body: Option<String>,
    diff: Option<moc_protocol::methods::ToolDiff>,
    options: Vec<moc_protocol::methods::PermissionOption>,
) -> Result<PermissionAnswer, CoreError> {
    let request = &ctx.request;
    let request_id = crate::ids::entry_id();
    // 插件有机会在询问用户之前代答，但只能代替一次：会话级授权仍由人选择。
    let plugin_context = ctx.plugin_call_context();
    let event = crate::service::plugin_events::permission_request_payload(
        &ctx.prompt_id,
        &request_id,
        tool_call_id,
        tool_name,
        &title,
        body.as_deref(),
        &options,
    );
    if let Some(option) = ctx
        .plugins
        .permission_requested(&event, &plugin_context)
        .await
    {
        crate::service::plugin_events::permission_resolved(
            &ctx.plugins,
            &plugin_context,
            &ctx.prompt_id,
            &request_id,
            Some(&option),
        )
        .await;
        return Ok(PermissionAnswer::Selected(option));
    }
    if !ctx
        .core
        .capabilities
        .read()
        .get(&request.connection_id())
        .is_some_and(|caps| caps.permission_requests)
    {
        return Ok(PermissionAnswer::Unavailable);
    }
    request
        .updates()
        .emit(
            ctx.session_id.clone(),
            SessionUpdate::PermissionRequested {
                prompt_id: ctx.prompt_id.clone(),
                request_id: request_id.clone(),
                tool_call_id: tool_call_id.to_owned(),
            },
        )
        .await
        .map_err(|error| CoreError::Protocol(error.to_string()))?;
    let answer = request
        .ask_permission(moc_protocol::methods::PermissionRequestParams {
            session_id: ctx.session_id.clone(),
            prompt_id: ctx.prompt_id.clone(),
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            title,
            body,
            diff,
            options,
        })
        .await;
    let selected = answer.ok().map(|answer| answer.option_id);
    request
        .updates()
        .emit(
            ctx.session_id.clone(),
            SessionUpdate::PermissionResolved {
                prompt_id: ctx.prompt_id.clone(),
                request_id: request_id.clone(),
                option_id: selected.clone(),
            },
        )
        .await
        .map_err(|error| CoreError::Protocol(error.to_string()))?;
    crate::service::plugin_events::permission_resolved(
        &ctx.plugins,
        &plugin_context,
        &ctx.prompt_id,
        &request_id,
        selected.as_deref(),
    )
    .await;
    Ok(match selected {
        None => PermissionAnswer::Dismissed,
        Some(option) => PermissionAnswer::Selected(option),
    })
}
