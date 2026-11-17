use super::*;
use moc_protocol::{
    ClientBuilder, ClientInfo, ConfigAction, ConfigManage, ConfigReply, ConfigScope, Message,
    PlanAction, Server, SessionNew, SessionPlanApprove, SessionRewind, transport,
};
use moc_protocol::{SessionNewParams, SessionPlanApproveParams, SessionRewindParams};
use serde_json::json;
use std::path::Path;

#[tokio::test]
async fn project_compression_is_selected_per_creation_and_after_config_save() {
    for global in [false, true] {
        let root = tempfile::tempdir().expect("临时目录");
        let home = root.path().join("home");
        let plain = root.path().join("plain");
        let compressed = root.path().join("compressed");
        std::fs::create_dir_all(&home).unwrap();
        for (cwd, enabled) in [(&plain, false), (&compressed, true)] {
            std::fs::create_dir_all(cwd.join(".moc")).unwrap();
            std::fs::write(
                cwd.join(".moc/config.toml"),
                format!("[session]\ncompress={enabled}\n"),
            )
            .unwrap();
        }
        std::fs::write(home.join("config.toml"), format!(
            "language='zh'\n[models.roles]\ndefault='p/m'\n[providers.p]\napi='openai_chat'\nbase_url='http://127.0.0.1:1'\n[providers.p.models.m]\n[session]\ncompress={global}\n"
        )).unwrap();
        let core = Core::new_with_sandbox_support(
            CoreConfig {
                home,
                cwd: plain.clone(),
                session_dir: Some(root.path().join("sessions")),
                ..Default::default()
            },
            crate::sandbox::SandboxSupport::full_only(),
        )
        .unwrap();
        let server = Server::new((*core).clone());
        let (server_side, client_side) = transport::duplex(65536);
        let connection = server.spawn(server_side);
        let (client, _) = ClientBuilder::new(ClientInfo {
            name: "压缩回归".into(),
            version: "1".into(),
            frontend: "test".into(),
        })
        .connect(client_side)
        .await
        .unwrap();
        let new = |cwd: &Path| SessionNewParams {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let (plain_info, compressed_info) = tokio::join!(
            client.call::<SessionNew>(new(&plain)),
            client.call::<SessionNew>(new(&compressed))
        );
        for (info, enabled) in [
            (plain_info.unwrap(), false),
            (compressed_info.unwrap(), true),
        ] {
            let record = core.inner.sessions.get(&info.id).await.unwrap();
            assert_eq!(
                record.paths.compressed(),
                enabled,
                "全局={global}，项目={enabled}"
            );
            let entry = core
                .inner
                .sessions
                .append_message(&info.id, Message::user_text("回退目标"), None, None)
                .await
                .unwrap();
            let rewind = client
                .call::<SessionRewind>(SessionRewindParams {
                    omit_auxiliary: false,
                    session_id: info.id.clone(),
                    entry_id: entry.id().to_owned(),
                    max_entries: None,
                })
                .await
                .unwrap();
            // 原地回退留在同一个 transcript 上，格式标记自然不变。
            assert_eq!(rewind.session.id, info.id);
            assert_eq!(
                core.inner
                    .sessions
                    .get(&rewind.session.id)
                    .await
                    .unwrap()
                    .paths
                    .compressed(),
                enabled
            );

            core.inner
                .sessions
                .append(
                    &info.id,
                    crate::session::entries::EntryPayload::Plan {
                        plan_id: "plan_test".into(),
                        title: "执行计划".into(),
                        path: "local://plan_test.md".into(),
                        markdown: "测试计划".into(),
                        status: moc_protocol::PlanStatus::Submitted,
                        version: 1,
                        previous_plan_id: None,
                    },
                )
                .await
                .unwrap();
            let plan = client
                .call::<SessionPlanApprove>(SessionPlanApproveParams {
                    session_id: info.id.clone(),
                    plan_id: "plan_test".into(),
                    action: PlanAction::ExecuteNewSession,
                    execution_engine: None,
                    execution_model: None,
                })
                .await
                .unwrap();
            assert_eq!(
                core.inner
                    .sessions
                    .get(&plan.session_id)
                    .await
                    .unwrap()
                    .paths
                    .compressed(),
                enabled
            );
        }
        // 保存项目配置后只影响以后创建的会话；全局与其它工作区不应被改写。
        let ConfigReply::Settings(view) = client
            .call::<ConfigManage>(ConfigAction::Read {
                scope: ConfigScope::Project,
                cwd: compressed.to_string_lossy().into_owned(),
            })
            .await
            .unwrap()
        else {
            panic!("配置视图");
        };
        client
            .call::<ConfigManage>(ConfigAction::Save {
                scope: ConfigScope::Project,
                cwd: compressed.to_string_lossy().into_owned(),
                document: json!({"session":{"compress":false}}),
                revision: view.revision,
            })
            .await
            .unwrap();
        let next = client.call::<SessionNew>(new(&compressed)).await.unwrap();
        assert!(
            !core
                .inner
                .sessions
                .get(&next.id)
                .await
                .unwrap()
                .paths
                .compressed()
        );
        client.close().await;
        connection.close();
        connection.wait().await;
        core.shutdown().await;
    }
}
