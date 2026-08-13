use std::sync::Arc;

use xai_computer_hub_sdk::LocalRegistry;
use xai_computer_use::{
    Action, ComputerUseService, FindRootsRequest, InMemoryBackend, RootInfo, StateStore, UiNode,
    register_computer_use_tools,
};

fn fixture() -> (Arc<InMemoryBackend>, Arc<ComputerUseService>) {
    let root = RootInfo {
        root_ref: "@r1".into(),
        resource_key: "window:1".into(),
        kind: "window".into(),
        title: "Test App".into(),
        app: Some("test.app".into()),
        pid: Some(42),
        ..Default::default()
    };
    let button = UiNode {
        element_ref: "@e2".into(),
        wire_ref: Some("ax:1".into()),
        role: "button".into(),
        title: "Save".into(),
        can_press: true,
        actions: vec!["press".into()],
        ..Default::default()
    };
    let root_node = UiNode {
        element_ref: "@e1".into(),
        role: "window".into(),
        title: "Test App".into(),
        children: vec![button],
        ..Default::default()
    };
    let backend = Arc::new(InMemoryBackend::new(vec![(root, root_node)]));
    let service = Arc::new(ComputerUseService::new(backend.clone()));
    (backend, service)
}

#[tokio::test]
async fn observes_searches_and_acts_from_a_captured_state() {
    let (backend, service) = fixture();
    let roots = service
        .find_roots(FindRootsRequest::default())
        .await
        .unwrap();
    let observation = service
        .observe(&roots[0].root_ref, Default::default())
        .await
        .unwrap();

    let matches = service
        .search(&observation.state_id, Some("save"), None, None)
        .await
        .unwrap();
    assert_eq!(matches.matches.len(), 1);
    assert_eq!(matches.matches[0].element_ref, "@e2");

    let result = service
        .act(xai_computer_use::ActRequest {
            state_id: observation.state_id.clone(),
            actions: vec![Action::Press {
                element_ref: "@e2".into(),
                wire_ref: None,
            }],
            expect: None,
        })
        .await
        .unwrap();
    assert_ne!(result.state_id, observation.state_id);
    assert_eq!(result.epoch, 1);
    assert_eq!(result.outcomes.len(), 1);
    assert!(backend.actions().await.len() == 1);

    let stale = service
        .act(xai_computer_use::ActRequest {
            state_id: observation.state_id,
            actions: vec![Action::Press {
                element_ref: "@e2".into(),
                wire_ref: None,
            }],
            expect: None,
        })
        .await
        .unwrap_err();
    assert!(stale.to_string().contains("stale"));
    assert_eq!(backend.actions().await.len(), 1);
}

#[tokio::test]
async fn rejects_uncertain_actions_before_backend_dispatch() {
    let (backend, service) = fixture();
    let observation = service.observe("@r1", Default::default()).await.unwrap();
    let error = service
        .act(xai_computer_use::ActRequest {
            state_id: observation.state_id,
            actions: vec![Action::SetText {
                element_ref: "@e2".into(),
                wire_ref: None,
                text: "unsafe".into(),
            }],
            expect: None,
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not supported"));
    assert!(backend.actions().await.is_empty());
}

#[tokio::test]
async fn resolves_wire_refs_before_backend_dispatch() {
    let (backend, service) = fixture();
    let observation = service.observe("@r1", Default::default()).await.unwrap();
    let result = service
        .act(xai_computer_use::ActRequest {
            state_id: observation.state_id,
            actions: vec![Action::Press {
                element_ref: "@e2".into(),
                wire_ref: None,
            }],
            expect: None,
        })
        .await
        .unwrap();
    assert_eq!(result.executed, 1);
    let dispatched = backend.actions().await;
    let press = &dispatched[0][0];
    match press {
        Action::Press { wire_ref, .. } => assert_eq!(wire_ref.as_deref(), Some("ax:1")),
        _ => panic!("expected press action"),
    }
}

#[tokio::test]
async fn wait_for_supports_live_timeout_polling() {
    let (_, service) = fixture();
    let observation = service.observe("@r1", Default::default()).await.unwrap();
    // Cached check against the stored state.
    let response = service
        .wait_for(xai_computer_use::WaitForRequest {
            state_id: observation.state_id.clone(),
            text: Some("Save".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(response.satisfied);
    // Absent condition on the same state.
    let response = service
        .wait_for(xai_computer_use::WaitForRequest {
            state_id: observation.state_id,
            text: Some("Save".into()),
            until: "absent".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!response.satisfied);
}

#[tokio::test]
async fn registers_the_complete_tool_family() {
    let (_, service) = fixture();
    let registry = LocalRegistry::new();
    register_computer_use_tools(&registry, service);
    assert_eq!(registry.len(), 11);
    let names: Vec<_> = registry
        .list_tools(&Default::default())
        .into_iter()
        .map(|description| description.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "find_roots",
            "observe_ui",
            "search_ui",
            "expand_ui",
            "inspect_ui",
            "act_ui",
            "read_text",
            "wait_for",
            "launch_browser",
            "navigate_browser",
            "evaluate_browser",
        ]
    );
}

#[tokio::test]
async fn state_store_evicts_old_observations_at_its_bound() {
    let store = StateStore::new(1);
    let first = store.create("window:1".into(), 0, "first").await;
    let second = store.create("window:1".into(), 1, "second").await;
    assert!(store.get(&first.state_id).await.is_none());
    assert_eq!(store.get(&second.state_id).await.unwrap().value, "second");
    assert_eq!(store.len().await, 1);
}
