use crate::drive::commands::ManagerCommand;
use crate::drive::commands::MountCommand;
use crate::drive::event_blocker::EventBlocker;
use crate::drive::ignore::IgnoreMatcher;
use crate::drive::sync::group_fs_events;
use crate::drive::utils::recycle_bin_url;
use crate::inventory::{DrivePropsUpdate, InventoryDb, TaskRecord};
use crate::platform::provider::{ProviderConfig, SyncProvider};
use crate::tasks::{TaskProgress, TaskQueue, TaskQueueConfig};
use crate::utils::toast;
use ::serde::{Deserialize, Serialize};
use anyhow::{Context, Result};
use cloudreve_api::api::user::UserApi;
use cloudreve_api::{Client, ClientConfig, models::user::Token};
use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
#[cfg(target_os = "windows")]
use sha2::{Digest, Sha256};
use std::time::Duration;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::spawn;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::task::JoinHandle;
use url::Url;

#[cfg(target_os = "windows")]
use crate::cfapi::root::{SecurityId, SyncRootId, SyncRootIdBuilder};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DriveConfig {
    pub id: String,
    pub name: String,
    pub instance_url: String,
    pub remote_path: String,
    pub credentials: Credentials,
    pub sync_path: PathBuf,
    pub icon_path: Option<String>,
    /// Path to the raw (non-ICO) favicon image
    pub raw_icon_path: Option<String>,
    pub enabled: bool,
    pub user_id: String,

    // Platform-specific sync root identifier (serialized string)
    #[cfg(target_os = "windows")]
    pub sync_root_id: Option<SyncRootId>,

    // On non-Windows platforms, store a plain string identifier
    #[cfg(not(target_os = "windows"))]
    pub sync_root_id: Option<String>,

    /// List of gitignore-style patterns for files/directories to ignore during sync
    #[serde(default)]
    pub ignore_patterns: Vec<String>,

    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Credentials {
    pub access_token: Option<String>,
    pub refresh_token: String,
    pub refresh_expires: String,
    pub access_expires: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MountSyncStatus {
    InSync,
    Syncing,
    Paused,
    Error,
    Warnning,
}

/// Bitflags for mount status flags
#[derive(Debug, Clone, Copy, Default)]
pub struct MountStatusFlags(u8);

impl MountStatusFlags {
    const CREDENTIAL_EXPIRED: u8 = 1 << 0;
    const EVENT_PUSH_SUBSCRIBED: u8 = 1 << 1;

    /// Create a new MountStatusFlags with all flags cleared
    pub fn new() -> Self {
        Self(0)
    }

    /// Check if credentials have expired
    pub fn is_credential_expired(&self) -> bool {
        self.0 & Self::CREDENTIAL_EXPIRED != 0
    }

    /// Set the credential expired flag
    pub fn set_credential_expired(&mut self, expired: bool) {
        if expired {
            self.0 |= Self::CREDENTIAL_EXPIRED;
        } else {
            self.0 &= !Self::CREDENTIAL_EXPIRED;
        }
    }

    /// Check if event push is subscribed
    pub fn is_event_push_subscribed(&self) -> bool {
        self.0 & Self::EVENT_PUSH_SUBSCRIBED != 0
    }

    /// Set the event push subscribed flag
    pub fn set_event_push_subscribed(&mut self, subscribed: bool) {
        if subscribed {
            self.0 |= Self::EVENT_PUSH_SUBSCRIBED;
        } else {
            self.0 &= !Self::EVENT_PUSH_SUBSCRIBED;
        }
    }

    /// Get the raw bits value
    pub fn bits(&self) -> u8 {
        self.0
    }

    /// Create from raw bits
    pub fn from_bits(bits: u8) -> Self {
        Self(bits)
    }
}

type FsWatcher = Debouncer<RecommendedWatcher, RecommendedCache>;

pub struct Mount {
    pub config: Arc<RwLock<DriveConfig>>,
    provider: Mutex<Option<Box<dyn SyncProvider>>>,
    pub command_tx: mpsc::UnboundedSender<MountCommand>,
    command_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<MountCommand>>>>,
    processor_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    props_refresh_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    remote_event_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    manager_command_tx: mpsc::UnboundedSender<ManagerCommand>,
    fs_watcher: Mutex<Option<FsWatcher>>,
    pub(crate) sync_lock: Mutex<()>,
    pub cr_client: Arc<Client>,
    pub inventory: Arc<InventoryDb>,
    pub task_queue: Arc<TaskQueue>,
    pub id: String,
    pub event_blocker: EventBlocker,
    /// Compiled glob matcher for ignore patterns
    pub ignore_matcher: IgnoreMatcher,
    /// Status flags for the mount (credential expired, event push subscribed, etc.)
    status_flags: Mutex<MountStatusFlags>,
}

impl Mount {
    pub async fn new(
        config: DriveConfig,
        inventory: Arc<InventoryDb>,
        manager_command_tx: mpsc::UnboundedSender<ManagerCommand>,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        // initialize the client with the credentials
        let client_config = ClientConfig::new(config.instance_url.clone())
            .with_client_id(config.id.clone())
            .with_user_agent(crate::USER_AGENT);
        let mut cr_client = Client::new(client_config);
        let _ = cr_client
            .set_tokens_with_expiry(&Token {
                access_token: config.credentials.access_token.clone().unwrap_or_default(),
                refresh_token: config.credentials.refresh_token.clone(),
                access_expires: config
                    .credentials
                    .access_expires
                    .clone()
                    .unwrap_or_default(),
                refresh_expires: config.credentials.refresh_expires.clone(),
            })
            .await;
        let command_tx_clone: mpsc::UnboundedSender<MountCommand> = command_tx.clone();
        // Setup hooks to update the credentials in the config
        cr_client.set_on_credential_refreshed(Arc::new(move |token| {
            let command_tx = command_tx_clone.clone();
                Box::pin(async move {
                    let command = MountCommand::RefreshCredentials { credentials: token };
                    if let Err(e) = command_tx.send(command) {
                        tracing::error!(target: "drive::mounts", error = %e, "Failed to send RefreshCredentials command");
                    }
                })
        }));

        // Setup hook for credential invalid events (401, 40020, 40089)
        let command_tx_invalid = command_tx.clone();
        cr_client.set_on_credential_invalid(Arc::new(move || {
            let command_tx = command_tx_invalid.clone();
            Box::pin(async move {
                if let Err(e) = command_tx.send(MountCommand::CredentialInvalid) {
                    tracing::error!(target: "drive::mounts", error = %e, "Failed to send CredentialInvalid command");
                }
            })
        }));

        let cr_client_arc = Arc::new(cr_client);
        let id = config.id.clone();
        let queue_config = resolve_task_queue_config(&config);
        let task_queue = TaskQueue::new(
            id.clone(),
            cr_client_arc.clone(),
            inventory.clone(),
            queue_config,
            config.sync_path.clone(),
            config.remote_path.clone(),
        )
        .await;

        // Parse ignore patterns from config
        let sync_path = config.sync_path.clone();
        let ignore_matcher = match IgnoreMatcher::new(&config.ignore_patterns, sync_path.clone()) {
            Ok(matcher) => {
                if !matcher.is_empty() {
                    tracing::info!(
                        target: "drive::mounts",
                        id = %id,
                        pattern_count = matcher.len(),
                        "Loaded ignore patterns"
                    );
                }
                matcher
            }
            Err(e) => {
                tracing::warn!(
                    target: "drive::mounts",
                    id = %id,
                    error = %e,
                    "Failed to parse ignore patterns, using empty matcher"
                );
                IgnoreMatcher::empty(sync_path)
            }
        };

        Self {
            config: Arc::new(RwLock::new(config)),
            provider: Mutex::new(None),
            command_tx,
            command_rx: Arc::new(tokio::sync::Mutex::new(Some(command_rx))),
            processor_handle: Arc::new(tokio::sync::Mutex::new(None)),
            props_refresh_handle: Arc::new(tokio::sync::Mutex::new(None)),
            remote_event_handle: Arc::new(tokio::sync::Mutex::new(None)),
            cr_client: cr_client_arc,
            inventory,
            task_queue,
            id,
            manager_command_tx,
            fs_watcher: Mutex::new(None),
            sync_lock: Mutex::new(()),
            event_blocker: EventBlocker::new(),
            ignore_matcher,
            status_flags: Mutex::new(MountStatusFlags::new()),
        }
    }

    // Provider is behind a Mutex; access it via lock when needed.

    pub async fn get_config(&self) -> DriveConfig {
        self.config.read().await.clone()
    }

    /// Get the sync path for the drive
    pub async fn get_sync_path(&self) -> PathBuf {
        self.config.read().await.sync_path.clone()
    }

    /// Get a reference to the ignore matcher
    pub fn ignore_matcher(&self) -> &IgnoreMatcher {
        &self.ignore_matcher
    }

    /// Check if an absolute path should be ignored based on the configured ignore patterns.
    pub fn is_ignored<P: AsRef<Path>>(&self, path: P) -> bool {
        self.ignore_matcher.is_match(path)
    }

    /// Check if a filename should be ignored based on the configured ignore patterns.
    pub fn is_ignored_filename(&self, filename: &str) -> bool {
        self.ignore_matcher.is_match_filename(filename)
    }

    /// Get a copy of the current status flags
    pub async fn get_status_flags(&self) -> MountStatusFlags {
        *self.status_flags.lock().await
    }

    /// Set the credential expired flag.
    /// If the flag changes from false to true, sends a toast notification to remind user to re-authorize.
    pub async fn set_credential_expired(&self, expired: bool) {
        let should_notify = {
            let mut flags = self.status_flags.lock().await;
            let was_expired = flags.is_credential_expired();
            let notify = expired && !was_expired;
            flags.set_credential_expired(expired);
            notify
        };

        // Send toast outside of the lock to avoid potential deadlocks
        if should_notify {
            let config = self.config.read().await;
            let drive_name = config.name.clone();
            let drive_id = config.id.clone();
            drop(config);

            toast::send_token_expiry_toast(
                &drive_id,
                &t!("credentialExpiredTitle"),
                &t!("credentialExpiredMessage", "drive" => drive_name),
            );
        }
    }

    /// Set the event push subscribed flag
    pub async fn set_event_push_subscribed(&self, subscribed: bool) {
        self.status_flags
            .lock()
            .await
            .set_event_push_subscribed(subscribed);
    }

    pub fn task_queue(&self) -> Arc<TaskQueue> {
        self.task_queue.clone()
    }

    pub fn list_active_tasks(&self) -> Result<Vec<TaskRecord>> {
        self.task_queue.list_active_tasks()
    }

    pub async fn list_task_progress(&self) -> Vec<TaskProgress> {
        self.task_queue.ongoing_progress().await
    }

    pub async fn start(&self) -> Result<()> {
        // Platform-specific provider construction and startup
        #[cfg(target_os = "windows")]
        {
            use crate::drive::callback::CallbackHandler;
            use crate::platform::windows::WindowsSyncProvider;

            if !WindowsSyncProvider::is_supported()
                .map_err(|e| anyhow::anyhow!("Cloud Filter API is not supported: {}", e))?
            {
                return Err(anyhow::anyhow!("Cloud Filter API is not supported"));
            }

            let mut write_guard = self.config.write().await;

            // if sync root id is not set, generate one
            if write_guard.sync_root_id.is_none() {
                write_guard.sync_root_id = Some(
                    generate_sync_root_id(
                        &write_guard.instance_url,
                        &write_guard.name,
                        &write_guard.user_id,
                        &write_guard.sync_path,
                    )
                    .context("failed to generate sync root id")?,
                );
            }

            drop(write_guard);
            let config = self.config.read().await;

            let provider_config = build_provider_config(&config)?;

            let mut provider = WindowsSyncProvider::new();
            provider
                .start(&provider_config)
                .map_err(|e| anyhow::anyhow!("failed to start sync provider: {}", e))?;

            tracing::info!(target: "drive::mounts",sync_path = %config.sync_path.display(), id = %self.id, "Connecting to sync root");
            provider
                .connect(
                    &config.sync_path,
                    CallbackHandler::new(
                        self.command_tx.clone(),
                        self.id.clone(),
                        self.inventory.clone(),
                    ),
                )
                .context("failed to connect to sync root")?;

            *self.provider.lock().await = Some(Box::new(provider));
        }

        #[cfg(target_os = "linux")]
        {
            use crate::platform::linux::LinuxFuseProvider;

            let config = self.config.read().await;
            let provider_config = build_provider_config(&config)?;

            let mut provider = LinuxFuseProvider::new();
            provider.set_command_tx(self.command_tx.clone());
            provider.set_inventory(self.inventory.clone());
            provider
                .start(&provider_config)
                .map_err(|e| anyhow::anyhow!("failed to start sync provider: {}", e))?;

            *self.provider.lock().await = Some(Box::new(provider));
        }

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            return Err(anyhow::anyhow!(
                "Sync provider not supported on this platform"
            ));
        }

        // On Linux, FUSE callbacks handle all local changes — no FS watcher needed.
        // Using a file watcher on a FUSE mount would observe our own events, causing loops.
        #[cfg(not(target_os = "linux"))]
        self.start_fs_watcher().await?;
        Ok(())
    }

    pub async fn start_fs_watcher(&self) -> Result<()> {
        let command_tx = self.command_tx.clone();
        let mut debouncer = new_debouncer(
            Duration::from_secs(2),
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    let grouped_events = group_fs_events(events);
                    let command = MountCommand::ProcessFsEvents {
                        events: grouped_events,
                    };
                    if let Err(e) = command_tx.send(command) {
                        tracing::error!(target: "drive::mounts", error = %e, "Failed to send ProcessFsEvents command");
                    }
                }
                Err(errors) => {
                    tracing::error!(target: "drive::mounts", errors = ?errors, "Failed to watch FS")
                }
            },
        )?;

        tracing::info!(target: "drive::mounts", id = %self.id, "Watching FS");
        debouncer.watch(
            &self.config.read().await.sync_path,
            RecursiveMode::Recursive,
        )?;
        *self.fs_watcher.lock().await = Some(debouncer);
        Ok(())
    }

    pub async fn spawn_command_processor(&self, s: Arc<Self>) {
        // Spawn the command processor task
        let mut command_rx_guard = self.command_rx.lock().await;
        if let Some(command_rx) = command_rx_guard.take() {
            let mount_id = self.id.to_string();
            let handle = tokio::spawn(async move {
                Self::process_commands(s, mount_id, command_rx).await;
            });
            *self.processor_handle.lock().await = Some(handle);
        }
    }

    pub async fn spawn_remote_event_processor(&self, s: Arc<Self>) {
        let handle = tokio::spawn(async move {
            Self::process_remote_events(s).await;
        });
        *self.remote_event_handle.lock().await = Some(handle);
    }

    /// Process commands from OS threads asynchronously
    async fn process_commands(
        s: Arc<Self>,
        mount_id: String,
        mut command_rx: mpsc::UnboundedReceiver<MountCommand>,
    ) {
        tracing::info!(target: "drive::mounts", id = %mount_id, "Command processor started");

        while let Some(command) = command_rx.recv().await {
            tracing::trace!(target: "drive::mounts", id = %mount_id, command = ?command, "Processing command");

            match command {
                MountCommand::Rename {
                    source,
                    target,
                    response,
                } => {
                    let s_clone = s.clone();
                    let mount_id_clone = mount_id.clone();
                    spawn(async move {
                        let result = s_clone.rename(source, target).await;
                        if let Err(e) = result {
                            tracing::error!(target: "drive::mounts", id = %mount_id_clone, error = %e, "Failed to rename");
                            let _ = response.send(Err(e));
                            return;
                        }
                        tracing::debug!(target: "drive::mounts", id = %mount_id_clone, result = ?result, "Renamed");
                        let _ = response.send(result);
                    });
                }
                MountCommand::Sync { mode, local_paths } => {
                    let s_clone = s.clone();
                    let mount_id_clone = mount_id.clone();
                    spawn(async move {
                        if let Err(e) = s_clone.sync_paths(local_paths, mode).await {
                            tracing::error!(target: "drive::mounts", id = %mount_id_clone, error = %e, "Failed to sync paths");
                        }
                    });
                }
                MountCommand::FetchPlaceholders { path, response } => {
                    let s_clone = s.clone();
                    let mount_id_clone = mount_id.clone();
                    spawn(async move {
                        let result = s_clone.fetch_placeholders(path).await;
                        if let Err(e) = result {
                            tracing::error!(target: "drive::mounts", id = %mount_id_clone, error = %e, "Failed to fetch placeholders");
                            let _ = response.send(Err(e));
                            return;
                        }
                        tracing::debug!(target: "drive::mounts", id = %mount_id_clone, result = ?result, "Fetched placeholders");
                        let _ = response.send(result);
                    });
                }
                MountCommand::RefreshCredentials { credentials } => {
                    let mut config = s.config.write().await;
                    config.credentials.access_token = Some(credentials.access_token);
                    config.credentials.refresh_token = credentials.refresh_token;
                    config.credentials.refresh_expires = credentials.refresh_expires;
                    config.credentials.access_expires = Some(credentials.access_expires);

                    // Clear credential expired flag since we got new credentials
                    s.set_credential_expired(false).await;

                    // Notify manager to persist config
                    let command = ManagerCommand::PersistConfig;
                    if let Err(e) = s.manager_command_tx.send(command) {
                        tracing::error!(target: "drive::mounts", id = %mount_id, error = %e, "Failed to send PersistConfig command");
                    }
                    drop(config);
                }
                MountCommand::CredentialInvalid => {
                    tracing::warn!(target: "drive::mounts", id = %mount_id, "Credential invalid, marking as expired");
                    s.set_credential_expired(true).await;
                }
                MountCommand::FetchData {
                    path,
                    writer,
                    range,
                    response,
                } => {
                    let s_clone = s.clone();
                    let mount_id_clone = mount_id.clone();
                    spawn(async move {
                        let result = s_clone.fetch_data(path, writer, range).await;
                        if let Err(e) = result {
                            tracing::error!(target: "drive::mounts", id = %mount_id_clone, error = ?e, "Failed to fetch data");
                            let _ = response.send(Err(e));
                            return;
                        }
                        tracing::debug!(target: "drive::mounts", id = %mount_id_clone, result = ?result, "Fetched data");
                        let _ = response.send(result);
                    });
                }
                MountCommand::ProcessFsEvents { events } => {
                    let s_clone = s.clone();
                    spawn(async move {
                        let _ = s_clone.process_fs_events(events).await;
                    });
                }
                MountCommand::Renamed {
                    source,
                    destination,
                } => {
                    let s_clone = s.clone();
                    let mount_id_clone = mount_id.clone();
                    spawn(async move {
                        if let Err(e) = s_clone.rename_completed(source, destination).await {
                            tracing::error!(target: "drive::mounts", id = %mount_id_clone, error = ?e, "Failed to rename completed");
                            return;
                        }
                    });
                }
            }
        }

        tracing::info!(target: "drive::mounts", id = %mount_id, "Command processor stopped");
    }

    pub async fn delete(&self) -> Result<()> {
        self.shutdown().await;

        // Stop the provider and unregister
        if let Some(ref mut provider) = *self.provider.lock().await {
            provider
                .stop()
                .map_err(|e| anyhow::anyhow!("failed to stop provider: {}", e))?;
            provider
                .unregister()
                .map_err(|e| anyhow::anyhow!("failed to unregister provider: {}", e))?;
        }
        *self.provider.lock().await = None;

        self.task_queue.shutdown().await;

        if let Err(e) = self.inventory.nuke_drive(&self.id) {
            tracing::error!(target: "drive::mounts", id=%self.id, error=%e, "Failed to nuke drive");
        }

        Ok(())
    }

    pub async fn shutdown(&self) {
        tracing::info!(target: "drive::mounts", id=%self.id, "Shutting down Mount");

        // Stop the remote event listener
        if let Some(handle) = self.remote_event_handle.lock().await.take() {
            tracing::debug!(target: "drive::mounts", id=%self.id, "Stopping remote event listener");
            handle.abort();
        }

        if let Some(fs_watcher) = self.fs_watcher.lock().await.take() {
            tracing::debug!(target: "drive::mounts", id=%self.id, "Stopping FS watcher");
            drop(fs_watcher);
        }

        // Close the command channel to signal the processor task to stop
        drop(self.command_tx.clone());

        // Wait for the processor task to finish
        if let Some(handle) = self.processor_handle.lock().await.take() {
            tracing::debug!(target: "drive::mounts", id=%self.id, "Waiting for command processor to finish");
            handle.abort();
        }

        // Stop the props refresh task
        if let Some(handle) = self.props_refresh_handle.lock().await.take() {
            tracing::debug!(target: "drive::mounts", id=%self.id, "Stopping props refresh task");
            handle.abort();
        }
    }

    /// Spawn the periodic props refresh task
    pub async fn spawn_props_refresh_task(self: &Arc<Self>) {
        let mount = self.clone();
        let mount_id = self.id.clone();

        // Check if props exist, if not, trigger immediate refresh
        let should_refresh_immediately = match self.inventory.has_drive_props(&self.id) {
            Ok(has_props) => !has_props,
            Err(e) => {
                tracing::warn!(target: "drive::mounts", id=%mount_id, error=%e, "Failed to check drive props existence");
                true // Refresh if we can't check
            }
        };

        let handle = spawn(async move {
            // Refresh interval: 5 minutes
            let refresh_interval = Duration::from_secs(300);

            // If no props exist, refresh immediately
            if should_refresh_immediately {
                tracing::info!(target: "drive::mounts", id=%mount_id, "No drive props found, triggering immediate refresh");
                if let Err(e) = mount.refresh_drive_props().await {
                    tracing::error!(target: "drive::mounts", id=%mount_id, error=%e, "Failed to refresh drive props");
                }
            }

            loop {
                tokio::time::sleep(refresh_interval).await;
                tracing::debug!(target: "drive::mounts", id=%mount_id, "Periodic props refresh triggered");

                if let Err(e) = mount.refresh_drive_props().await {
                    tracing::error!(target: "drive::mounts", id=%mount_id, error=%e, "Failed to refresh drive props");
                }
            }
        });

        *self.props_refresh_handle.lock().await = Some(handle);
    }

    /// Refresh drive props from the API (capacity and user settings)
    pub async fn refresh_drive_props(&self) -> Result<()> {
        tracing::debug!(target: "drive::mounts", id=%self.id, "Refreshing drive props");

        let mut update = DrivePropsUpdate::default();

        // Fetch user capacity
        match self.cr_client.get_user_capacity().await {
            Ok(capacity) => {
                tracing::debug!(target: "drive::mounts", id=%self.id, used=%capacity.used, total=%capacity.total, "Fetched user capacity");
                update = update.with_capacity(capacity);
            }
            Err(e) => {
                tracing::warn!(target: "drive::mounts", id=%self.id, error=%e, "Failed to fetch user capacity");
            }
        }

        // Fetch user settings
        match self.cr_client.get_user_storage_policies().await {
            Ok(policies) => {
                tracing::debug!(target: "drive::mounts", id=%self.id, "Fetched user storage policies");
                update = update.with_storage_policies(policies);
            }
            Err(e) => {
                tracing::warn!(target: "drive::mounts", id=%self.id, error=%e, "Failed to fetch user storage policies");
            }
        }

        // Save to database if we have any updates
        if !update.is_empty() {
            self.inventory
                .upsert_drive_props(&self.id, update)
                .context("Failed to save drive props")?;
            tracing::info!(target: "drive::mounts", id=%self.id, "Drive props updated successfully");
        }

        Ok(())
    }

    /// Get cached drive props from the database
    pub fn get_drive_props(&self) -> Result<Option<crate::inventory::DriveProps>> {
        self.inventory
            .get_drive_props(&self.id)
            .context("Failed to get drive props")
    }
}

