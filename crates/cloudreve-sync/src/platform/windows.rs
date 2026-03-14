use std::ffi::OsString;
use std::path::Path;

use crate::cfapi::filter::ticket;
use crate::cfapi::metadata::Metadata;
use crate::cfapi::placeholder::{ConvertOptions, LocalFileInfo, OpenOptions, UpdateOptions as CfapiUpdateOptions};
use crate::cfapi::placeholder_file::PlaceholderFile;
use crate::cfapi::root::{
    Connection, HydrationType, PopulationType, SecurityId, Session, SyncRootId, SyncRootIdBuilder,
    SyncRootInfo,
};
use crate::cfapi::utility::WriteAt;
use crate::drive::callback::CallbackHandler;

use anyhow::Context;
use nt_time::FileTime;
use sha2::{Digest, Sha256};
use url::Url;
use widestring::U16CString;
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Storage::EnhancedStorage::PKEY_LastSyncError;
use windows::Win32::System::Variant::VT_UI4;
use windows::Win32::UI::Shell::{
    IShellItem2, PropertiesSystem::{GPS_EXTRINSICPROPERTIESONLY, GPS_READWRITE, IPropertyStore},
    SHCNE_ATTRIBUTES, SHCNE_DELETE,
    SHCreateItemFromParsingName, SHCNE_ID, SHCNF_PATHW, SHChangeNotify,
};
use windows::Storage::Provider::StorageProviderSyncRootManager;
use windows::core::{HSTRING, PCWSTR};
use windows_core::PROPVARIANT;

use super::error::{PlatformError, PlatformResult};
use super::provider::{ProviderConfig, SyncProvider, UpdateOptions};
use super::types::{FileMetadataPlatform, HydrationWriter, LocalFileState, PlaceholderEntry};

/// Windows Cloud Filter API implementation of `SyncProvider`.
pub struct WindowsSyncProvider {
    connection: Option<Connection<CallbackHandler>>,
    sync_root_id: Option<SyncRootId>,
}

impl WindowsSyncProvider {
    pub fn new() -> Self {
        Self {
            connection: None,
            sync_root_id: None,
        }
    }

    /// Get or generate the SyncRootId from a ProviderConfig.
    /// If `provider_id` is set, deserialize; otherwise generate a new one.
    pub fn resolve_sync_root_id(config: &ProviderConfig) -> anyhow::Result<SyncRootId> {
        if !config.provider_id.is_empty() {
            // Reconstruct SyncRootId from its serialized string form (same as Deserialize impl)
            Ok(SyncRootId(HSTRING::from(&config.provider_id)))
        } else {
            generate_sync_root_id(
                &config.instance_url,
                &config.user_id,
                &config.sync_path,
            )
        }
    }

    /// Get a reference to the active connection (if started).
    pub fn connection(&self) -> Option<&Connection<CallbackHandler>> {
        self.connection.as_ref()
    }

    /// Connect to an already-registered sync root with a callback handler.
    /// This is called by `Mount::start()` to establish the cfapi connection.
    pub fn connect(
        &mut self,
        sync_path: &Path,
        handler: CallbackHandler,
    ) -> anyhow::Result<()> {
        let connection = Session::new()
            .connect(sync_path, handler)
            .context("failed to connect to sync root")?;
        self.connection = Some(connection);
        Ok(())
    }
}

