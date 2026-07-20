//! Shared, cache-backed access to the saved-workflow registry.
//!
//! Mirrors the role [`codex_core::skills::SkillsService`] plays for skills: a
//! process-scoped owner of discovered workflow metadata with an on-demand,
//! invalidatable cache. Discovery only statically parses the leading `meta`
//! literal of each script (never executing the body), so building a registry is
//! safe but still involves filesystem walks and parsing — hence the cache.
//!
//! The workflows file watcher ([`crate::workflows_watcher::WorkflowsWatcher`])
//! calls [`WorkflowsService::clear_cache`] whenever a watched workflow file
//! changes, then emits `workflows/changed`, so the next reader observes fresh
//! metadata. Registry readers such as `workflow/list` call
//! [`WorkflowsService::registry_for_roots`] to obtain a cached registry for a
//! given precedence-ordered root set.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use codex_core_workflows::WorkflowRegistry;
use codex_core_workflows::WorkflowRoot;
use codex_core_workflows::WorkflowScope;
use codex_core_workflows::load_workflows_from_roots;
use tracing::info;

/// Cache key for a precedence-ordered set of workflow roots.
///
/// Two calls that pass the same roots (path + scope, in the same order) share a
/// cached registry; any difference re-discovers. Scope is part of the key
/// because it drives dedupe precedence during discovery.
type WorkflowRootsCacheKey = Vec<(PathBuf, WorkflowScope)>;

fn cache_key(roots: &[WorkflowRoot]) -> WorkflowRootsCacheKey {
    roots
        .iter()
        .map(|root| (root.path.clone(), root.scope))
        .collect()
}

/// Mutex-guarded cache state: the discovered registries plus a generation
/// counter bumped on every [`WorkflowsService::clear_cache`].
///
/// The generation is what closes the clear-during-load race: a reader that
/// misses snapshots the current generation *before* it starts its (awaited)
/// discovery, and only writes its result back if the generation has not moved.
/// If a `clear_cache()` fired while the load was in flight, the generation has
/// advanced and the now-stale result is dropped instead of repopulating the
/// cache the watcher just invalidated.
#[derive(Default)]
struct CacheState {
    generation: u64,
    entries: HashMap<WorkflowRootsCacheKey, Arc<WorkflowRegistry>>,
}

/// Outcome of a cache lookup: either a live hit, or a miss carrying the
/// generation the caller must load under.
enum CacheLookup {
    Hit(Arc<WorkflowRegistry>),
    Miss { generation: u64 },
}

/// Process-scoped owner of discovered workflow metadata with an invalidatable
/// cache. Cheap to `Arc`-share across the watcher and request processors.
#[derive(Default)]
pub(crate) struct WorkflowsService {
    cache: Mutex<CacheState>,
}

impl WorkflowsService {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Return the cached registry for `roots`, discovering (and caching) it on a
    /// miss. Discovery never executes workflow bodies.
    ///
    /// The watcher only needs [`Self::clear_cache`].
    pub(crate) async fn registry_for_roots(
        &self,
        roots: Vec<WorkflowRoot>,
    ) -> Arc<WorkflowRegistry> {
        let key = cache_key(&roots);
        // Snapshot the generation *before* the await so a concurrent
        // `clear_cache()` is observable when we go to store the result.
        let generation = match self.lookup(&key) {
            CacheLookup::Hit(hit) => return hit,
            CacheLookup::Miss { generation } => generation,
        };
        let registry = Arc::new(load_workflows_from_roots(roots).await);
        self.store_if_current(key, generation, registry)
    }

    /// Drop all cached registries so the next reader re-discovers, and bump the
    /// generation so any in-flight load loses the [`Self::store_if_current`]
    /// race. Called by the watcher before emitting `workflows/changed`.
    pub(crate) fn clear_cache(&self) {
        let cleared = {
            let mut cache = self.lock();
            let cleared = cache.entries.len();
            cache.entries.clear();
            cache.generation = cache.generation.wrapping_add(1);
            cleared
        };
        info!("workflows cache cleared ({cleared} entries)");
    }

    /// Look up `key`, returning a hit or the generation a miss must load under.
    fn lookup(&self, key: &WorkflowRootsCacheKey) -> CacheLookup {
        let cache = self.lock();
        match cache.entries.get(key) {
            Some(hit) => CacheLookup::Hit(Arc::clone(hit)),
            None => CacheLookup::Miss {
                generation: cache.generation,
            },
        }
    }

    /// Store `registry` for `key` iff the cache generation still matches the one
    /// snapshotted before the load. Returns the registry the caller should use.
    ///
    /// * Generation unchanged: cache and return whichever `Arc` is now present
    ///   (another loader for the same key may have won the race first), so
    ///   readers observe a stable `Arc`.
    /// * Generation advanced: a `clear_cache()` raced our load, so the result
    ///   may be stale. Do **not** repopulate the just-invalidated cache — hand
    ///   this caller its freshly loaded value and let the next reader
    ///   re-discover.
    fn store_if_current(
        &self,
        key: WorkflowRootsCacheKey,
        generation: u64,
        registry: Arc<WorkflowRegistry>,
    ) -> Arc<WorkflowRegistry> {
        let mut cache = self.lock();
        if cache.generation != generation {
            return registry;
        }
        Arc::clone(cache.entries.entry(key).or_insert(registry))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[path = "workflows_service_tests.rs"]
mod tests;
