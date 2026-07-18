use super::*;
use std::sync::Arc;
use tempfile::TempDir;

fn write_workflow(dir: &std::path::Path, file: &str, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(file),
        format!("export const meta = {{ name: '{name}', description: 'd' }};\n"),
    )
    .unwrap();
}

#[tokio::test]
async fn registry_for_roots_caches_until_cleared() {
    let tmp = TempDir::new().unwrap();
    let root_dir = tmp.path().join("workflows");
    write_workflow(&root_dir, "a.workflow.js", "alpha");
    let roots = vec![WorkflowRoot::new(
        root_dir.clone(),
        WorkflowScope::CodexHome,
    )];

    let service = WorkflowsService::new();
    let first = service.registry_for_roots(roots.clone()).await;
    assert_eq!(first.names().collect::<Vec<_>>(), vec!["alpha"]);

    // Add a second workflow on disk; without invalidation the cache should
    // still return the original snapshot.
    write_workflow(&root_dir, "b.workflow.js", "bravo");
    let cached = service.registry_for_roots(roots.clone()).await;
    assert!(Arc::ptr_eq(&first, &cached));
    assert_eq!(cached.names().collect::<Vec<_>>(), vec!["alpha"]);

    // After clearing, discovery re-runs and observes the new file.
    service.clear_cache();
    let refreshed = service.registry_for_roots(roots).await;
    assert!(!Arc::ptr_eq(&first, &refreshed));
    assert_eq!(
        refreshed.names().collect::<Vec<_>>(),
        vec!["alpha", "bravo"]
    );
}

#[tokio::test]
async fn distinct_roots_do_not_share_cache_entries() {
    let tmp = TempDir::new().unwrap();
    let a_dir = tmp.path().join("a");
    let b_dir = tmp.path().join("b");
    write_workflow(&a_dir, "a.workflow.js", "from-a");
    write_workflow(&b_dir, "b.workflow.js", "from-b");

    let service = WorkflowsService::new();
    let a = service
        .registry_for_roots(vec![WorkflowRoot::new(a_dir, WorkflowScope::CodexHome)])
        .await;
    let b = service
        .registry_for_roots(vec![WorkflowRoot::new(b_dir, WorkflowScope::CodexHome)])
        .await;
    assert_eq!(a.names().collect::<Vec<_>>(), vec!["from-a"]);
    assert_eq!(b.names().collect::<Vec<_>>(), vec!["from-b"]);
}

/// Deterministic reproduction of the clear-during-load race (P1).
///
/// A reader misses and snapshots the generation. Before its (awaited) load
/// finishes, the watcher fires `clear_cache()` — bumping the generation and
/// emitting `workflows/changed`. When the in-flight load then tries to store
/// its now-stale result, it must NOT repopulate the just-invalidated cache;
/// otherwise every subsequent reader would observe the stale snapshot forever
/// despite the change notification.
///
/// This drives the same `lookup` / `store_if_current` steps that
/// [`WorkflowsService::registry_for_roots`] performs, interleaving the
/// `clear_cache()` exactly at the vulnerable point between them. No sleeps or
/// wall-clock timing are involved: the ordering is enforced by explicit call
/// sequencing, so the test is fully deterministic.
#[tokio::test]
async fn clear_cache_during_load_does_not_cache_stale_result() {
    let tmp = TempDir::new().unwrap();
    let root_dir = tmp.path().join("workflows");
    write_workflow(&root_dir, "a.workflow.js", "alpha");
    let roots = vec![WorkflowRoot::new(
        root_dir.clone(),
        WorkflowScope::CodexHome,
    )];
    let key = cache_key(&roots);

    let service = WorkflowsService::new();

    // Reader misses and snapshots the generation it will load under.
    let generation = match service.lookup(&key) {
        CacheLookup::Miss { generation } => generation,
        CacheLookup::Hit(_) => panic!("expected a cold-cache miss"),
    };

    // While that reader's discovery is "in flight", the watcher invalidates the
    // cache (and would emit workflows/changed).
    service.clear_cache();

    // The in-flight load completes and attempts to store its (now stale)
    // result under the pre-clear generation.
    let stale = Arc::new(load_workflows_from_roots(roots.clone()).await);
    let returned = service.store_if_current(key.clone(), generation, Arc::clone(&stale));

    // The racing caller still receives its own freshly loaded value...
    assert!(Arc::ptr_eq(&returned, &stale));

    // ...but the stale result must NOT have repopulated the invalidated cache:
    // the next reader still sees a miss and will re-discover.
    match service.lookup(&key) {
        CacheLookup::Miss { generation: g } => {
            assert_eq!(g, generation.wrapping_add(1));
        }
        CacheLookup::Hit(_) => panic!("stale result was cached despite clear_cache()"),
    }
}

/// A load whose generation is unchanged (no interleaved clear) does populate
/// the cache, and a concurrent loader for the same key observes the same `Arc`.
#[tokio::test]
async fn store_if_current_keeps_existing_entry_when_generation_unchanged() {
    let tmp = TempDir::new().unwrap();
    let root_dir = tmp.path().join("workflows");
    write_workflow(&root_dir, "a.workflow.js", "alpha");
    let roots = vec![WorkflowRoot::new(
        root_dir.clone(),
        WorkflowScope::CodexHome,
    )];
    let key = cache_key(&roots);

    let service = WorkflowsService::new();
    let generation = match service.lookup(&key) {
        CacheLookup::Miss { generation } => generation,
        CacheLookup::Hit(_) => panic!("expected a cold-cache miss"),
    };

    // First loader wins and its Arc becomes the cached entry.
    let first = Arc::new(load_workflows_from_roots(roots.clone()).await);
    let stored = service.store_if_current(key.clone(), generation, Arc::clone(&first));
    assert!(Arc::ptr_eq(&stored, &first));

    // A second loader for the same key (same generation) must observe the
    // already-cached Arc, not overwrite it.
    let second = Arc::new(load_workflows_from_roots(roots).await);
    let observed = service.store_if_current(key, generation, second);
    assert!(Arc::ptr_eq(&observed, &first));
}