impl SyncProvider for WindowsSyncProvider {
    fn start(&mut self, config: &ProviderConfig) -> PlatformResult<()> {
        let sync_root_id = Self::resolve_sync_root_id(config)
            .map_err(|e| PlatformError::Failed(format!("failed to resolve sync root id: {}", e)))?;

        // Register sync root if not already registered
        let is_registered = sync_root_id.is_registered()
            .map_err(|e| PlatformError::Failed(format!("failed to check registration: {}", e)))?;

        if !is_registered {
            let mut sync_root_info = SyncRootInfo::default();
            sync_root_info.set_display_name(config.display_name.clone());
            sync_root_info.set_hydration_type(HydrationType::Full);
            sync_root_info.set_population_type(PopulationType::Full);
            if let Some(icon_path) = config.icon_path.as_ref() {
                sync_root_info.set_icon(format!("{},0", icon_path));
            }
            sync_root_info.set_version("1.0.0");
            sync_root_info
                .set_recycle_bin_uri(&config.recycle_bin_uri)
                .map_err(|e| PlatformError::Failed(format!("failed to set recycle bin uri: {}", e)))?;
            sync_root_info
                .set_path(&config.sync_path)
                .map_err(|e| PlatformError::Failed(format!("failed to set sync root path: {}", e)))?;
            sync_root_info.add_custom_state(t!("shared").as_ref(), 1)
                .map_err(|e| PlatformError::Failed(format!("failed to add custom state: {}", e)))?;
            sync_root_info.add_custom_state(t!("accessible").as_ref(), 2)
                .map_err(|e| PlatformError::Failed(format!("failed to add custom state: {}", e)))?;
            sync_root_id
                .register(sync_root_info)
                .map_err(|e| PlatformError::Failed(format!("failed to register sync root: {}", e)))?;
        }

        // Add to search indexer
        if let Err(e) = sync_root_id.index() {
            tracing::warn!(target: "platform::windows", error = %e, "Failed to add sync root to search indexer");
        }

        self.sync_root_id = Some(sync_root_id);
        Ok(())
    }

    fn stop(&mut self) -> PlatformResult<()> {
        if let Some(ref connection) = self.connection {
            connection.disconnect()
                .map_err(|e| PlatformError::Failed(format!("failed to disconnect: {}", e)))?;
        }
        self.connection = None;
        Ok(())
    }

    fn unregister(&mut self) -> PlatformResult<()> {
        if let Some(ref sync_root_id) = self.sync_root_id {
            sync_root_id.unregister()
                .map_err(|e| PlatformError::Failed(format!("failed to unregister sync root: {}", e)))?;
        }
        self.sync_root_id = None;
        Ok(())
    }

    fn get_file_state(&self, path: &Path) -> LocalFileState {
        match LocalFileInfo::from_path(path) {
            Ok(info) => local_file_info_to_state(&info),
            Err(_) => LocalFileState::missing(),
        }
    }

    fn create_placeholders(
        &self,
        parent: &Path,
        entries: &mut [PlaceholderEntry],
    ) -> PlatformResult<()> {
        let placeholders: Vec<PlaceholderFile> = entries
            .iter()
            .map(|entry| entry_to_cfapi_placeholder(entry))
            .collect::<Result<Vec<_>, _>>()?;

        // PlaceholderFile::create takes self by value
        for placeholder in placeholders {
            placeholder.create::<&Path>(parent)
                .map_err(|e| PlatformError::Failed(format!("failed to create placeholder: {}", e)))?;
        }

        Ok(())
    }

    fn update_placeholder(
        &self,
        path: &Path,
        meta: &FileMetadataPlatform,
        _etag: &str,
        options: UpdateOptions,
    ) -> PlatformResult<()> {
        let metadata = metadata_to_cfapi(meta)
            .map_err(|e| PlatformError::Failed(format!("failed to convert metadata: {}", e)))?;

        let mut update_options = CfapiUpdateOptions::default()
            .mark_in_sync()
            .metadata(metadata);

        if options.dehydrate {
            update_options = update_options.dehydrate();
        }
        if options.mark_no_children {
            update_options = update_options.has_no_children();
        }

        let is_dir = meta.is_directory;
        let mut handle = if options.dehydrate {
            OpenOptions::new()
                .write_access()
                .exclusive()
                .open(path)
                .map_err(|e| PlatformError::Failed(format!("failed to open for dehydration: {}", e)))?
        } else {
            match is_dir {
                true => OpenOptions::new().open(path),
                false => OpenOptions::new().open_win32(path),
            }
            .map_err(|e| PlatformError::Failed(format!("failed to open placeholder: {}", e)))?
        };

        handle
            .update(update_options, None)
            .map_err(|e| PlatformError::Failed(format!("failed to update placeholder: {}", e)))?;

        Ok(())
    }

