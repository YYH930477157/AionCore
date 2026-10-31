use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aion_agent::bootstrap::AgentBootstrap;
use aion_agent::engine::WorkspaceRoot;
use aion_agent::output::null_sink::NullSink;
use aion_agent::spawner::{AgentSpawner, SubAgentConfig};
use aion_agent::tool_policy::ToolPolicy;
use aion_config::config::{CliArgs, Config};
use aion_providers::LlmProvider;
use aion_providers::error::ProviderError;
use aion_types::llm::{LlmEvent, LlmRequest};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::{Semaphore, mpsc::Receiver};

fn config(directory: &Path) -> Config {
    let mut config = Config::resolve(&CliArgs {
        provider: Some("anthropic".into()),
        api_key: Some("test".into()),
        base_url: None,
        model: Some("test".into()),
        max_tokens: Some(100),
        thinking: None,
        thinking_budget: None,
        max_turns: Some(1),
        max_tool_call_malformed_turns: None,
        max_tool_call_failure_turns: None,
        system_prompt: None,
        profile: None,
        auto_approve: true,
        project_dir: Some(directory.to_path_buf()),
    })
    .unwrap();
    config.hooks = Default::default();
    config.mcp.servers.clear();
    config.session.enabled = false;
    config
}

#[derive(Default)]
struct RecordingProvider(Mutex<Vec<String>>);
#[async_trait]
impl LlmProvider for RecordingProvider {
    async fn stream(&self, request: &LlmRequest) -> Result<Receiver<LlmEvent>, ProviderError> {
        self.0.lock().unwrap().push(request.system.clone());
        Err(ProviderError::Parse("recorded request".into()))
    }
}

#[tokio::test]
async fn current_workspace_updates_actual_tools_model_requests_and_spawned_children() {
    let initial = tempfile::tempdir().unwrap();
    let next = tempfile::tempdir().unwrap();
    std::fs::write(next.path().join("next.txt"), "next-root-proof").unwrap();
    let provider = Arc::new(RecordingProvider::default());
    let mut engine = AgentBootstrap::new(
        config(initial.path()),
        initial.path().to_string_lossy(),
        Arc::new(NullSink),
    )
    .provider(provider.clone())
    .build()
    .await
    .unwrap()
    .engine;
    engine
        .update_workspace_context(
            Some(next.path().to_path_buf()),
            vec![WorkspaceRoot {
                id: "next-id".into(),
                name: "Next root".into(),
                path: next.path().to_path_buf(),
                available: true,
            }],
        )
        .unwrap();
    let command = if cfg!(windows) {
        "Get-Content next.txt"
    } else {
        "cat next.txt"
    };
    let result = engine
        .registry_mut()
        .get("ExecCommand")
        .unwrap()
        .execute(json!({"cmd": command}))
        .await;
    assert!(
        !result.is_error && result.content.contains("next-root-proof"),
        "{}",
        result.content
    );
    assert!(result.content.contains("Next root"), "{}", result.content);
    let _ = engine.run("Inspect current context", "first").await;
    assert!(provider.0.lock().unwrap().last().unwrap().contains("Next root"));
    engine.update_workspace_context(None, vec![]).unwrap();
    let result = engine
        .registry_mut()
        .get("ExecCommand")
        .unwrap()
        .execute(json!({"cmd": "echo forbidden"}))
        .await;
    assert!(
        result.is_error && result.content.contains("No default working directory"),
        "{}",
        result.content
    );
    let _ = engine.run("Inspect cleared context", "second").await;
    let system = provider.0.lock().unwrap().last().unwrap().clone();
    assert!(system.contains("No default working directory") && !system.contains("Next root"));
    let requests_before_spawn = provider.0.lock().unwrap().len();
    let _ = engine
        .registry_mut()
        .get("Spawn")
        .unwrap()
        .execute(json!({"tasks": [{"name": "child", "prompt": "Inspect context"}]}))
        .await;
    assert_eq!(provider.0.lock().unwrap().len(), requests_before_spawn + 1);
    let system = provider.0.lock().unwrap().last().unwrap().clone();
    assert!(
        system.contains("No default working directory"),
        "child kept stale workspace: {system}"
    );
}

