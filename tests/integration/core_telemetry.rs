use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::common::{
    Scripted, calls_tool, default_budget, path_input, says, session_config, session_over, tool_set,
    with_usage,
};
use jensen_harness_core::agent::Recovered;
use jensen_harness_core::agent::Session;
use jensen_harness_core::providers::Usage;
use jensen_harness_core::store::{EventKind, Store};
use std::fs;
use tempfile::tempdir;

fn usage(input: u64, cache_read: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: 10,
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
    }
}

fn read_then_done(
    first: Usage,
    second: Usage,
    third: Usage,
) -> Vec<jensen_harness_core::providers::CompletionResponse> {
    vec![
        with_usage(calls_tool("t1", "read", path_input("a.txt")), first),
        with_usage(calls_tool("t2", "read", path_input("a.txt")), second),
        with_usage(says("done"), third),
    ]
}

async fn run(provider: &Scripted, dir: &std::path::Path) -> (Store, String, Session) {
    let store = Store::in_memory().unwrap();
    let mut session = session_over(
        store.clone(),
        tool_set(dir),
        session_config(20, default_budget()),
    )
    .await;
    session.prompt(provider, "go").await.unwrap();
    let id = session.session_id().to_string();
    (store, id, session)
}

fn setup() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "x").unwrap();
    dir
}

#[tokio::test]
async fn each_call_event_carries_the_fields_a_dashboard_needs() {
    let dir = setup();
    let provider = Scripted::of(read_then_done(
        usage(1_000, 9_000),
        usage(1_000, 9_000),
        usage(1_000, 9_000),
    ))
    .reporting_cache();
    let (store, id, _) = run(&provider, dir.path()).await;

    let events = store.events(&id).await.unwrap();
    let responded: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::ModelResponded)
        .collect();

    assert_eq!(responded.len(), 3);
    let payload = &responded[0].payload;
    for field in [
        "call_id",
        "run_id",
        "model",
        "stop_reason",
        "usage",
        "cost_micro_usd",
        "price_version",
        "latency_ms",
        "cache_hit_permille",
        "content",
    ] {
        assert!(payload.get(field).is_some(), "missing {field}: {payload}");
    }
    assert_eq!(payload["run_id"], format!("run-{id}"));
    assert_eq!(payload["cache_hit_permille"], 900);
    assert_eq!(payload["usage"]["cache_read_tokens"], 9_000);
}

#[tokio::test]
async fn a_low_hit_ratio_alerts_once_on_the_second_call_never_the_first() {
    let dir = setup();
    let provider = Scripted::of(read_then_done(
        usage(10_000, 0),
        usage(10_000, 0),
        usage(10_000, 0),
    ))
    .reporting_cache();
    let (store, id, _) = run(&provider, dir.path()).await;

    let events = store.events(&id).await.unwrap();
    let alerts: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::CacheLow)
        .collect();

    assert_eq!(alerts.len(), 1, "once per session, not once per call");
    assert_eq!(alerts[0].payload["cache_hit_permille"], 0);
    let responses: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::ModelResponded)
        .collect();
    assert_eq!(
        alerts[0].payload["call_id"],
        responses[1].payload["call_id"]
    );
    let alert_at = events
        .iter()
        .position(|e| e.kind == EventKind::CacheLow)
        .unwrap();
    let second_at = events.iter().position(|e| e == responses[1]).unwrap();
    assert_eq!(alert_at, second_at + 1, "right after the call it is about");
}

#[tokio::test]
async fn a_healthy_ratio_a_silent_provider_and_a_first_call_never_alert() {
    let dir = setup();
    let healthy = Scripted::of(read_then_done(
        usage(1_000, 9_000),
        usage(2_000, 8_000),
        usage(1_999, 8_001),
    ))
    .reporting_cache();
    let (store, id, _) = run(&healthy, dir.path()).await;
    assert!(
        store
            .events(&id)
            .await
            .unwrap()
            .iter()
            .all(|e| e.kind != EventKind::CacheLow)
    );

    let silent = Scripted::of(read_then_done(
        usage(10_000, 0),
        usage(10_000, 0),
        usage(10_000, 0),
    ));
    let (store, id, _) = run(&silent, dir.path()).await;
    assert!(
        store
            .events(&id)
            .await
            .unwrap()
            .iter()
            .all(|e| e.kind != EventKind::CacheLow)
    );

    let single = Scripted::of(vec![with_usage(says("hi"), usage(10_000, 0))]).reporting_cache();
    let (store, id, _) = run(&single, dir.path()).await;
    assert!(
        store
            .events(&id)
            .await
            .unwrap()
            .iter()
            .all(|e| e.kind != EventKind::CacheLow)
    );
}

#[tokio::test]
async fn a_restored_session_does_not_alert_twice() {
    let dir = setup();
    let provider = Scripted::of(read_then_done(
        usage(10_000, 0),
        usage(10_000, 0),
        usage(10_000, 0),
    ))
    .reporting_cache();
    let (store, id, _) = run(&provider, dir.path()).await;
    let recovered = Recovered::load(&store, &id).await.unwrap();
    let mut restored = Session::restore(store.clone(), recovered, tool_set(dir.path()))
        .await
        .unwrap();
    let more = Scripted::of(vec![with_usage(says("again"), usage(10_000, 0))]).reporting_cache();

    restored.prompt(&more, "once more").await.unwrap();

    let alerts = store
        .events(&id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::CacheLow)
        .count();
    assert_eq!(alerts, 1);
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One subscriber for the whole test process: a scoped one is per thread and
/// loses events when other tests run alongside.
fn captured_log() -> &'static Captured {
    static LOG: std::sync::OnceLock<Captured> = std::sync::OnceLock::new();
    LOG.get_or_init(|| {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
        captured
    })
}

#[tokio::test]
async fn every_settled_call_is_also_a_tracing_event_and_an_alert_is_a_warning() {
    let captured = captured_log();
    let dir = setup();
    let provider = Scripted::of(read_then_done(
        usage(10_000, 0),
        usage(10_000, 0),
        usage(10_000, 0),
    ))
    .reporting_cache();

    let (_, id, _) = run(&provider, dir.path()).await;

    let everything = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    let mine: Vec<&str> = everything
        .lines()
        .filter(|line| line.contains(id.as_str()))
        .collect();
    let settled: Vec<&&str> = mine
        .iter()
        .filter(|line| line.contains("model call settled"))
        .collect();
    assert_eq!(settled.len(), 3, "{mine:#?}");
    for field in [
        "input_tokens=10000",
        "cost_micro_usd=0",
        "price_version=test",
        "latency_ms=",
        "cache_hit_permille=Some(0)",
    ] {
        assert!(
            settled[0].contains(field),
            "missing {field} in {}",
            settled[0]
        );
    }
    let warnings: Vec<&&str> = mine.iter().filter(|line| line.contains("WARN")).collect();
    assert_eq!(warnings.len(), 1, "{mine:#?}");
    assert!(warnings[0].contains("less than 80% of the prompt was read from cache"));
}
