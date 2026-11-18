use super::Input;
use super::display::parse_engine;
use super::rpc_io_error;
use anyhow::bail;
use moc_protocol::ClientHandle;
use moc_protocol::RpcError;
use moc_protocol::RpcErrorCode;
use moc_protocol::methods::AskAnswer;
use moc_protocol::methods::AskResult;
use moc_protocol::methods::InputPart;
use moc_protocol::methods::PermissionRequestParams;
use moc_protocol::methods::PermissionResult;
use moc_protocol::methods::PlanAction;
use moc_protocol::methods::PlanStatus;
use moc_protocol::methods::SessionCancel;
use moc_protocol::methods::SessionCancelParams;
use moc_protocol::methods::SessionInfo;
use moc_protocol::methods::SessionLoad;
use moc_protocol::methods::SessionLoadParams;
use moc_protocol::methods::SessionPlanApprove;
use moc_protocol::methods::SessionPlanApproveParams;
use moc_protocol::methods::SessionPrompt;
use moc_protocol::methods::SessionRetry;
use moc_protocol::methods::SessionRetryParams;
use moc_protocol::methods::prompt::PromptMode;
use rust_i18n::t;
use std::io::Write;

pub(super) async fn approve_plan(
    client: &ClientHandle,
    command: &str,
    session: &mut SessionInfo,
) -> anyhow::Result<()> {
    let load = client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: session.id.clone(),
            max_entries: None,
            read_only: false,
        })
        .await?;
    let plan = load
        .plans
        .iter()
        .rev()
        .find(|plan| plan.status == PlanStatus::Submitted)
        .ok_or_else(|| anyhow::anyhow!("{}", t!("cli.inter.no_plan")))?;
    // `new` 可带目标引擎：`/approve new ptc`；缺省沿用当前会话引擎。
    let (verb, engine_arg) = command
        .split_once(' ')
        .map_or((command, ""), |(v, rest)| (v, rest.trim()));
    let (action, execution_engine) = match verb {
        "execute" if engine_arg.is_empty() => (PlanAction::Execute, None),
        "compact" if engine_arg.is_empty() => (PlanAction::ExecuteCompacted, None),
        "new" => (
            PlanAction::ExecuteNewSession,
            if engine_arg.is_empty() {
                None
            } else {
                Some(parse_engine(engine_arg)?)
            },
        ),
        "reject" if engine_arg.is_empty() => (PlanAction::Reject, None),
        _ => bail!("{}", t!("cli.cmd.usage_approve")),
    };
    let approved = client
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: session.id.clone(),
            plan_id: plan.plan_id.clone(),
            action,
            execution_engine,
        })
        .await?;
    let next = client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: approved.session_id,
            max_entries: None,
            read_only: false,
        })
        .await?;
    *session = next.session;
    if let Some(markdown) = approved.plan_markdown {
        println!("\n{markdown}\n");
    }
    if let Some(prompt) = approved.auto_prompt {
        send_prompt(client, &session.id, prompt, None, None).await?;
    }
    Ok(())
}

pub(super) async fn send_prompt(
    client: &ClientHandle,
    session_id: &str,
    prompt: String,
    mode: Option<PromptMode>,
    max_turns: Option<u32>,
) -> anyhow::Result<moc_protocol::methods::PromptResult> {
    let pending = client.call::<SessionPrompt>(moc_protocol::methods::PromptParams {
        session_id: session_id.to_owned(),
        input: vec![InputPart::Text { text: prompt }],
        mode,
        agent_message: None,
        goal_round: None,
        max_turns,
    });
    tokio::pin!(pending);
    tokio::select! {
        result = &mut pending => result.map_err(Into::into),
        signal = tokio::signal::ctrl_c() => {
            signal?;
            client.call::<SessionCancel>(SessionCancelParams { session_id: session_id.to_owned() }).await?;
            pending.await.map_err(Into::into)
        }
    }
}

/// `/retry`：接着现有上下文继续跑，不追加用户消息。
///
/// 上一轮没跑完（取消、上游中断）时，core 会先把那半截回合从会话里剪掉再继续，
/// 所以这里只管发请求、等它结束。
pub(super) async fn send_retry(client: &ClientHandle, session_id: &str) -> anyhow::Result<()> {
    let pending = client.call::<SessionRetry>(SessionRetryParams {
        session_id: session_id.to_owned(),
    });
    tokio::pin!(pending);
    tokio::select! {
        result = &mut pending => result.map(|_| ()).map_err(Into::into),
        signal = tokio::signal::ctrl_c() => {
            signal?;
            client.call::<SessionCancel>(SessionCancelParams { session_id: session_id.to_owned() }).await?;
            pending.await.map(|_| ()).map_err(Into::into)
        }
    }
}