struct ActiveCall(Arc<AtomicUsize>);
impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
struct WaitingProvider {
    active: Arc<AtomicUsize>,
    started: Semaphore,
    release: Semaphore,
}
#[async_trait]
impl LlmProvider for WaitingProvider {
    async fn stream(&self, _: &LlmRequest) -> Result<Receiver<LlmEvent>, ProviderError> {
        self.active.fetch_add(1, Ordering::SeqCst);
        let _active = ActiveCall(self.active.clone());
        self.started.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        Err(ProviderError::Parse("test completed".into()))
    }
}
fn tasks() -> Vec<SubAgentConfig> {
    ["first", "second"]
        .into_iter()
        .map(|name| SubAgentConfig {
            name: name.into(),
            prompt: name.into(),
            max_turns: 1,
            max_tokens: 100,
            system_prompt: None,
        })
        .collect()
}
fn waiting_provider(permits: usize) -> Arc<WaitingProvider> {
    Arc::new(WaitingProvider {
        active: Arc::new(AtomicUsize::new(0)),
        started: Semaphore::new(0),
        release: Semaphore::new(permits),
    })
}

#[tokio::test]
async fn parent_cancellation_drops_all_concurrent_spawned_children() {
    let directory = tempfile::tempdir().unwrap();
    let provider = waiting_provider(0);
    let spawner = AgentSpawner::new(
        provider.clone(),
        config(directory.path()),
        directory.path().to_path_buf(),
        ToolPolicy::Unrestricted,
    );
    let parent = tokio::spawn(async move { spawner.spawn_parallel(tasks()).await });
    let started = tokio::time::timeout(Duration::from_secs(5), provider.started.acquire_many(2)).await;
    parent.abort();
    let _ = parent.await;
    let surviving = provider.active.load(Ordering::SeqCst);
    provider.release.add_permits(2);
    assert!(started.is_ok(), "both children must run concurrently");
    assert_eq!(surviving, 0, "parent cancellation detached running child tasks");
}

#[tokio::test]
async fn spawned_results_keep_input_order() {
    let directory = tempfile::tempdir().unwrap();
    let spawner = AgentSpawner::new(
        waiting_provider(2),
        config(directory.path()),
        directory.path().to_path_buf(),
        ToolPolicy::Unrestricted,
    );
    let results = spawner.spawn_parallel(tasks()).await;
    assert_eq!(
        results.iter().map(|result| result.name.as_str()).collect::<Vec<_>>(),
        ["first", "second"]
    );
}

