mod support;

use moc_core::sandbox::{SandboxRunner, SandboxSupport};
use moc_protocol::SessionPermission;
use moc_protocol::methods::{SessionMode, SessionSetPermission, SessionSetPermissionParams};
use support::Harness;

fn all_modes() -> SandboxSupport {
    SandboxSupport {
        runner: Some(SandboxRunner::Bubblewrap),
        modes: vec![
            SessionPermission::Full,
            SessionPermission::Workspace,
            SessionPermission::WorkspaceOnly,
        ],
        home_overlay: false,
    }
}

async fn harness_with(support: SandboxSupport) -> Harness {
    Harness::with_sandbox_support(
        "http://127.0.0.1:1",
        "openai_chat",
        "test/gpt-test:off",
        "test/gpt-test:off",
        "",
        support,
    )
    .await
}

#[tokio::test]
async fn set_permission_accepts_only_supported_modes_and_clear_grants() {
    let harness = harness_with(all_modes()).await;
    let id = harness.session(SessionMode::Normal).await;

    let info = harness
        .client
        .call::<SessionSetPermission>(SessionSetPermissionParams {
            session_id: id.clone(),
            permission: SessionPermission::WorkspaceOnly,
            clear_grants: false,
        })
        .await
        .expect("workspace-only supported");
    assert_eq!(info.permission, SessionPermission::WorkspaceOnly);

    let info = harness
        .client
        .call::<SessionSetPermission>(SessionSetPermissionParams {
            session_id: id.clone(),
            permission: SessionPermission::Workspace,
            clear_grants: true,
        })
        .await
        .expect("clear grants while switching");
    assert_eq!(info.permission, SessionPermission::Workspace);

    harness.close().await;
}

#[tokio::test]
async fn set_permission_rejects_modes_without_a_runner() {
    let harness = harness_with(SandboxSupport::full_only()).await;
    let id = harness.session(SessionMode::Normal).await;
    let error = harness
        .client
        .call::<SessionSetPermission>(SessionSetPermissionParams {
            session_id: id,
            permission: SessionPermission::Workspace,
            clear_grants: false,
        })
        .await
        .expect_err("full-only support must reject workspace");
    assert!(error.to_string().contains("可用模式"));
    harness.close().await;
}