    fn convert_to_placeholder(
        &self,
        path: &Path,
        etag: &str,
        is_directory: bool,
    ) -> PlatformResult<()> {
        let blob = OsString::from(etag).into_encoded_bytes();

        let mut handle = match is_directory {
            true => OpenOptions::new().open(path),
            false => OpenOptions::new().open_win32(path),
        }
        .map_err(|e| PlatformError::Failed(format!("failed to open for conversion: {}", e)))?;

        handle
            .convert_to_placeholder(
                ConvertOptions::default().mark_in_sync().blob(blob),
                None,
            )
            .map_err(|e| PlatformError::Failed(format!("failed to convert to placeholder: {}", e)))?;

        Ok(())
    }

    fn create_placeholder(&self, parent: &Path, entry: &PlaceholderEntry) -> PlatformResult<()> {
        let placeholder = entry_to_cfapi_placeholder(entry)?;
        // PlaceholderFile::create takes self by value
        placeholder.create::<&Path>(parent)
            .map_err(|e| PlatformError::Failed(format!("failed to create placeholder: {}", e)))?;
        Ok(())
    }

    fn delete_placeholder(&self, path: &Path) -> PlatformResult<()> {
        if path.is_dir() {
            std::fs::remove_dir_all(path)
                .map_err(|e| PlatformError::Failed(format!("failed to delete directory: {}", e)))?;
        } else if path.exists() {
            std::fs::remove_file(path)
                .map_err(|e| PlatformError::Failed(format!("failed to delete file: {}", e)))?;
        }

        notify_shell_change_internal(path, SHCNE_DELETE)?;
        Ok(())
    }

    fn notify_change(&self, path: &Path) -> PlatformResult<()> {
        notify_shell_change_internal(path, SHCNE_ATTRIBUTES)
    }

    fn set_error_state(&self, path: &Path, has_error: bool) -> PlatformResult<()> {
        // Check if file is a placeholder first
        let state = self.get_file_state(path);
        if !state.is_placeholder {
            return Ok(());
        }

        let path_wide = U16CString::from_os_str(path)
            .map_err(|e| PlatformError::Failed(format!("failed to convert path: {}", e)))?;

        unsafe {
            let item: IShellItem2 = SHCreateItemFromParsingName(PCWSTR(path_wide.as_ptr()), None)
                .map_err(|e| PlatformError::Failed(format!("failed to create shell item: {}", e)))?;

            let flags = GPS_READWRITE | GPS_EXTRINSICPROPERTIESONLY;
            let property_store: IPropertyStore = item
                .GetPropertyStore(flags)
                .map_err(|e| PlatformError::Failed(format!("failed to get property store: {}", e)))?;

            let prop_var = if has_error {
                let mut pv = PROPVARIANT::default().as_raw().clone();
                pv.Anonymous.Anonymous.vt = VT_UI4.0;
                pv.Anonymous.Anonymous.Anonymous.ulVal = E_FAIL.0 as u32;
                PROPVARIANT::from_raw(pv)
            } else {
                let pv = PROPVARIANT::default().as_raw().clone();
                PROPVARIANT::from_raw(pv)
            };

            property_store
                .SetValue(&PKEY_LastSyncError, &prop_var)
                .map_err(|e| PlatformError::Failed(format!("failed to set error property: {}", e)))?;

            property_store
                .Commit()
                .map_err(|e| PlatformError::Failed(format!("failed to commit property store: {}", e)))?;
        }

        Ok(())
    }

    fn is_supported() -> PlatformResult<bool> {
        StorageProviderSyncRootManager::IsSupported()
            .map_err(|e| PlatformError::Failed(format!("failed to check cfapi support: {}", e)))
    }