#[tokio::test]
async fn local_workspace_references_resolve_files_and_commands_without_default() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(first.path().join("value.txt"), "first").unwrap();
    std::fs::write(second.path().join("value.txt"), "second").unwrap();
    let mut engine = AgentBootstrap::new(
        config(first.path()),
        first.path().to_string_lossy().into_owned(),
        Arc::new(NullSink),
    )
    .provider(Arc::new(RecordingProvider::default()))
    .build()
    .await
    .unwrap()
    .engine;
    engine
        .update_workspace_context(
            None,
            vec![
                WorkspaceRoot {
                    id: "app-id".into(),
                    name: "application".into(),
                    path: first.path().into(),
                    available: true,
                },
                WorkspaceRoot {
                    id: "lib-id".into(),
                    name: "公共 library".into(),
                    path: second.path().into(),
                    available: true,
                },
            ],
        )
        .unwrap();
    let registry = engine.registry_mut();
    let tool = registry
        .get("Workspace")
        .expect("Local workspace lookup must be available without system context");
    let listing = tool.execute(json!({})).await;
    assert!(!listing.is_error, "{}", listing.content);
    let listing: serde_json::Value = serde_json::from_str(&listing.content).unwrap();
    assert!(listing["default_cwd"].is_null());
    assert_eq!(listing["roots"][1]["name"], "公共 library");
    let read = registry
        .get("Read")
        .unwrap()
        .execute(json!({"file_path": "@{公共 library}/value.txt"}))
        .await;
    assert!(!read.is_error && read.content.contains("second"), "{}", read.content);
    let edit = registry
        .get("Edit")
        .unwrap()
        .execute(
            json!({"file_path": "@{lib-id}/value.txt", "old_string": "second", "new_string": "@application/value.txt"}),
        )
        .await;
    assert!(!edit.is_error, "{}", edit.content);
    assert_eq!(
        std::fs::read_to_string(second.path().join("value.txt")).unwrap(),
        "@application/value.txt"
    );
    assert_eq!(
        std::fs::read_to_string(first.path().join("value.txt")).unwrap(),
        "first"
    );
    let write = registry
        .get("Write")
        .unwrap()
        .execute(json!({"file_path":"@application/new/file.txt", "content":"@{lib-id}/value.txt"}))
        .await;
    assert!(!write.is_error, "{}", write.content);
    let glob = registry
        .get("Glob")
        .unwrap()
        .execute(json!({"path":"@application", "pattern":"**/*.txt"}))
        .await;
    assert!(!glob.is_error && glob.content.contains("file.txt"), "{}", glob.content);
    let command = if cfg!(windows) {
        "Write-Output '@application'; Get-Location"
    } else {
        "printf '@application\\n'; pwd"
    };
    let result = registry
        .get("ExecCommand")
        .unwrap()
        .execute(json!({"workdir":"@{lib-id}", "cmd":command}))
        .await;
    assert!(
        !result.is_error && result.content.contains("@application") && result.content.contains("公共 library"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn local_workspace_references_reject_ambiguity_unavailable_and_escape() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = AgentBootstrap::new(
        config(directory.path()),
        directory.path().to_string_lossy().into_owned(),
        Arc::new(NullSink),
    )
    .provider(Arc::new(RecordingProvider::default()))
    .build()
    .await
    .unwrap()
    .engine;
    let root = |id: &str, available| WorkspaceRoot {
        id: id.into(),
        name: "library".into(),
        path: directory.path().into(),
        available,
    };
    engine
        .update_workspace_context(None, vec![root("one", true), root("two", true), root("offline", false)])
        .unwrap();
    for (reference, expected) in [
        ("@library", "ambiguous"),
        ("@missing", "Unknown"),
        ("@{offline}", "unavailable"),
        ("@{one}/../outside", "outside"),
    ] {
        let result = engine
            .registry_mut()
            .get("ExecCommand")
            .unwrap()
            .execute(json!({"workdir":reference,"cmd":"echo should-not-run"}))
            .await;
        assert!(
            result.is_error && result.content.contains(expected),
            "{reference}: {}",
            result.content
        );
    }
    let query = engine
        .registry_mut()
        .get("Workspace")
        .unwrap()
        .execute(json!({"reference":"@{one}"}))
        .await;
    assert!(!query.is_error, "{}", query.content);
    #[cfg(windows)]
    {
        let ambiguous = engine
            .registry_mut()
            .get("Workspace")
            .unwrap()
            .execute(json!({"reference":"@LIBRARY"}))
            .await;
        assert!(ambiguous.is_error && ambiguous.content.contains("ambiguous"));
    }
    engine.update_workspace_context(None, vec![root("two", true)]).unwrap();
    let renamed_root = WorkspaceRoot {
        name: "renamed".into(),
        ..root("two", true)
    };
    engine.update_workspace_context(None, vec![renamed_root]).unwrap();
    let renamed = engine
        .registry_mut()
        .get("Workspace")
        .unwrap()
        .execute(json!({"reference": if cfg!(windows) { "@RENAMED" } else { "@renamed" }}))
        .await;
    assert!(!renamed.is_error, "{}", renamed.content);
    let stale = engine
        .registry_mut()
        .get("Read")
        .unwrap()
        .execute(json!({"file_path":"@{one}/value.txt"}))
        .await;
    assert!(stale.is_error && stale.content.contains("Unknown"), "{}", stale.content);
}

#[cfg(windows)]
#[tokio::test]
async fn workspace_reference_rejects_junction_escape_for_new_write_target() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let link = directory.path().join("outside-link");
    let command = format!(
        "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
        link.display(),
        outside.path().display()
    );
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &command])
        .status()
        .unwrap();
    assert!(status.success());
    let mut engine = AgentBootstrap::new(
        config(directory.path()),
        directory.path().to_string_lossy(),
        Arc::new(NullSink),
    )
    .provider(Arc::new(RecordingProvider::default()))
    .build()
    .await
    .unwrap()
    .engine;
    engine
        .update_workspace_context(
            None,
            vec![WorkspaceRoot {
                id: "one".into(),
                name: "library".into(),
                path: directory.path().into(),
                available: true,
            }],
        )
        .unwrap();
    let result = engine
        .registry_mut()
        .get("Write")
        .unwrap()
        .execute(json!({"file_path":"@library/outside-link/new/file.txt", "content":"must not escape"}))
        .await;
    // Remove the junction itself before TempDir cleanup; never recurse into its target.
    std::fs::remove_dir(&link).unwrap();
    assert!(
        result.is_error && result.content.contains("outside"),
        "{}",
        result.content
    );
    assert!(!outside.path().join("new").exists());
}

