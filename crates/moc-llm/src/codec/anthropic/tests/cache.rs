use super::*;

#[test]
fn static_system_block_takes_its_own_cache_breakpoint() {
    let system = Message {
        role: Role::System,
        content: crate::message_content![
            ContentPart::Text("模板".into()),
            ContentPart::Text("项目".into()),
        ],
    };
    // 无断点：多块系统提示拍平成空行分隔的字符串，与单块形态逐字一致。
    let plain = ChatRequest {
        messages: vec![system.clone(), Message::user_text("hi")],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &plain, false);
    assert_eq!(body["system"], json!("模板\n\n项目"));

    // 消息断点落在系统末块，静态块断点落在第 0 块，两者可以同时存在。
    let req = ChatRequest {
        messages: vec![system, Message::user_text("hi")],
        options: RequestOptions {
            cache: Some(CacheConfig {
                breakpoints: vec![0],
                instruction_blocks: vec![InstructionBlock {
                    message: 0,
                    block: 0,
                }],
                ..Default::default()
            }),
            ..Default::default()
        },
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["system"],
        json!([
            {"type": "text", "text": "模板", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "项目", "cache_control": {"type": "ephemeral"}},
        ])
    );

    // 越界的静态块下标报错，而不是静默丢断点。
    let bad = ChatRequest {
        messages: vec![Message::system("只有一块"), Message::user_text("hi")],
        options: RequestOptions {
            cache: Some(CacheConfig {
                instruction_blocks: vec![InstructionBlock {
                    message: 0,
                    block: 3,
                }],
                ..Default::default()
            }),
            ..Default::default()
        },
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let err = encode(&config(), &bad, false).unwrap_err();
    assert!(err.to_string().contains("越界"), "{err}");
}

#[test]
fn user_message_blocks_keep_cache_breakpoints_without_system() {
    // 没有系统提示时，user 内容块仍可设置独立缓存断点。
    let req = ChatRequest {
        messages: vec![
            Message {
                role: Role::User,
                content: crate::message_content![
                    ContentPart::Text("工具政策".into()),
                    ContentPart::Text("仓库规则".into()),
                ],
            },
            Message::user_text("hi"),
        ],
        options: RequestOptions {
            cache: Some(CacheConfig {
                breakpoints: vec![0],
                instruction_blocks: vec![InstructionBlock {
                    message: 0,
                    block: 0,
                }],
                ..Default::default()
            }),
            ..Default::default()
        },
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    // 相邻 user 消息合并：静态前缀断点落在第 0 块，消息断点落在指令末块上。
    assert!(body.get("system").is_none());
    assert_eq!(
        body["messages"],
        json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "工具政策", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "仓库规则", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "hi"},
            ],
        }])
    );
}

#[test]
fn system_and_user_messages_keep_independent_cache_breakpoints() {
    // 系统边界断点与 user 内容块断点互不干扰，相邻 user 消息合并后仍保留位置。
    let req = ChatRequest {
        messages: vec![
            Message {
                role: Role::System,
                content: crate::message_content![
                    ContentPart::Text("身份".into()),
                    ContentPart::Text("环境".into()),
                ],
            },
            Message {
                role: Role::User,
                content: crate::message_content![
                    ContentPart::Text("工具政策".into()),
                    ContentPart::Text("仓库规则".into()),
                ],
            },
            Message::user_text("hi"),
        ],
        options: RequestOptions {
            cache: Some(CacheConfig {
                breakpoints: vec![0, 2],
                instruction_blocks: vec![InstructionBlock {
                    message: 1,
                    block: 0,
                }],
                ..Default::default()
            }),
            ..Default::default()
        },
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["system"],
        json!([
            {"type": "text", "text": "身份"},
            {"type": "text", "text": "环境", "cache_control": {"type": "ephemeral"}},
        ])
    );
    assert_eq!(
        body["messages"],
        json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "工具政策", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "仓库规则"},
                {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}},
            ],
        }])
    );
}