    fn provider_id(&self) -> Option<&str> {
        // SyncRootId doesn't directly expose as &str, so return None.
        // The caller should use DriveConfig.sync_root_id for serialization.
        None
    }
}

/// Wraps cfapi's `ticket::FetchData` as a platform-agnostic `HydrationWriter`.
pub struct WindowsHydrationWriter(pub ticket::FetchData);

impl HydrationWriter for WindowsHydrationWriter {
    fn write_at(&self, buf: &[u8], offset: u64) -> anyhow::Result<()> {
        self.0.write_at(buf, offset)
            .map_err(|e| anyhow::anyhow!("cfapi write_at failed: {:?}", e))
    }

    fn report_progress(&self, total: u64, completed: u64) -> anyhow::Result<()> {
        self.0.report_progress(total, completed)
            .map_err(|e| anyhow::anyhow!("cfapi report_progress failed: {:?}", e))
    }
}

// --- Helper functions ---

fn local_file_info_to_state(info: &LocalFileInfo) -> LocalFileState {
    use crate::cfapi::placeholder::PinState;

    LocalFileState {
        exists: info.exists,
        is_directory: info.is_directory,
        is_placeholder: info.is_placeholder(),
        is_hydrated: !info.partial_on_disk(),
        is_folder_populated: info.is_folder_populated(),
        in_sync: info.in_sync(),
        is_pinned: info.pin_state == PinState::Pinned,
        is_unpinned: info.pin_state == PinState::Unpinned,
        size: info.file_size,
    }
}

fn entry_to_cfapi_placeholder(entry: &PlaceholderEntry) -> PlatformResult<PlaceholderFile> {
    let metadata = metadata_to_cfapi(&entry.metadata)?;
    let blob = OsString::from(&entry.etag).into_encoded_bytes();

    let mut pf = PlaceholderFile::new(&entry.relative_name)
        .metadata(metadata)
        .overwrite()
        .blob(blob);

    if entry.mark_in_sync {
        pf = pf.mark_in_sync();
    }

    Ok(pf)
}

fn metadata_to_cfapi(meta: &FileMetadataPlatform) -> PlatformResult<Metadata> {
    let created = FileTime::from_unix_time(meta.created_at)
        .map_err(|e| PlatformError::Failed(format!("invalid created_at timestamp: {}", e)))?;
    let modified = FileTime::from_unix_time(meta.modified_at)
        .map_err(|e| PlatformError::Failed(format!("invalid modified_at timestamp: {}", e)))?;

    Ok(match meta.is_directory {
        true => Metadata::directory(),
        false => Metadata::file(),
    }
    .size(meta.size)
    .changed(modified)
    .written(modified)
    .created(created))
}

fn notify_shell_change_internal(path: &Path, event: SHCNE_ID) -> PlatformResult<()> {
    let utf16_path = U16CString::from_os_str(path)
        .map_err(|e| PlatformError::Failed(format!("failed to encode path: {}", e)))?;
    unsafe {
        SHChangeNotify(
            event,
            SHCNF_PATHW,
            Some(utf16_path.as_ptr() as *const _),
            None,
        );
    }
    Ok(())
}

fn generate_sync_root_id(
    instance_url: &str,
    user_id: &str,
    sync_path: &Path,
) -> anyhow::Result<SyncRootId> {
    let url = Url::parse(instance_url)?;
    let hostname = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid URL: no host found"))?;

    let mut hasher = Sha256::new();
    hasher.update(hostname.as_bytes());
    hasher.update(sync_path.to_string_lossy().as_bytes());
    let hash_result = hasher.finalize();

    let hash_hex = format!("{:x}", hash_result);
    let provider_name = format!("cloudreve{}", &hash_hex[..16]);

    let sync_root_id = SyncRootIdBuilder::new(provider_name)
        .user_security_id(SecurityId::current_user()?)
        .account_name(user_id)
        .build();

    Ok(sync_root_id)
}