struct ApprovalCapture(tokio::sync::mpsc::UnboundedSender<serde_json::Value>);
impl aion_protocol::writer::ProtocolEmitter for ApprovalCapture {
    fn emit(&self, event: &aion_protocol::events::ProtocolEvent) -> std::io::Result<()> {
        if let aion_protocol::events::ProtocolEvent::ToolRequest { tool, .. } = event {
            self.0.send(tool.args.clone()).unwrap();
        }
        Ok(())
    }
}

struct ResultCapture(Mutex<Vec<String>>);
impl aion_protocol::writer::ProtocolEmitter for ResultCapture {
    fn emit(&self, event: &aion_protocol::events::ProtocolEvent) -> std::io::Result<()> {
        if let aion_protocol::events::ProtocolEvent::ToolResult { call_id, status, .. } = event {
            assert!(matches!(status, aion_protocol::events::ToolStatus::Error));
            self.0.lock().unwrap().push(call_id.clone());
        }
        Ok(())
    }
}

#[tokio::test]
async fn invalid_workspace_reference_emits_protocol_result() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = AgentBootstrap::new(
        config(directory.path()),
        directory.path().to_string_lossy(),
        Arc::new(NullSink),
    )
    .provider(Arc::new(RecordingProvider::default()))
    .build()
    .await
    .unwrap()
    .engine;
    let calls = vec![aion_types::message::ContentBlock::ToolUse {
        id: "invalid-reference".into(),
        name: "Read".into(),
        input: json!({"file_path":"@missing/file.txt"}),
        extra: None,
    }];
    let capture = Arc::new(ResultCapture(Mutex::new(Vec::new())));
    let writer: Arc<dyn aion_protocol::writer::ProtocolEmitter> = capture.clone();
    let manager = Arc::new(aion_protocol::ToolApprovalManager::new());
    let outcome = aion_agent::orchestration::execute_tool_calls_with_approval(
        engine.registry_mut(),
        &calls,
        &manager,
        &writer,
        "test",
        true,
        &[],
        None,
        Default::default(),
        false,
    )
    .await
    .unwrap();
    assert!(matches!(
        &outcome.results[0],
        aion_types::message::ContentBlock::ToolResult { is_error: true, .. }
    ));
    assert_eq!(*capture.0.lock().unwrap(), ["invalid-reference"]);
}

#[tokio::test]
async fn workspace_reference_approval_uses_actual_path_and_preserves_original_input() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = AgentBootstrap::new(
        config(directory.path()),
        directory.path().to_string_lossy(),
        Arc::new(NullSink),
    )
    .provider(Arc::new(RecordingProvider::default()))
    .build()
    .await
    .unwrap()
    .engine;
    engine
        .update_workspace_context(
            None,
            vec![WorkspaceRoot {
                id: "root-id".into(),
                name: "library".into(),
                path: directory.path().into(),
                available: true,
            }],
        )
        .unwrap();
    let calls = vec![aion_types::message::ContentBlock::ToolUse {
        id: "approval-call".into(),
        name: "Write".into(),
        input: json!({"file_path":"@library/new.txt","content":"@library stays literal"}),
        extra: None,
    }];
    let manager = Arc::new(aion_protocol::ToolApprovalManager::new());
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let writer: Arc<dyn aion_protocol::writer::ProtocolEmitter> = Arc::new(ApprovalCapture(sender));
    let pending = manager.clone();
    let check = tokio::spawn(async move {
        let args = receiver.recv().await.unwrap();
        pending.resolve("approval-call", aion_protocol::ToolApprovalResult::Approved);
        args
    });
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        aion_agent::orchestration::execute_tool_calls_with_approval(
            engine.registry_mut(),
            &calls,
            &manager,
            &writer,
            "test",
            false,
            &[],
            None,
            Default::default(),
            false,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let args = check.await.unwrap();
    assert_eq!(
        args["file_path"],
        directory.path().join("new.txt").to_string_lossy().as_ref()
    );
    assert_eq!(args["content"], "@library stays literal");
    assert!(
        matches!(&calls[0], aion_types::message::ContentBlock::ToolUse {input,..} if input["file_path"] == "@library/new.txt")
    );
    assert!(matches!(
        &outcome.results[0],
        aion_types::message::ContentBlock::ToolResult { is_error: false, .. }
    ));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("new.txt")).unwrap(),
        "@library stays literal"
    );
}