#[test]
fn cache_control_top_level_and_breakpoints() {
    // 无断点：cache_control 落在顶层 system 的最后一块。
    let req = ChatRequest {
        messages: vec![Message::system("系统提示"), Message::user_text("hi")],
        options: RequestOptions {
            cache: Some(CacheConfig::default()),
            ..Default::default()
        },
        ..base_request()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "系统提示", "cache_control": {"type": "ephemeral"}}])
    );

    // 有断点：落在该 IR 消息的最后一个内容块上（跨合并仍然正确）。
    let req = ChatRequest {
        messages: vec![
            Message::system("系统提示"),
            Message::user_text("先查"),
            assistant_with_tools(&[("call_1", "get_weather")]),
            Message::tool_result("call_1", "晴", false),
            Message::user_text("继续"),
        ],
        options: RequestOptions {
            cache: Some(CacheConfig {
                breakpoints: vec![4],
                ttl: Some(CacheTtl::OneHour),
                ..Default::default()
            }),
            ..Default::default()
        },
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(body["system"], json!("系统提示"));
    let content = body["messages"][2]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(
        content[1],
        json!({"type": "text", "text": "继续", "cache_control": {"type": "ephemeral", "ttl": "1h"}})
    );
    assert!(content[0].get("cache_control").is_none());

    // 1h TTL 自动补 beta 头，且不覆盖 core 的显式配置。
    let headers = headers_of(&config(), &req);
    assert!(
        headers.contains(&("anthropic-beta".to_string(), EXTENDED_TTL_BETA.to_string())),
        "{headers:?}"
    );

    // 不支持的 TTL 报错。
    let bad = ChatRequest {
        messages: vec![Message::system("s"), Message::user_text("hi")],
        options: RequestOptions {
            cache: Some(CacheConfig {
                ttl: Some(CacheTtl::ThirtyMinutes),
                ..Default::default()
            }),
            ..Default::default()
        },
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let err = encode(&config(), &bad, false).unwrap_err();
    assert!(err.to_string().contains("只支持 5m 与 1h"), "{err}");

    // 断点指向不存在的线上块时报错。
    let bad = ChatRequest {
        messages: vec![Message::system("s"), Message::user_text("hi")],
        options: RequestOptions {
            cache: Some(CacheConfig {
                breakpoints: vec![9],
                ..Default::default()
            }),
            ..Default::default()
        },
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let err = encode(&config(), &bad, false).unwrap_err();
    assert!(err.to_string().contains("没有对应的线上内容块"), "{err}");

    // 无系统提示且无断点：退到首条消息的最后一块，而不是报错。
    let plain = ChatRequest {
        messages: vec![Message::user_text("hi")],
        options: RequestOptions {
            cache: Some(CacheConfig::default()),
            ..Default::default()
        },
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &plain, false);
    assert!(body.get("system").is_none());
    assert_eq!(
        body["messages"],
        json!([{
            "role": "user",
            "content": [{
                "type": "text",
                "text": "hi",
                "cache_control": {"type": "ephemeral"},
            }],
        }])
    );

    // 指令块断点指向不存在的指令消息时报错，而不是静默丢弃断点。
    let bad = ChatRequest {
        messages: vec![Message::user_text("hi")],
        options: RequestOptions {
            cache: Some(CacheConfig {
                instruction_blocks: vec![InstructionBlock {
                    message: 5,
                    block: 0,
                }],
                ..Default::default()
            }),
            ..Default::default()
        },
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let err = encode(&config(), &bad, false).unwrap_err();
    assert!(err.to_string().contains("没有对应的指令消息"), "{err}");
}

#[test]
fn cache_breakpoint_on_hoisted_system_message_lands_on_system_boundary() {
    // core 对 Anthropic 恒定给出 `[0, …]`，而 `messages[0]` 是系统提示：
    // 断点落在被上提的消息上时必须落在 `system` 边界，而不是报「没有对应的
    // 线上内容块」——否则每个请求都在本地编码阶段失败。
    let req = ChatRequest {
        messages: vec![
            Message::system("系统提示"),
            Message::developer("补充规则"),
            Message::user_text("hi"),
        ],
        options: RequestOptions {
            cache: Some(CacheConfig {
                breakpoints: vec![0, 2],
                ttl: Some(CacheTtl::FiveMinutes),
                ..Default::default()
            }),
            ..Default::default()
        },
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    // 系统与开发者消息各自成块，断点按「系统边界」语义落在最后一块上。
    assert_eq!(
        body["system"],
        json!([
            {"type": "text", "text": "系统提示"},
            {
                "type": "text",
                "text": "补充规则",
                "cache_control": {"type": "ephemeral", "ttl": "5m"},
            }
        ])
    );
    assert_eq!(
        body["messages"],
        json!([{
            "role": "user",
            "content": [{
                "type": "text",
                "text": "hi",
                "cache_control": {"type": "ephemeral", "ttl": "5m"},
            }],
        }])
    );

    // 断点落在系统提示上但系统提示为空：没有可落位的块，报错而不是静默丢弃。
    let bad = ChatRequest {
        messages: vec![Message::system(""), Message::user_text("hi")],
        options: RequestOptions {
            cache: Some(CacheConfig {
                breakpoints: vec![0],
                ..Default::default()
            }),
            ..Default::default()
        },
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let err = encode(&config(), &bad, false).unwrap_err();
    assert!(err.to_string().contains("系统提示为空"), "{err}");
}
