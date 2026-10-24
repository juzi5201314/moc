//! 用轻量协议替身验证进程生命周期；JS 行为由 worker 单测覆盖。
#![cfg(unix)]
use super::*;
use serde_json::Value;

fn host() -> PtcHost {
    PtcHost::new(PtcWorkerCommand {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), r#"
while IFS= read -r line; do
    case "$line" in
      *'"type":"start"'*)
        id=${line#*\"execution_id\":}; id=${id%%,*}
        case "$line" in
          *'"code":"crash"'*) exit 7 ;;
          *'"code":"corrupt"'*) printf 'invalid json\n' ;;
          *'"code":"hold"'*)
            printf '{"type":"step","execution_id":%s,"step":{"requests":[],"state":{"state":"pending"}}}\n' "$id" ;;
          *) printf '{"type":"step","execution_id":%s,"step":{"requests":[],"state":{"state":"done","value":%s}}}\n' "$id" "$$" ;;
        esac ;;
    esac
done
"#.into()],
    })
}
fn request(code: &str) -> StartRequest {
    StartRequest {
        code: code.into(),
        memory_limit_bytes: 64 * 1024 * 1024,
        max_output_bytes: 1024,
        max_message_bytes: 8192,
        max_pending_calls: 8,
        max_total_calls: 1000,
        timeout_ms: 10_000,
    }
}
async fn step(guest: &mut Guest) -> Result<GuestStep, ToolError> {
    tokio::time::timeout(Duration::from_secs(4), guest.step(Vec::new()))
        .await
        .expect("Host 不应挂起")
}

async fn step_with_timeout(guest: &mut Guest, timeout: Duration) -> Result<GuestStep, ToolError> {
    tokio::time::timeout(timeout, guest.step(Vec::new()))
        .await
        .expect("Host 不应挂起")
}
async fn receive(guest: &mut Guest) -> Result<GuestStep, ToolError> {
    tokio::time::timeout(Duration::from_secs(4), guest.receive())
        .await
        .expect("Host 应及时失败")
}
async fn pid(host: &PtcHost, config: &PtcConfig) -> u64 {
    let mut guest = host.start(request("pid"), config).unwrap();
    match step(&mut guest).await.unwrap().state {
        GuestState::Done {
            value: Value::Number(pid),
        } => pid.as_u64().unwrap(),
        state => panic!("{state:?}"),
    }
}

#[tokio::test]
async fn shutdown_is_idempotent_when_callers_race() {
    let host = host();
    tokio::time::timeout(Duration::from_secs(4), async {
        tokio::join!(host.shutdown(), host.shutdown());
    })
    .await
    .expect("并发 shutdown 不应互相等待");
    assert!(host.start(request("pid"), &PtcConfig::default()).is_err());
}

#[tokio::test]
async fn reuses_one_pid_and_routes_concurrent_executions() {
    let host = host();
    let config = PtcConfig::default();
    let original = pid(&host, &config).await;
    let mut a = host.start(request("pid"), &config).unwrap();
    let mut b = host.start(request("pid"), &config).unwrap();
    let (a, b) = tokio::join!(step(&mut a), step(&mut b));
    for step in [a.unwrap(), b.unwrap()] {
        assert!(matches!(step.state, GuestState::Done { value } if value == original));
    }
    assert_eq!(pid(&host, &config).await, original);
    host.shutdown().await;
    assert!(host.start(request("pid"), &config).is_err());
}

#[tokio::test]
async fn fatal_errors_fail_all_routes_and_restart_lazily() {
    let host = host();
    let config = PtcConfig::default();
    for fault in ["crash", "corrupt"] {
        let original = pid(&host, &config).await;
        let mut pending = host.start(request("hold"), &config).unwrap();
        assert!(matches!(
            step(&mut pending).await.unwrap().state,
            GuestState::Pending
        ));
        let mut broken = host.start(request(fault), &config).unwrap();
        assert_eq!(step(&mut broken).await.unwrap_err().code, "E_PTC_WORKER");
        assert_eq!(
            receive(&mut pending).await.unwrap_err().code,
            "E_PTC_WORKER"
        );
        assert_ne!(pid(&host, &config).await, original);
    }
    host.shutdown().await;
}

#[tokio::test]
async fn bounds_global_runs_and_kills_unresponsive_cancel() {
    let host = host();
    let config = PtcConfig {
        max_concurrent_runs: 1,
        ..PtcConfig::default()
    };
    let original = pid(&host, &config).await;
    let mut pending = host.start(request("hold"), &config).unwrap();
    step(&mut pending).await.unwrap();
    let mut rejected = host.start(request("pid"), &config).unwrap();
    assert_eq!(step(&mut rejected).await.unwrap_err().code, "E_QUEUE_LIMIT");
    pending.cancel("E_CANCELLED");
    assert_eq!(
        receive(&mut pending).await.unwrap_err().code,
        "E_PTC_WORKER"
    );
    assert_ne!(pid(&host, &config).await, original);
    host.shutdown().await;
}

#[tokio::test]
async fn oversized_route_does_not_kill_healthy_siblings() {
    let host = host();
    let config = PtcConfig::default();
    let mut oversized = request("hold");
    oversized.max_message_bytes = 1;
    let mut oversized = host.start(oversized, &config).unwrap();
    assert_eq!(
        step_with_timeout(&mut oversized, Duration::from_secs(1))
            .await
            .unwrap_err()
            .code,
        "E_OUTPUT_LIMIT"
    );

    let mut sibling = host.start(request("pid"), &config).unwrap();
    let result = step_with_timeout(&mut sibling, Duration::from_secs(1)).await;
    assert!(matches!(result.unwrap().state, GuestState::Done { .. }));
    host.shutdown().await;
}

#[tokio::test]
async fn idle_reaps_only_without_active_execution() {
    let host = host();
    let config = PtcConfig {
        host_idle_timeout_secs: 1,
        ..PtcConfig::default()
    };
    let original = pid(&host, &config).await;
    let mut pending = host.start(request("hold"), &config).unwrap();
    step(&mut pending).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(pid(&host, &config).await, original);
    // 显式关闭会让所有 execution 失败，并等待子进程回收。
    host.shutdown().await;
    assert_eq!(
        receive(&mut pending).await.unwrap_err().code,
        "E_PTC_WORKER"
    );
    let host = self::host();
    let original = pid(&host, &config).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_ne!(pid(&host, &config).await, original);
    host.shutdown().await;
}

#[tokio::test]
async fn dropping_execution_triggers_cancel_without_killing_healthy_siblings_early() {
    let host = host();
    let config = PtcConfig::default();
    let mut dropped = host.start(request("hold"), &config).unwrap();
    step(&mut dropped).await.unwrap();
    let mut sibling = host.start(request("hold"), &config).unwrap();
    step(&mut sibling).await.unwrap();
    drop(dropped);
    // 替身刻意不响应 Cancel，宽限期结束后 sibling 一同收到 Host 故障。
    assert_eq!(
        receive(&mut sibling).await.unwrap_err().code,
        "E_PTC_WORKER"
    );
    let _ = pid(&host, &config).await;
    host.shutdown().await;
}