#[tokio::test]
async fn workspace_references_preserve_image_follow_up_blocks() {
    let directory = tempfile::tempdir().unwrap();
    use base64::Engine as _;
    std::fs::write(
        directory.path().join("pixel.png"),
        base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6WQAAAABJRU5ErkJggg==")
            .unwrap(),
    )
    .unwrap();
    let mut engine = AgentBootstrap::new(
        config(directory.path()),
        directory.path().to_string_lossy(),
        Arc::new(NullSink),
    )
    .provider(Arc::new(RecordingProvider::default()))
    .build()
    .await
    .unwrap()
    .engine;
    engine
        .update_workspace_context(
            None,
            vec![WorkspaceRoot {
                id: "root-id".into(),
                name: "images".into(),
                path: directory.path().into(),
                available: true,
            }],
        )
        .unwrap();
    let output = engine
        .registry_mut()
        .get("ViewImage")
        .unwrap()
        .execute_with_follow_up(json!({"file_path":"@images/pixel.png"}))
        .await;
    assert!(!output.result.is_error, "{}", output.result.content);
    assert!(
        !output.follow_up_blocks.is_empty(),
        "image adapter lost its follow-up attachment"
    );
}

#[tokio::test]
async fn workspace_reference_failures_keep_concurrent_call_result_order() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("value.txt"), "proof").unwrap();
    let mut engine = AgentBootstrap::new(
        config(directory.path()),
        directory.path().to_string_lossy(),
        Arc::new(NullSink),
    )
    .provider(Arc::new(RecordingProvider::default()))
    .build()
    .await
    .unwrap()
    .engine;
    engine
        .update_workspace_context(
            None,
            vec![WorkspaceRoot {
                id: "one".into(),
                name: "library".into(),
                path: directory.path().into(),
                available: true,
            }],
        )
        .unwrap();
    let calls: Vec<_> = [
        ("first", "@library/value.txt"),
        ("second", "@missing/value.txt"),
        ("third", "@library/value.txt"),
    ]
    .into_iter()
    .map(|(id, path)| aion_types::message::ContentBlock::ToolUse {
        id: id.into(),
        name: "Read".into(),
        input: json!({"file_path":path}),
        extra: None,
    })
    .collect();
    let confirmer = Arc::new(Mutex::new(aion_agent::confirm::ToolConfirmer::new(true, vec![])));
    let outcome = aion_agent::orchestration::execute_tool_calls(
        engine.registry_mut(),
        &calls,
        &confirmer,
        None,
        Default::default(),
        false,
    )
    .await
    .unwrap();
    let ids: Vec<_> = outcome
        .results
        .iter()
        .filter_map(|block| match block {
            aion_types::message::ContentBlock::ToolResult {
                tool_use_id, is_error, ..
            } => Some((tool_use_id.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(ids, [("first", false), ("second", true), ("third", false)]);
}

#[tokio::test]
async fn read_after_agent_write_returns_new_content_before_deduplicating() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("value.txt");
    std::fs::write(&file, "version-one").unwrap();
    let mut engine = AgentBootstrap::new(
        config(directory.path()),
        directory.path().to_string_lossy(),
        Arc::new(NullSink),
    )
    .provider(Arc::new(RecordingProvider::default()))
    .build()
    .await
    .unwrap()
    .engine;
    let registry = engine.registry_mut();
    let input = json!({"file_path":file});
    let read = registry.get("Read").unwrap();
    assert!(read.execute(input.clone()).await.content.contains("version-one"));
    let edit = registry
        .get("Edit")
        .unwrap()
        .execute(json!({"file_path":file,"old_string":"version-one","new_string":"version-two"}))
        .await;
    assert!(!edit.is_error, "{}", edit.content);
    // Consecutive edits must retain the staleness guard state without requiring a read between each edit.
    let edit = registry
        .get("Edit")
        .unwrap()
        .execute(json!({"file_path":file,"old_string":"version-two","new_string":"version-three"}))
        .await;
    assert!(!edit.is_error, "{}", edit.content);
    let updated = read.execute(input.clone()).await;
    assert!(
        updated.content.contains("version-three"),
        "Read must return edited content: {}",
        updated.content
    );
    assert!(read.execute(input.clone()).await.content.contains("File unchanged"));
    let write = registry
        .get("Write")
        .unwrap()
        .execute(json!({"file_path":file,"content":"version-four"}))
        .await;
    assert!(!write.is_error, "{}", write.content);
    let updated = read.execute(input.clone()).await;
    assert!(
        updated.content.contains("version-four"),
        "Read must return written content: {}",
        updated.content
    );
    assert!(read.execute(input).await.content.contains("File unchanged"));
}