/// Build a ProviderConfig from a DriveConfig.
fn build_provider_config(config: &DriveConfig) -> Result<ProviderConfig> {
    // Serialize the sync root ID to a string for the platform-agnostic config
    let provider_id = {
        #[cfg(target_os = "windows")]
        {
            config
                .sync_root_id
                .as_ref()
                .map(|id| serde_json::to_value(id).ok())
                .flatten()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_default()
        }
        #[cfg(not(target_os = "windows"))]
        {
            config.sync_root_id.clone().unwrap_or_default()
        }
    };

    Ok(ProviderConfig {
        sync_path: config.sync_path.clone(),
        display_name: config.name.clone(),
        icon_path: config.icon_path.clone(),
        provider_id,
        instance_url: config.instance_url.clone(),
        user_id: config.user_id.clone(),
        remote_path: config.remote_path.clone(),
        recycle_bin_uri: recycle_bin_url(config)
            .unwrap_or_else(|_| "https://cloudreve.org".to_string()),
    })
}

#[cfg(target_os = "windows")]
fn generate_sync_root_id(
    instance_url: &str,
    _account_name: &str,
    user_id: &str,
    sync_path: &PathBuf,
) -> Result<SyncRootId> {
    // Parse the instance URL to get the hostname
    let url = Url::parse(instance_url)?;
    let hostname = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid URL: no host found"))?;

    // Generate a SHA-256 hash of the hostname
    let mut hasher = Sha256::new();
    hasher.update(hostname.as_bytes());
    hasher.update(sync_path.to_string_lossy().as_bytes());
    let hash_result = hasher.finalize();

    // Convert hash to hex string and truncate to reasonable length
    let hash_hex = format!("{:x}", hash_result);
    let provider_name = format!("cloudreve{}", &hash_hex[..16]);

    // Build the sync root ID
    let sync_root_id = SyncRootIdBuilder::new(provider_name)
        .user_security_id(SecurityId::current_user()?)
        .account_name(user_id)
        .build();

    Ok(sync_root_id)
}

fn resolve_task_queue_config(config: &DriveConfig) -> TaskQueueConfig {
    let concurrency = config
        .extra
        .get("task_queue_max_concurrency")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .filter(|value| *value > 0)
        .unwrap_or(2);

    TaskQueueConfig {
        max_concurrent: concurrency,
    }
}