pub(super) async fn send_bash(
    client: &ClientHandle,
    session_id: &str,
    command: moc_protocol::BashCommand,
) -> anyhow::Result<()> {
    let pending = client.call::<moc_protocol::SessionBash>(moc_protocol::BashParams {
        session_id: session_id.to_owned(),
        command,
    });
    tokio::pin!(pending);
    tokio::select! {
        result = &mut pending => result.map(|_| ()).map_err(Into::into),
        signal = tokio::signal::ctrl_c() => {
            signal?;
            client.call::<SessionCancel>(SessionCancelParams { session_id: session_id.to_owned() }).await?;
            pending.await.map(|_| ()).map_err(Into::into)
        }
    }
}

pub(super) async fn permission_prompt(
    input: Input,
    params: PermissionRequestParams,
) -> Result<PermissionResult, RpcError> {
    println!("\n{}", params.title);
    if let Some(body) = params.body {
        println!("{body}");
    }
    for (index, option) in params.options.iter().enumerate() {
        println!("  {}. {}", index + 1, option.label);
    }
    print!("{}", t!("cli.inter.choose"));
    let _ = std::io::stdout().flush();
    let line = read_line(&input)
        .await
        .map_err(rpc_io_error)?
        .unwrap_or_default();
    let option = line
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| params.options.get(index))
        .or_else(|| {
            params
                .options
                .iter()
                .find(|option| option.id == "reject_once")
        })
        .ok_or_else(|| RpcError::of(RpcErrorCode::InvalidParams))?;
    Ok(PermissionResult {
        option_id: option.id.clone(),
    })
}

pub(super) async fn ask_prompt(
    input: Input,
    params: moc_protocol::methods::AskParams,
) -> Result<AskResult, RpcError> {
    let mut answers = Vec::with_capacity(params.questions.len());
    for question in params.questions {
        println!(
            "\n{}",
            question.header.as_deref().unwrap_or(&question.question)
        );
        if question.header.is_some() {
            println!("{}", question.question);
        }
        for (index, option) in question.options.iter().enumerate() {
            let recommended = question.recommended.contains(&(index as u32));
            println!(
                "  {}. {}{}",
                index + 1,
                option.label,
                if recommended {
                    t!("cli.inter.recommended").into_owned()
                } else {
                    String::new()
                }
            );
            if let Some(description) = &option.description {
                println!("     {description}");
            }
            if let Some(preview) = &option.preview {
                // CLI 不渲染 markdown：预览按原样缩进打印，保住 ASCII 示意图的对齐。
                for row in preview.lines() {
                    println!("     {row}");
                }
            }
        }
        print!(
            "{}",
            t!(
                "cli.inter.input_number",
                multi = if question.multi {
                    t!("cli.inter.multi_hint").into_owned()
                } else {
                    String::new()
                }
            )
        );
        let _ = std::io::stdout().flush();
        let line = read_line(&input)
            .await
            .map_err(rpc_io_error)?
            .unwrap_or_default();
        let raw = line.trim();
        let mut selected = Vec::new();
        let mut custom = None;
        if raw.is_empty() {
            for option in question
                .recommended
                .iter()
                .filter_map(|&index| question.options.get(index as usize))
            {
                selected.push(option.label.clone());
            }
        } else {
            let choices: Vec<_> = if question.multi {
                raw.split(',').map(str::trim).collect()
            } else {
                vec![raw]
            };
            for choice in &choices {
                if let Ok(index) = choice.parse::<usize>()
                    && let Some(option) = index
                        .checked_sub(1)
                        .and_then(|index| question.options.get(index))
                {
                    selected.push(option.label.clone());
                } else {
                    custom = Some(raw.to_owned());
                    selected.clear();
                    break;
                }
            }
        }
        answers.push(AskAnswer {
            id: question.id,
            selected,
            custom,
        });
    }
    Ok(AskResult { answers })
}

pub(super) async fn read_line(input: &Input) -> std::io::Result<Option<String>> {
    input.lock().await.recv().await.transpose()
}
