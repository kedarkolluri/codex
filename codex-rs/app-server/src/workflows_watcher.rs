//! File watcher that keeps the saved-workflow registry live.
//!
//! Clones the shape of [`crate::skills_watcher::SkillsWatcher`]: it watches the
//! precedence-ordered workflow roots (spec §9) and, whenever a `*.js` /
//! `*.workflow.js` file under any of them changes, invalidates the shared
//! [`WorkflowsService`] cache and emits a single
//! [`ServerNotification::WorkflowsChanged`] (wire string `"workflows/changed"`)
//! so clients (and the next registry reader) treat their cached workflow list as
//! stale.
//!
//! Root registration mirrors the skills watcher's two-tier model:
//!
//! * **Static roots** — the personal (`$HOME/.agents/workflows`) and
//!   `$CODEX_HOME/workflows` roots — depend only on the startup config, so they
//!   are registered once at construction time.
//! * **Per-thread project roots** — `<thread cwd>/.codex/workflows` — depend on
//!   the attaching thread's cwd, so they are registered per thread via
//!   [`WorkflowsWatcher::register_thread_config`] (invoked from the same place
//!   the skills watcher's is: `ensure_listener_task_running`). The returned
//!   [`WatchRegistration`] is held for the thread listener's lifetime and
//!   unregisters on drop, exactly like the skills registration.

use std::sync::Arc;
use std::time::Duration;

use crate::outgoing_message::OutgoingMessageSender;
use crate::workflows_service::WorkflowsService;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::WorkflowsChangedNotification;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core_workflows::WorkflowRoot;
use codex_core_workflows::workflow_roots;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::Receiver;
use codex_file_watcher::ThrottledWatchReceiver;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use codex_protocol::protocol::TurnEnvironmentSelection;
use tokio_util::sync::CancellationToken;
use tokio_util::sync::DropGuard;
use tracing::warn;

#[cfg(not(test))]
const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_millis(50);

/// Assemble the **static** (config-level) workflow roots: the personal
/// (`$HOME/.agents/workflows`) and `$CODEX_HOME/workflows` roots. The
/// project-scoped `<cwd>/.codex/workflows` root is intentionally excluded here —
/// it is registered per thread from the attaching thread's cwd (see
/// [`WorkflowsWatcher::register_thread_config`]). Non-existent roots are
/// tolerated by discovery and by the file watcher (which falls back to the
/// nearest existing ancestor).
pub(crate) fn workflow_static_roots_from_config(config: &Config) -> Vec<WorkflowRoot> {
    let home_dir = dirs::home_dir();
    workflow_roots(
        /* repo_root */ None,
        home_dir.as_deref(),
        Some(config.codex_home.as_path()),
    )
}

fn watch_paths_for_roots(roots: &[WorkflowRoot]) -> Vec<WatchPath> {
    roots
        .iter()
        .map(|root| WatchPath {
            path: root.path.clone(),
            recursive: true,
        })
        .collect()
}

pub(crate) struct WorkflowsWatcher {
    // Held so its RAII guard is not dropped for the watcher's lifetime: dropping
    // the subscriber removes it (and all its path registrations) from the file
    // watcher.
    subscriber: FileWatcherSubscriber,
    // Static-root registration; dropped with the watcher.
    _static_roots_registration: WatchRegistration,
    shutdown_token: CancellationToken,
    _shutdown_drop_guard: DropGuard,
}

impl WorkflowsWatcher {
    pub(crate) fn new(
        service: Arc<WorkflowsService>,
        static_roots: Vec<WorkflowRoot>,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> Arc<Self> {
        let file_watcher = match FileWatcher::new() {
            Ok(file_watcher) => Arc::new(file_watcher),
            Err(err) => {
                warn!("failed to initialize workflows file watcher: {err}");
                Arc::new(FileWatcher::noop())
            }
        };
        let (subscriber, rx) = file_watcher.add_subscriber();
        let static_roots_registration =
            subscriber.register_paths(watch_paths_for_roots(&static_roots));
        let shutdown_token = CancellationToken::new();
        let shutdown_drop_guard = shutdown_token.clone().drop_guard();
        Self::spawn_event_loop(rx, service, outgoing, shutdown_token.child_token());
        Arc::new(Self {
            subscriber,
            _static_roots_registration: static_roots_registration,
            shutdown_token,
            _shutdown_drop_guard: shutdown_drop_guard,
        })
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown_token.cancel();
    }

    /// Register the attaching thread's project root
    /// (`<thread cwd>/.codex/workflows`) for watching and return the RAII
    /// registration. The caller holds it for the thread listener's lifetime so
    /// it unregisters on drop, mirroring [`SkillsWatcher::register_thread_config`].
    ///
    /// Remote environments are skipped: their cwd is not a host-local path, so
    /// watching it would register a spurious watch on an unrelated local
    /// ancestor.
    ///
    /// [`SkillsWatcher::register_thread_config`]: crate::skills_watcher::SkillsWatcher::register_thread_config
    pub(crate) fn register_thread_config(
        &self,
        config: &Config,
        thread_manager: &ThreadManager,
        environments: &[TurnEnvironmentSelection],
    ) -> WatchRegistration {
        let Some(environment_selection) = environments.first() else {
            return WatchRegistration::default();
        };
        let Some(environment) = thread_manager
            .environment_manager()
            .get_environment(&environment_selection.environment_id)
        else {
            warn!(
                "failed to register workflows watcher for unknown environment `{}`",
                environment_selection.environment_id
            );
            return WatchRegistration::default();
        };
        if environment.is_remote() {
            return WatchRegistration::default();
        }
        let roots = workflow_roots(
            Some(config.cwd.as_path()),
            /* home_dir */ None,
            /* codex_home */ None,
        );
        self.subscriber
            .register_paths(watch_paths_for_roots(&roots))
    }

    fn spawn_event_loop(
        rx: Receiver,
        service: Arc<WorkflowsService>,
        outgoing: Arc<OutgoingMessageSender>,
        shutdown_token: CancellationToken,
    ) {
        let mut rx = ThrottledWatchReceiver::new(rx, WATCHER_THROTTLE_INTERVAL);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!("workflows watcher listener skipped: no Tokio runtime available");
            return;
        };
        handle.spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = shutdown_token.cancelled() => break,
                    event = rx.recv() => event,
                };
                if event.is_none() {
                    break;
                }
                // Invalidate the shared registry cache so the next reader
                // re-discovers, then notify clients to invalidate theirs. No
                // workflow body is executed: discovery only statically parses the
                // leading `meta` literal, and it happens lazily on the next read.
                service.clear_cache();
                outgoing
                    .send_server_notification(ServerNotification::WorkflowsChanged(
                        WorkflowsChangedNotification {},
                    ))
                    .await;
            }
        });
    }
}
