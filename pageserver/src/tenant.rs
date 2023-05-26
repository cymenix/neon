//!
//! Timeline repository implementation that keeps old data in files on disk, and
//! the recent changes in memory. See tenant/*_layer.rs files.
//! The functions here are responsible for locating the correct layer for the
//! get/put call, walking back the timeline branching history as needed.
//!
//! The files are stored in the .neon/tenants/<tenant_id>/timelines/<timeline_id>
//! directory. See docs/pageserver-storage.md for how the files are managed.
//! In addition to the layer files, there is a metadata file in the same
//! directory that contains information about the timeline, in particular its
//! parent timeline, and the last LSN that has been written to disk.
//!

use anyhow::{bail, Context};
use futures::FutureExt;
use pageserver_api::models::TimelineState;
use remote_storage::DownloadError;
use remote_storage::GenericRemoteStorage;
use storage_broker::BrokerClientChannel;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::*;
use utils::crashsafe::path_with_suffix_extension;

use std::cmp::min;
use std::collections::hash_map::Entry;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::fs::DirEntry;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::ops::Bound::Included;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use self::config::TenantConf;
use self::metadata::TimelineMetadata;
use self::remote_timeline_client::RemoteTimelineClient;
use self::timeline::EvictionTaskTenantState;
use crate::config::PageServerConf;
use crate::context::{DownloadBehavior, RequestContext};
use crate::import_datadir;
use crate::is_uninit_mark;
use crate::metrics::{remove_tenant_metrics, TENANT_STATE_METRIC, TENANT_SYNTHETIC_SIZE_METRIC};
use crate::repository::GcResult;
use crate::task_mgr;
use crate::task_mgr::TaskKind;
use crate::tenant::config::TenantConfOpt;
use crate::tenant::metadata::load_metadata;
use crate::tenant::remote_timeline_client::index::IndexPart;
use crate::tenant::remote_timeline_client::MaybeDeletedIndexPart;
use crate::tenant::remote_timeline_client::PersistIndexPartWithDeletedFlagError;
use crate::tenant::storage_layer::DeltaLayer;
use crate::tenant::storage_layer::ImageLayer;
use crate::tenant::storage_layer::Layer;

use crate::virtual_file::VirtualFile;
use crate::walredo::PostgresRedoManager;
use crate::walredo::WalRedoManager;
use crate::TEMP_FILE_SUFFIX;
pub use pageserver_api::models::TenantState;

use toml_edit;
use utils::{
    crashsafe,
    id::{TenantId, TimelineId},
    lsn::{Lsn, RecordLsn},
};

pub mod blob_io;
pub mod block_io;
pub mod disk_btree;
pub(crate) mod ephemeral_file;
pub mod layer_map;

pub mod metadata;
mod par_fsync;
mod remote_timeline_client;
pub mod storage_layer;

pub mod config;
pub mod mgr;
pub mod tasks;
pub mod upload_queue;

mod timeline;

pub mod size;

pub(crate) use timeline::debug_assert_current_span_has_tenant_and_timeline_id;
pub use timeline::{
    LocalLayerInfoForDiskUsageEviction, LogicalSizeCalculationCause, PageReconstructError, Timeline,
};

// re-export this function so that page_cache.rs can use it.
pub use crate::tenant::ephemeral_file::writeback as writeback_ephemeral_file;

// re-export for use in storage_sync.rs
pub use crate::tenant::metadata::save_metadata;

// re-export for use in walreceiver
pub use crate::tenant::timeline::WalReceiverInfo;

/// Parts of the `.neon/tenants/<tenant_id>/timelines/<timeline_id>` directory prefix.
pub const TIMELINES_SEGMENT_NAME: &str = "timelines";

pub const TENANT_ATTACHING_MARKER_FILENAME: &str = "attaching";

///
/// Tenant consists of multiple timelines. Keep them in a hash table.
///
pub struct Tenant {
    // Global pageserver config parameters
    pub conf: &'static PageServerConf,

    /// The value creation timestamp, used to measure activation delay, see:
    /// <https://github.com/neondatabase/neon/issues/4025>
    loading_started_at: Instant,

    state: watch::Sender<TenantState>,

    // Overridden tenant-specific config parameters.
    // We keep TenantConfOpt sturct here to preserve the information
    // about parameters that are not set.
    // This is necessary to allow global config updates.
    tenant_conf: Arc<RwLock<TenantConfOpt>>,

    tenant_id: TenantId,
    pub(super) timelines: Mutex<HashMap<TimelineId, Arc<Timeline>>>,
    // This mutex prevents creation of new timelines during GC.
    // Adding yet another mutex (in addition to `timelines`) is needed because holding
    // `timelines` mutex during all GC iteration
    // may block for a long time `get_timeline`, `get_timelines_state`,... and other operations
    // with timelines, which in turn may cause dropping replication connection, expiration of wait_for_lsn
    // timeout...
    gc_cs: tokio::sync::Mutex<()>,
    walredo_mgr: Arc<dyn WalRedoManager + Send + Sync>,

    // provides access to timeline data sitting in the remote storage
    remote_storage: Option<GenericRemoteStorage>,

    /// Cached logical sizes updated updated on each [`Tenant::gather_size_inputs`].
    cached_logical_sizes: tokio::sync::Mutex<HashMap<(TimelineId, Lsn), u64>>,
    cached_synthetic_tenant_size: Arc<AtomicU64>,

    eviction_task_tenant_state: tokio::sync::Mutex<EvictionTaskTenantState>,
}

/// Similar to `Arc::ptr_eq`, but only compares the object pointers, not vtables.
#[inline(always)]
pub(crate) fn compare_arced_timeline(left: &Arc<Timeline>, right: &Arc<Timeline>) -> bool {
    // See: https://github.com/rust-lang/rust/issues/103763
    // See: https://github.com/rust-lang/rust/pull/106450
    let left = Arc::as_ptr(left) as *const ();
    let right = Arc::as_ptr(right) as *const ();
    left == right
}

#[derive(Debug, thiserror::Error)]
enum StartCreatingTimelineError {
    /// If this variant is returned, no on-disk changes have been made for this timeline yet
    /// and all in-memory changes have been rolled back.
    #[error("timeline {timeline_id} already exists ({existing_state:?})")]
    AlreadyExists {
        timeline_id: TimelineId,
        existing_state: &'static str,
    },
    /// If this variant is returned, a placeholder timeline in `TimelineState::Creating` is present
    /// in the `Tenant::timelines` map, and there may or may not be on-disk state for the timeline.
    ///
    /// The correct way to handle this error is to
    /// 1. log the error and
    /// 2. keep the placeholder timeline in memory and
    /// 3. instruct the operator to restart pageserver / ignore+load the tenant.
    ///
    /// The restart / ignore+load operation will resume the cleanup.
    ///
    /// TODO: ignore + load (schedule_local_tenant_processing) need to check for presence of uninit mark.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub(crate) struct CreatingTimelineGuard<'t> {
    owning_tenant: &'t Tenant,
    timeline_id: TimelineId,
    timeline_path: PathBuf,
    uninit_mark_path: PathBuf,
    placeholder_timeline: Arc<Timeline>,
}

impl<'t> CreatingTimelineGuard<'t> {
    /// If this returns an error, the placeholder may or may not be gone from the FS but it's not guaranteed that the removal is durable yet.
    /// The correct way forward in this case is to leave the placeholder tenant in place and require manual intervention.
    /// A log message instructing the operator how to do it is logged.
    ///
    /// TODO Pageserver restart in response to an error may result in the timeline loading correctly, but technically, the uninit marker removal might not be durable yet.
    pub(crate) fn creation_complete_remove_uninit_marker_and_get_placeholder_timeline(
        self,
    ) -> anyhow::Result<Arc<Timeline>> {
        let doit = || {
            let uninit_mark_exists = self
                .uninit_mark_path
                .try_exists()
                .expect("if the filesystem can't answer, let's just die");
            if uninit_mark_exists {
                std::fs::remove_file(&self.uninit_mark_path).context("remove uninit mark")?;
            }
            // always fsync, we might be a restarted pageserver
            let uninit_mark_path_parent = self
                .uninit_mark_path
                .parent()
                .expect("uninit mark always has parent");
            crashsafe::fsync(uninit_mark_path_parent).with_context(|| {
                format!("fsync uninit mark parent dir {uninit_mark_path_parent:?}")
            })?;
            anyhow::Ok(())
        };
        match doit() {
            Ok(()) => Ok(self.placeholder_timeline),
            Err(e) => {
                error!("failed to remove uninit mark, timeline will remain in memory and be undeletable, ignore+fix_manually+load the affected tenant: {:?}", e);
                Err(e.context("remove unint mark"))
            }
        }
    }

    /// Tries to remove the creating timeline's timeline dir and uninit marker.
    /// If this suceeeds, the placeholder timeline is removed from the owning tenant's timelines map, enabling a clean retry.
    /// If the filesystem operations fail, the placeholder timeline will remain in the owning tenant's timelines map, preventing retries.
    /// In that case, we log an error and instruct the operator to manually remove the timeline dir and uninit marker.
    /// Pageserver restart will re-attempt the cleanup as well.
    pub(crate) fn creation_failed(self) {
        // remove timeline dir and uninit mark before removing from memory, so, subsequent attempts won't get surprised if we fail to remove on-disk state
        let doit = || {
            let uninit_mark_exists = self
                .uninit_mark_path
                .try_exists()
                .expect("if the filesystem can't answer, let's just die");
            assert!(
                uninit_mark_exists,
                "uninit mark should exist at {:?}",
                self.uninit_mark_path
            );
            if self.timeline_path.exists() {
                std::fs::remove_dir_all(&self.timeline_path).context("remove timeline dir")?;
            }
            // always fsync before removal, we might be a restarted pageserver
            let timeline_dir_parent = self
                .timeline_path
                .parent()
                .expect("timeline dir always has parent");
            crashsafe::fsync(timeline_dir_parent).with_context(|| {
                format!("fsync timeline dir parent dir {timeline_dir_parent:?}")
            })?;
            std::fs::remove_file(&self.uninit_mark_path).context("remove uninit mark")?;
            let uninit_mark_path_parent = self
                .uninit_mark_path
                .parent()
                .expect("uninit mark always has parent");
            crashsafe::fsync(uninit_mark_path_parent).with_context(|| {
                format!("fsync uninit mark parent dir {uninit_mark_path_parent:?}")
            })?;
            anyhow::Ok(())
        };
        match doit() {
            Ok(()) => {
                self.remove_placeholder_timeline_object_from_inmemory_map();
            }
            Err(e) => {
                error!(timeline_id=%self.timeline_id, error=?e, "failure during cleanup of creating timeline, it will remain in memory and be undeletable, ignore+fix_manually+load the affected tenant");
            }
        }
    }

    fn remove_placeholder_timeline_object_from_inmemory_map(&self) {
        let Ok(mut timelines) = self.owning_tenant.timelines.lock() else {
            error!("timelines lock poisoned, not removing placeholder timeline");
            return;
        };
        match timelines.entry(self.timeline_id) {
            Entry::Occupied(entry) => {
                if compare_arced_timeline(&self.placeholder_timeline, entry.get()) {
                    info!("removing placeholder timeline from in-memory map");
                    entry.remove();
                } else {
                    // TODO do we really need this branch?
                    info!(
                        "placeholder timeline was replaced with another timeline, not removing it"
                    );
                }
            }
            Entry::Vacant(_) => {
                error!("either placeholder timeline or real timeline should be present in the timelines map");
            }
        }
    }
}

/// Newtype to avoid conusing local variables that are both Arc<Timelien>
struct AncestorArg(Option<Arc<Timeline>>);

impl AncestorArg {
    pub fn ancestor(ancestor: Arc<Timeline>) -> Self {
        Self(Some(ancestor))
    }
    pub fn no_ancestor() -> Self {
        Self(None)
    }
}

// We should not blindly overwrite local metadata with remote one.
// For example, consider the following case:
//     Image layer is flushed to disk as a new delta layer, we update local metadata and start upload task but after that
//     pageserver crashes. During startup we'll load new metadata, and then reset it
//     to the state of remote one. But current layermap will have layers from the old
//     metadata which is inconsistent.
//     And with current logic it wont disgard them during load because during layermap
//     load it sees local disk consistent lsn which is ahead of layer lsns.
//     If we treat remote as source of truth we need to completely sync with it,
//     i e delete local files which are missing on the remote. This will add extra work,
//     wal for these layers needs to be reingested for example
//
// So the solution is to take remote metadata only when we're attaching.
pub fn merge_local_remote_metadata<'a>(
    local: Option<&'a TimelineMetadata>,
    remote: Option<&'a TimelineMetadata>,
) -> anyhow::Result<(&'a TimelineMetadata, bool)> {
    match (local, remote) {
        (None, None) => anyhow::bail!("we should have either local metadata or remote"),
        (Some(local), None) => Ok((local, true)),
        // happens if we crash during attach, before writing out the metadata file
        (None, Some(remote)) => Ok((remote, false)),
        // This is the regular case where we crash/exit before finishing queued uploads.
        // Also, it happens if we crash during attach after writing the metadata file
        // but before removing the attaching marker file.
        (Some(local), Some(remote)) => {
            let consistent_lsn_cmp = local
                .disk_consistent_lsn()
                .cmp(&remote.disk_consistent_lsn());
            let gc_cutoff_lsn_cmp = local
                .latest_gc_cutoff_lsn()
                .cmp(&remote.latest_gc_cutoff_lsn());
            use std::cmp::Ordering::*;
            match (consistent_lsn_cmp, gc_cutoff_lsn_cmp) {
                // It wouldn't matter, but pick the local one so that we don't rewrite the metadata file.
                (Equal, Equal) => Ok((local, true)),
                // Local state is clearly ahead of the remote.
                (Greater, Greater) => Ok((local, true)),
                // We have local layer files that aren't on the remote, but GC horizon is on par.
                (Greater, Equal) => Ok((local, true)),
                // Local GC started running but we couldn't sync it to the remote.
                (Equal, Greater) => Ok((local, true)),

                // We always update the local value first, so something else must have
                // updated the remote value, probably a different pageserver.
                // The control plane is supposed to prevent this from happening.
                // Bail out.
                (Less, Less)
                | (Less, Equal)
                | (Equal, Less)
                | (Less, Greater)
                | (Greater, Less) => {
                    anyhow::bail!(
                        r#"remote metadata appears to be ahead of local metadata:
local:
  {local:#?}
remote:
  {remote:#?}
"#
                    );
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DeleteTimelineError {
    #[error("NotFound")]
    NotFound,
    #[error("HasChildren")]
    HasChildren,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub enum SetStoppingError {
    AlreadyStopping,
    Broken,
}

struct RemoteStartupData {
    index_part: IndexPart,
    remote_metadata: TimelineMetadata,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum WaitToBecomeActiveError {
    WillNotBecomeActive {
        tenant_id: TenantId,
        state: TenantState,
    },
    TenantDropped {
        tenant_id: TenantId,
    },
}

impl std::fmt::Display for WaitToBecomeActiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaitToBecomeActiveError::WillNotBecomeActive { tenant_id, state } => {
                write!(
                    f,
                    "Tenant {} will not become active. Current state: {:?}",
                    tenant_id, state
                )
            }
            WaitToBecomeActiveError::TenantDropped { tenant_id } => {
                write!(f, "Tenant {tenant_id} will not become active (dropped)")
            }
        }
    }
}

#[derive(Clone)]
pub enum TimelineLoadCause {
    Startup,
    Attach,
    TenantCreate,
    TimelineCreate {
        placeholder_timeline: Arc<Timeline>,
        expxect_layer_files: bool,
    },
    TenantLoad,
    #[cfg(test)]
    Test,
}

impl std::fmt::Debug for TimelineLoadCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimelineLoadCause::Startup => write!(f, "Startup"),
            TimelineLoadCause::Attach => write!(f, "Attach"),
            TimelineLoadCause::TenantCreate => write!(f, "TenantCreate"),
            TimelineLoadCause::TimelineCreate { .. } => write!(f, "TimelineCreate"),
            TimelineLoadCause::TenantLoad => write!(f, "TenantLoad"),
            #[cfg(test)]
            TimelineLoadCause::Test => write!(f, "Test"),
        }
    }
}

impl Tenant {
    /// Yet another helper for timeline initialization.
    /// Contains the common part of `load_local_timeline` and `load_remote_timeline`.
    ///
    /// - Initializes the Timeline struct and inserts it into the tenant's hash map
    /// - Scans the local timeline directory for layer files and builds the layer map
    /// - Downloads remote index file and adds remote files to the layer map
    /// - Schedules remote upload tasks for any files that are present locally but missing from remote storage.
    ///
    /// If the operation fails, the timeline is left in the tenant's hash map in Broken state. On success,
    /// it is marked as Active.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(?cause))]
    async fn timeline_init_and_sync(
        &self,
        timeline_id: TimelineId,
        remote_client: Option<Arc<RemoteTimelineClient>>,
        remote_startup_data: Option<RemoteStartupData>,
        local_metadata: Option<TimelineMetadata>,
        ancestor: AncestorArg,
        cause: TimelineLoadCause,
        first_save: bool, // TODO need to think about this
        _ctx: &RequestContext,
    ) -> anyhow::Result<Arc<Timeline>> {
        debug_assert_current_span_has_tenant_and_timeline_id();

        let tenant_id = self.tenant_id;
        let ancestor = ancestor.0;

        let agreed_ancestor_id = match (
            &local_metadata,
            remote_startup_data.as_ref().map(|rsd| &rsd.remote_metadata),
        ) {
            (Some(local_metadata), Some(remote_metadata)) => {
                anyhow::ensure!(
                    local_metadata.ancestor_timeline() == remote_metadata.ancestor_timeline(),
                    "local and remote metadata do not agree on ancestorship, local={local:?} remote={remote:?}",
                    local = local_metadata,
                    remote = remote_metadata,
                );
                local_metadata.ancestor_timeline()
            }
            (None, None) => {
                unreachable!("TODO probably get rid of this possiblity at the same time as we eliminate first_save");
            }
            (Some(md), None) | (None, Some(md)) => md.ancestor_timeline(),
        };
        assert_eq!(
            ancestor.as_ref().map(|a| a.timeline_id),
            // we could check either local or remote metadata, it doesn't matter,
            // we checked above that they're either (None, None) or (Some, Some)
            agreed_ancestor_id,
            "caller does not provide correct ancestor"
        );

        let (up_to_date_metadata, picked_local) = merge_local_remote_metadata(
            local_metadata.as_ref(),
            remote_startup_data.as_ref().map(|r| &r.remote_metadata),
        )
        .context("merge_local_remote_metadata")?
        .to_owned();

        assert_eq!(
            up_to_date_metadata.ancestor_timeline(),
            ancestor.as_ref().map(|a| a.timeline_id),
            "merge_local_remote_metadata should not change ancestor"
        );

        let timeline = {
            let timeline = self.create_timeline_data(
                timeline_id,
                up_to_date_metadata,
                ancestor.clone(),
                remote_client,
            )?;
            let new_disk_consistent_lsn = timeline.get_disk_consistent_lsn();
            // TODO it would be good to ensure that, but apparently a lot of our testing is dependend on that at least
            // ensure!(new_disk_consistent_lsn.is_valid(),
            //     "Timeline {tenant_id}/{timeline_id} has invalid disk_consistent_lsn and cannot be initialized");
            timeline
                .load_layer_map(new_disk_consistent_lsn)
                .with_context(|| {
                    format!("Failed to load layermap for timeline {tenant_id}/{timeline_id}")
                })?;

            timeline
        };

        if self.remote_storage.is_some() {
            // Reconcile local state with remote storage, downloading anything that's
            // missing locally, and scheduling uploads for anything that's missing
            // in remote storage.
            timeline
                .reconcile_with_remote(
                    up_to_date_metadata,
                    remote_startup_data.as_ref().map(|r| &r.index_part),
                )
                .await
                .context("failed to reconcile with remote")?
        }

        match cause {
            TimelineLoadCause::TenantCreate => {
                unreachable!("tenant create does not create timelines")
            }
            TimelineLoadCause::Attach
            | TimelineLoadCause::TenantLoad
            | TimelineLoadCause::Startup
            | TimelineLoadCause::TimelineCreate {
                expxect_layer_files: true,
                ..
            } => {
                // Sanity check: a timeline should have some content.
                anyhow::ensure!(
                    ancestor.is_some()
                        || timeline
                            .layers
                            .read()
                            .unwrap()
                            .iter_historic_layers()
                            .next()
                            .is_some(),
                    "Timeline has no ancestor and no layer files"
                );
            }
            TimelineLoadCause::TimelineCreate {
                expxect_layer_files: false,
                ..
            } => {
                let has_layers = timeline
                    .layers
                    .read()
                    .unwrap()
                    .iter_historic_layers()
                    .next()
                    .is_some();
                assert!(!has_layers, "timeline is not expected to have layers");
            }
            // tests do all sorts of weird stuff
            #[cfg(test)]
            TimelineLoadCause::Test => {}
        }

        // Save the metadata file to local disk.
        if !picked_local {
            save_metadata(
                self.conf,
                timeline_id,
                tenant_id,
                up_to_date_metadata,
                first_save,
            )
            .context("save_metadata")?;
        }

        Ok(timeline)
    }

    ///
    /// Attach a tenant that's available in cloud storage.
    ///
    /// This returns quickly, after just creating the in-memory object
    /// Tenant struct and launching a background task to download
    /// the remote index files.  On return, the tenant is most likely still in
    /// Attaching state, and it will become Active once the background task
    /// finishes. You can use wait_until_active() to wait for the task to
    /// complete.
    ///
    pub(crate) fn spawn_attach(
        conf: &'static PageServerConf,
        tenant_id: TenantId,
        broker_client: storage_broker::BrokerClientChannel,
        remote_storage: GenericRemoteStorage,
        ctx: &RequestContext,
    ) -> anyhow::Result<Arc<Tenant>> {
        // TODO dedup with spawn_load
        let tenant_conf =
            Self::load_tenant_config(conf, tenant_id).context("load tenant config")?;

        let wal_redo_manager = Arc::new(PostgresRedoManager::new(conf, tenant_id));
        let tenant = Arc::new(Tenant::new(
            TenantState::Attaching,
            conf,
            tenant_conf,
            wal_redo_manager,
            tenant_id,
            Some(remote_storage),
        ));

        // Do all the hard work in the background
        let tenant_clone = Arc::clone(&tenant);

        let ctx = ctx.detached_child(TaskKind::Attach, DownloadBehavior::Warn);
        task_mgr::spawn(
            &tokio::runtime::Handle::current(),
            TaskKind::Attach,
            Some(tenant_id),
            None,
            "attach tenant",
            false,
            async move {
                match tenant_clone.attach(&ctx).await {
                    Ok(()) => {
                        info!("attach finished, activating");
                        tenant_clone.activate(broker_client, &ctx);
                    }
                    Err(e) => {
                        error!("attach failed, setting tenant state to Broken: {:?}", e);
                        tenant_clone.state.send_modify(|state| {
                            assert_eq!(*state, TenantState::Attaching, "the attach task owns the tenant state until activation is complete");
                            *state = TenantState::broken_from_reason(e.to_string());
                        });
                    }
                }
                Ok(())
            }
            .instrument({
                let span = tracing::info_span!(parent: None, "attach", tenant_id=%tenant_id);
                span.follows_from(Span::current());
                span
            }),
        );
        Ok(tenant)
    }

    ///
    /// Background task that downloads all data for a tenant and brings it to Active state.
    ///
    /// No background tasks are started as part of this routine.
    ///
    async fn attach(self: &Arc<Tenant>, ctx: &RequestContext) -> anyhow::Result<()> {
        debug_assert_current_span_has_tenant_id();

        let marker_file = self.conf.tenant_attaching_mark_file_path(&self.tenant_id);
        if !tokio::fs::try_exists(&marker_file)
            .await
            .context("check for existence of marker file")?
        {
            anyhow::bail!(
                "implementation error: marker file should exist at beginning of this function"
            );
        }

        // Get list of remote timelines
        // download index files for every tenant timeline
        info!("listing remote timelines");

        let remote_storage = self
            .remote_storage
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("cannot attach without remote storage"))?;

        let remote_timeline_ids = remote_timeline_client::list_remote_timelines(
            remote_storage,
            self.conf,
            self.tenant_id,
        )
        .await?;

        info!("found {} timelines", remote_timeline_ids.len());

        // Download & parse index parts
        let mut part_downloads = JoinSet::new();
        for timeline_id in remote_timeline_ids {
            let client = RemoteTimelineClient::new(
                remote_storage.clone(),
                self.conf,
                self.tenant_id,
                timeline_id,
            );
            part_downloads.spawn(
                async move {
                    debug!("starting index part download");

                    let index_part = client
                        .download_index_file()
                        .await
                        .context("download index file")?;

                    debug!("finished index part download");

                    Result::<_, anyhow::Error>::Ok((timeline_id, client, index_part))
                }
                .map(move |res| {
                    res.with_context(|| format!("download index part for timeline {timeline_id}"))
                })
                .instrument(info_span!("download_index_part", timeline=%timeline_id)),
            );
        }
        // Wait for all the download tasks to complete & collect results.
        let mut remote_index_and_client = HashMap::new();
        let mut timeline_ancestors = HashMap::new();
        while let Some(result) = part_downloads.join_next().await {
            // NB: we already added timeline_id as context to the error
            let result: Result<_, anyhow::Error> = result.context("joinset task join")?;
            let (timeline_id, client, index_part) = result?;
            debug!("successfully downloaded index part for timeline {timeline_id}");
            match index_part {
                MaybeDeletedIndexPart::IndexPart(index_part) => {
                    timeline_ancestors.insert(
                        timeline_id,
                        index_part.parse_metadata().context("parse_metadata")?,
                    );
                    remote_index_and_client.insert(timeline_id, (index_part, client));
                }
                MaybeDeletedIndexPart::Deleted => {
                    info!("timeline {} is deleted, skipping", timeline_id);
                    continue;
                }
            }
        }

        // For every timeline, download the metadata file, scan the local directory,
        // and build a layer map that contains an entry for each remote and local
        // layer file.
        let sorted_timelines = tree_sort_timelines(timeline_ancestors)?;
        for (timeline_id, remote_metadata) in sorted_timelines {
            let (index_part, remote_client) = remote_index_and_client
                .remove(&timeline_id)
                .expect("just put it in above");

            // TODO again handle early failure
            let ancestor = if let Some(ancestor_id) = remote_metadata.ancestor_timeline() {
                let timelines = self.timelines.lock().unwrap();
                AncestorArg::ancestor(Arc::clone(timelines.get(&ancestor_id).ok_or_else(
                    || {
                        anyhow::anyhow!(
                        "cannot find ancestor timeline {ancestor_id} for timeline {timeline_id}"
                    )
                    },
                )?))
            } else {
                AncestorArg::no_ancestor()
            };
            let timeline = self
                .load_remote_timeline(
                    timeline_id,
                    index_part,
                    remote_metadata,
                    ancestor,
                    remote_client,
                    ctx,
                )
                .await
                .with_context(|| {
                    format!(
                        "failed to load remote timeline {} for tenant {}",
                        timeline_id, self.tenant_id
                    )
                })?;
            // TODO: why can't load_remote_timeline return None like load_local_timeline does?

            let mut timelines = self.timelines.lock().unwrap();
            let overwritten = timelines.insert(timeline_id, Arc::clone(&timeline));
            if let Some(overwritten) = overwritten {
                panic!(
                    "timeline should not be in the map yet, but is: {timeline_id}: {:?}",
                    overwritten.current_state()
                );
            }
        }

        std::fs::remove_file(&marker_file)
            .with_context(|| format!("unlink attach marker file {}", marker_file.display()))?;
        crashsafe::fsync(marker_file.parent().expect("marker file has parent dir"))
            .context("fsync tenant directory after unlinking attach marker file")?;

        utils::failpoint_sleep_millis_async!("attach-before-activate");

        info!("Done");

        Ok(())
    }

    /// get size of all remote timelines
    ///
    /// This function relies on the index_part instead of listing the remote storage
    ///
    pub async fn get_remote_size(&self) -> anyhow::Result<u64> {
        let mut size = 0;

        for timeline in self.list_timelines().iter() {
            if let Some(remote_client) = &timeline.remote_client {
                size += remote_client.get_remote_physical_size();
            }
        }

        Ok(size)
    }

    #[instrument(skip_all, fields(timeline_id=%timeline_id))]
    async fn load_remote_timeline(
        &self,
        timeline_id: TimelineId,
        index_part: IndexPart,
        remote_metadata: TimelineMetadata,
        ancestor: AncestorArg,
        remote_client: RemoteTimelineClient,
        ctx: &RequestContext,
    ) -> anyhow::Result<Arc<Timeline>> {
        debug_assert_current_span_has_tenant_id();

        info!("downloading index file for timeline {}", timeline_id);
        tokio::fs::create_dir_all(self.conf.timeline_path(&timeline_id, &self.tenant_id))
            .await
            .context("Failed to create new timeline directory")?;

        // Even if there is local metadata it cannot be ahead of the remote one
        // since we're attaching. Even if we resume interrupted attach remote one
        // cannot be older than the local one
        let local_metadata = None;

        self.timeline_init_and_sync(
            timeline_id,
            Some(Arc::new(remote_client)),
            Some(RemoteStartupData {
                index_part,
                remote_metadata,
            }),
            local_metadata,
            ancestor,
            TimelineLoadCause::Attach,
            true,
            ctx,
        )
        .await
    }

    /// Create a placeholder Tenant object for a broken tenant
    pub fn create_broken_tenant(
        conf: &'static PageServerConf,
        tenant_id: TenantId,
        reason: String,
    ) -> Arc<Tenant> {
        let wal_redo_manager = Arc::new(PostgresRedoManager::new(conf, tenant_id));
        Arc::new(Tenant::new(
            TenantState::Broken {
                reason,
                backtrace: String::new(),
            },
            conf,
            TenantConfOpt::default(),
            wal_redo_manager,
            tenant_id,
            None,
        ))
    }

    ///
    /// Load a tenant that's available on local disk
    ///
    /// This is used at pageserver startup, to rebuild the in-memory
    /// structures from on-disk state. This is similar to attaching a tenant,
    /// but the index files already exist on local disk, as well as some layer
    /// files.
    ///
    /// If the loading fails for some reason, the Tenant will go into Broken
    /// state.
    ///
    #[instrument(skip_all, fields(tenant_id=%tenant_id))]
    pub fn spawn_load(
        conf: &'static PageServerConf,
        tenant_id: TenantId,
        broker_client: storage_broker::BrokerClientChannel,
        remote_storage: Option<GenericRemoteStorage>,
        cause: TimelineLoadCause,
        ctx: &RequestContext,
    ) -> Arc<Tenant> {
        let tenant_conf = match Self::load_tenant_config(conf, tenant_id) {
            Ok(conf) => conf,
            Err(e) => {
                error!("load tenant config failed: {:?}", e);
                return Tenant::create_broken_tenant(conf, tenant_id, format!("{e:#}"));
            }
        };

        let wal_redo_manager = Arc::new(PostgresRedoManager::new(conf, tenant_id));
        let tenant = Tenant::new(
            TenantState::Loading,
            conf,
            tenant_conf,
            wal_redo_manager,
            tenant_id,
            remote_storage,
        );
        let tenant = Arc::new(tenant);

        // Do all the hard work in a background task
        let tenant_clone = Arc::clone(&tenant);

        let ctx = ctx.detached_child(TaskKind::InitialLoad, DownloadBehavior::Warn);
        let _ = task_mgr::spawn(
            &tokio::runtime::Handle::current(),
            TaskKind::InitialLoad,
            Some(tenant_id),
            None,
            "initial tenant load",
            false,
            async move {
                match tenant_clone.load(cause, &ctx).await {
                    Ok(()) => {
                        info!("load finished, activating");
                        tenant_clone.activate(broker_client, &ctx);
                    }
                    Err(err) => {
                        error!("load failed, setting tenant state to Broken: {err:?}");
                        tenant_clone.state.send_modify(|state| {
                            assert_eq!(*state, TenantState::Loading, "the loading task owns the tenant state until activation is complete");
                            *state = TenantState::broken_from_reason(err.to_string());
                        });
                    }
                }
               Ok(())
            }
            .instrument({
                let span = tracing::info_span!(parent: None, "load", tenant_id=%tenant_id);
                span.follows_from(Span::current());
                span
            }),
        );

        info!("spawned load into background");

        tenant
    }

    ///
    /// Background task to load in-memory data structures for this tenant, from
    /// files on disk. Used at pageserver startup.
    ///
    /// No background tasks are started as part of this routine.
    async fn load(
        self: &Arc<Tenant>,
        cause: TimelineLoadCause,
        ctx: &RequestContext,
    ) -> anyhow::Result<()> {
        debug_assert_current_span_has_tenant_id();

        info!("loading tenant task");

        utils::failpoint_sleep_millis_async!("before-loading-tenant");

        // TODO split this into two functions, scan and actual load

        // Load in-memory state to reflect the local files on disk
        //
        // Scan the directory, peek into the metadata file of each timeline, and
        // collect a list of timelines and their ancestors.
        let mut timelines_to_load: HashMap<TimelineId, TimelineMetadata> = HashMap::new();
        let timelines_dir = self.conf.timelines_path(&self.tenant_id);
        let entries: Vec<DirEntry> = loop {
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(&timelines_dir).with_context(|| {
                format!(
                    "Failed to list timelines directory for tenant {}",
                    self.tenant_id
                )
            })? {
                let entry = entry.with_context(|| {
                    format!("cannot read timeline dir entry for {}", self.tenant_id)
                })?;
                entries.push(entry);
            }

            let mut removed_unint_timeline = false;
            for entry in &entries {
                let timeline_dir = entry.path();
                if crate::is_temporary(&timeline_dir) {
                    info!(
                        "Found temporary timeline directory, removing: {}",
                        timeline_dir.display()
                    );
                    if let Err(e) = std::fs::remove_dir_all(&timeline_dir) {
                        error!(
                            "Failed to remove temporary directory '{}': {:?}",
                            timeline_dir.display(),
                            e
                        );
                    }
                } else if is_uninit_mark(&timeline_dir) {
                    let timeline_uninit_mark_file = &timeline_dir;
                    info!(
                        "Found an uninit mark file {}, removing the timeline and its uninit mark",
                        timeline_uninit_mark_file.display()
                    );
                    let timeline_id = timeline_uninit_mark_file
                        .file_stem()
                        .and_then(OsStr::to_str)
                        .unwrap_or_default()
                        .parse::<TimelineId>()
                        .with_context(|| {
                            format!(
                                "Could not parse timeline id out of the timeline uninit mark name {}",
                                timeline_uninit_mark_file.display()
                            )
                        })?;
                    let timeline_dir = self.conf.timeline_path(&timeline_id, &self.tenant_id);
                    remove_timeline_and_uninit_mark(&timeline_dir, timeline_uninit_mark_file)?;
                    removed_unint_timeline = true;
                }
            }

            if removed_unint_timeline {
                continue;
            }

            break entries;
        };

        for entry in entries {
            let timeline_dir = entry.path();
            assert!(!crate::is_temporary(&timeline_dir), "removed above");
            assert!(!is_uninit_mark(&timeline_dir), "removed above");
            let timeline_id = timeline_dir
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .parse::<TimelineId>()
                .with_context(|| {
                    format!(
                        "Could not parse timeline id out of the timeline dir name {}",
                        timeline_dir.display()
                    )
                })?;
            let metadata = load_metadata(self.conf, timeline_id, self.tenant_id)
                .context("failed to load metadata")?;
            timelines_to_load.insert(timeline_id, metadata);
        }

        // Sort the array of timeline IDs into tree-order, so that parent comes before
        // all its children.
        let sorted_timelines = tree_sort_timelines(timelines_to_load)?;
        // FIXME original collect_timeline_files contained one more check:
        //    1. "Timeline has no ancestor and no layer files"

        for (timeline_id, local_metadata) in sorted_timelines {
            let ancestor = if let Some(ancestor_id) = local_metadata.ancestor_timeline() {
                let timelines = self.timelines.lock().unwrap();
                AncestorArg::ancestor(Arc::clone(timelines.get(&ancestor_id).ok_or_else(
                    || {
                        anyhow::anyhow!(
                        "cannot find ancestor timeline {ancestor_id} for timeline {timeline_id}"
                    )
                    },
                )?))
            } else {
                AncestorArg::no_ancestor()
            };
            let timeline = self
                .load_local_timeline(timeline_id, local_metadata, ancestor, cause.clone(), ctx)
                .await
                .with_context(|| format!("load local timeline {timeline_id}"))?;
            match timeline {
                Some(loaded_timeline) => {
                    let mut timelines = self.timelines.lock().unwrap();
                    let overwritten = timelines.insert(timeline_id, Arc::clone(&loaded_timeline));
                    if let Some(overwritten) = overwritten {
                        panic!(
                            "timeline should not be in the map yet, but is: {timeline_id}: {:?}",
                            overwritten.current_state()
                        );
                    }
                }
                None => {
                    info!(%timeline_id, "timeline is marked as deleted on the remote, load_local_timeline finished the deletion locally");
                    // TODO don't we need to restart the tree sort?
                }
            }
        }

        info!("Done");

        Ok(())
    }

    /// Subroutine of `load_tenant`, to load an individual timeline
    ///
    /// NB: The parent is assumed to be already loaded!
    #[instrument(skip_all, fields(timeline_id))]
    async fn load_local_timeline(
        &self,
        timeline_id: TimelineId,
        local_metadata: TimelineMetadata,
        ancestor: AncestorArg,
        cause: TimelineLoadCause,
        ctx: &RequestContext,
    ) -> anyhow::Result<Option<Arc<Timeline>>> {
        debug_assert_current_span_has_tenant_id();

        let remote_client = self.remote_storage.as_ref().map(|remote_storage| {
            Arc::new(RemoteTimelineClient::new(
                remote_storage.clone(),
                self.conf,
                self.tenant_id,
                timeline_id,
            ))
        });

        let remote_startup_data = match &remote_client {
            Some(remote_client) => match remote_client.download_index_file().await {
                Ok(index_part) => {
                    let index_part = match index_part {
                        MaybeDeletedIndexPart::IndexPart(index_part) => index_part,
                        MaybeDeletedIndexPart::Deleted => {
                            // TODO: we won't reach here if remote storage gets de-configured after start of the deletion operation.
                            // Example:
                            //  start deletion operation
                            //  finishes upload of index part
                            //  pageserver crashes
                            //  remote storage gets de-configured
                            //  pageserver starts
                            //
                            // We don't really anticipate remote storage to be de-configured, so, for now, this is fine.
                            // Also, maybe we'll remove that option entirely in the future, see https://github.com/neondatabase/neon/issues/4099.
                            info!("is_deleted is set on remote, resuming removal of local data originally done by timeline deletion handler");
                            std::fs::remove_dir_all(
                                self.conf.timeline_path(&timeline_id, &self.tenant_id),
                            )
                            .context("remove_dir_all")?;

                            return Ok(None);
                        }
                    };

                    let remote_metadata = index_part.parse_metadata().context("parse_metadata")?;
                    Some(RemoteStartupData {
                        index_part,
                        remote_metadata,
                    })
                }
                Err(DownloadError::NotFound) => {
                    info!("no index file was found on the remote");
                    None
                }
                Err(e) => return Err(anyhow::anyhow!(e)),
            },
            None => None,
        };

        let inserted_timeline = self
            .timeline_init_and_sync(
                timeline_id,
                remote_client,
                remote_startup_data,
                Some(local_metadata),
                ancestor,
                cause,
                false,
                ctx,
            )
            .await?;
        Ok(Some(inserted_timeline))
    }

    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Get Timeline handle for given Neon timeline ID.
    /// This function is idempotent. It doesn't change internal state in any way.
    pub fn get_timeline(
        &self,
        timeline_id: TimelineId,
        active_only: bool,
    ) -> anyhow::Result<Arc<Timeline>> {
        let timelines_accessor = self.timelines.lock().unwrap();
        let timeline = timelines_accessor.get(&timeline_id).with_context(|| {
            format!("Timeline {}/{} was not found", self.tenant_id, timeline_id)
        })?;

        if active_only && !timeline.is_active() {
            anyhow::bail!(
                "Timeline {}/{} is not active, state: {:?}",
                self.tenant_id,
                timeline_id,
                timeline.current_state()
            )
        } else {
            Ok(Arc::clone(timeline))
        }
    }

    /// Lists timelines the tenant contains.
    /// Up to tenant's implementation to omit certain timelines that ar not considered ready for use.
    pub fn list_timelines(&self) -> Vec<Arc<Timeline>> {
        self.timelines
            .lock()
            .unwrap()
            .values()
            .map(Arc::clone)
            .collect()
    }

    /// This is used to create the initial 'main' timeline during bootstrapping,
    /// or when importing a new base backup. The caller is expected to load an
    /// initial image of the datadir to the new timeline after this.
    ///
    /// Cancel-safety: not cancel safe.
    ///
    /// TODO: pull in latest changes from create_timeline()
    pub(crate) async fn create_empty_timeline(
        &self,
        new_timeline_id: TimelineId,
        initdb_lsn: Lsn,
        pg_version: u32,
        ctx: &RequestContext,
    ) -> anyhow::Result<(CreatingTimelineGuard, Arc<Timeline>)> {
        anyhow::ensure!(
            self.is_active(),
            "Cannot create empty timelines on inactive tenant"
        );
        // TODO: dedup with create_timeline

        let guard = self.start_creating_timeline(new_timeline_id)?;

        // Create timeline on-disk & remote state.
        //
        // Use an async block make sure we remove the uninit mark if the closure fails.
        let create_ondisk_state = async {
            let remote_client = self.remote_storage.as_ref().map(|remote_storage| {
                Arc::new(RemoteTimelineClient::new(
                    remote_storage.clone(),
                    self.conf,
                    self.tenant_id,
                    new_timeline_id,
                ))
            });

            let new_metadata = TimelineMetadata::new(
                Lsn(0), // TODO should this be initdb_lsn as well, at least for the handle_import_basebackup use case?
                None,
                None,
                Lsn(0),
                initdb_lsn,
                initdb_lsn,
                pg_version,
            );

            self.create_timeline_files(&guard.timeline_path, new_timeline_id, &new_metadata)
                .context("create_timeline_files")?;

            if let Some(remote_client) = remote_client.as_ref() {
                remote_client.init_upload_queue_for_empty_remote(&new_metadata)?;
                remote_client
                    .schedule_index_upload_for_metadata_update(&new_metadata)
                    .context("branch initial metadata upload")?;
                remote_client
                    .wait_completion()
                    .await
                    .context("wait for initial uploads to complete")?;
            }

            // XXX do we need to remove uninit mark before starting uploads?
            // If we die with uninit mark present, we'll leak the uploaded state in S3.
            Ok(())
        };
        let guard = match create_ondisk_state.await {
            Ok(()) => {
                // caller will continue with creation, so, not calling creation complete yet
                guard
            }
            Err(err) => {
                debug_assert_current_span_has_tenant_and_timeline_id();
                error!(
                    "failed to create on-disk state for new_timeline_id={new_timeline_id}: {err:#}"
                );
                guard.creation_failed();
                return Err(err);
            }
        };

        // From here on, it's just like during pageserver startup.
        let metadata = load_metadata(self.conf, new_timeline_id, self.tenant_id)
            .context("load newly created on-disk timeline metadata")?;
        let real_timeline = self
            .load_local_timeline(
                new_timeline_id,
                metadata,
                AncestorArg::no_ancestor(),
                TimelineLoadCause::TimelineCreate {
                    placeholder_timeline: Arc::clone(&guard.placeholder_timeline),
                    expxect_layer_files: false,
                },
                ctx,
            )
            .await
            .context("load newly created on-disk timeline state")?
            .expect("load_local_timeline should have created the timeline");

        // don't replace the placeholder timeline, the caller is going to fill
        // real_timeline with more data and once that's done, we're ready to
        // replace the placeholder

        // let real_timeline = match self.timelines.lock().unwrap().entry(new_timeline_id) {
        //     Entry::Vacant(_) => unreachable!("we created a placeholder earlier, and load_local_timeline should have inserted the real timeline"),
        //     Entry::Occupied(entry) => {
        //         assert_eq!(guard.placeholder_timeline.current_state(), TimelineState::Creating);
        //         assert!(compare_arced_timeline(&real_timeline, entry.get()));
        //         assert_eq!(real_timeline.current_state(), TimelineState::Loading);
        //         assert!(!compare_arced_timeline(&guard.placeholder_timeline, entry.get()), "load_local_timeline should have replaced the placeholder with the real timeline");
        //         Arc::clone(entry.get())
        //     }
        // };

        // Do not activate, the caller is responsible for that.
        // Also, the caller is still responsible for removing the uninit mark file.
        // Before that happens, the timeline will be removed during restart.
        //
        // TODO: can we just keep the placeholder in there for longer?
        Ok((guard, real_timeline))

        // TODO
        // unfinished_timeline
        // .layers
        // .write()
        // .unwrap()
        // .next_open_layer_at = Some(initdb_lsn);
    }

    /// Helper for unit tests to create an emtpy timeline.
    ///
    /// The timeline is has state value `Active` but its background loops are not running.
    // This makes the various functions which anyhow::ensure! for Active state work in tests.
    // Our current tests don't need the background loops.
    #[cfg(test)]
    pub async fn create_test_timeline(
        &self,
        new_timeline_id: TimelineId,
        initdb_lsn: Lsn,
        pg_version: u32,
        ctx: &RequestContext,
    ) -> anyhow::Result<Arc<Timeline>> {
        let (guard, real_timeline) = self
            .create_empty_timeline(new_timeline_id, initdb_lsn, pg_version, ctx)
            // make the debug_assert_current_span_has_tenant_id() in create_empty_timeline() happy
            .instrument(tracing::info_span!("create_test_timeline", tenant_id=%self.tenant_id))
            .await
            .context("create empty timeline")?;
        // the tests don't need any content in the timeline, we're done here
        let placeholder_timeline = guard
            .creation_complete_remove_uninit_marker_and_get_placeholder_timeline()
            .context("creation_complete_remove_uninit_marker_and_get_placeholder_timeline")?;

        match self.timelines.lock().unwrap().entry(new_timeline_id) {
            Entry::Vacant(_) => unreachable!("we created a placeholder earlier, and load_local_timeline should have inserted the real timeline"),
            Entry::Occupied(mut o) => {
                info!("replacing placeholder timeline with the real one");
                assert_eq!(placeholder_timeline.current_state(), TimelineState::Creating);
                assert!(compare_arced_timeline(&placeholder_timeline, o.get()));
                let replaced_placeholder = o.insert(Arc::clone(&real_timeline));
                assert!(compare_arced_timeline(&replaced_placeholder, &placeholder_timeline));
            },
        }

        // The non-test code would call tl.activate() here.
        real_timeline.maybe_spawn_flush_loop();
        real_timeline.set_state(TimelineState::Active);
        Ok(real_timeline)
    }

    /// Create a new timeline.
    ///
    /// Returns the new timeline ID and reference to its Timeline object.
    ///
    /// If the caller specified the timeline ID to use (`new_timeline_id`), and timeline with
    /// the same timeline ID already exists, returns None. If `new_timeline_id` is not given,
    /// a new unique ID is generated.
    pub async fn create_timeline(
        self: &Arc<Self>,
        new_timeline_id: TimelineId,
        ancestor_timeline_id: Option<TimelineId>,
        ancestor_start_lsn: Option<Lsn>,
        pg_version: u32,
        broker_client: storage_broker::BrokerClientChannel,
        ctx: &RequestContext,
    ) -> anyhow::Result<Option<Arc<Timeline>>> {
        let ctx = ctx.detached_child(TaskKind::CreateTimeline, DownloadBehavior::Warn);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let self_clone = Arc::clone(self);
        task_mgr::spawn(
            &tokio::runtime::Handle::current(),
            TaskKind::CreateTimeline,
            Some(self.tenant_id),
            None, // this is a tenant-level operation
            "create timeline",
            false,
            async move {
                let res = self_clone
                    .create_timeline_task(
                        new_timeline_id,
                        ancestor_timeline_id,
                        ancestor_start_lsn,
                        pg_version,
                        broker_client,
                        &ctx,
                    )
                    .await;
                let _ = tx.send(res); // receiver may get dropped due to request cancellation
                Ok(())
            }
            // may outlive caller if caller is cancelled, yet, it's useful to have caller's request id in the logs
            .instrument(tracing::info_span!( "create_timeline", tenant_id=%self.tenant_id)),
        );
        rx.await.expect("task_mgr tasks run to completion")
    }

    /// This is not cancel-safe. Run inside a task_mgr task.
    async fn create_timeline_task(
        self: &Tenant,
        new_timeline_id: TimelineId,
        ancestor_timeline_id: Option<TimelineId>,
        mut ancestor_start_lsn: Option<Lsn>,
        pg_version: u32,
        broker_client: storage_broker::BrokerClientChannel,
        ctx: &RequestContext,
    ) -> anyhow::Result<Option<Arc<Timeline>>> {
        debug_assert_current_span_has_tenant_and_timeline_id();

        anyhow::ensure!(
            self.is_active(),
            "Cannot create timelines on inactive tenant"
        );

        let guard = self.start_creating_timeline(new_timeline_id)?;

        // Create timeline on-disk & remote state.
        //
        // Use an async block to make sure we remove the uninit mark if the closure fails.
        let create_ondisk_state = async {
            let remote_client = self.remote_storage.as_ref().map(|remote_storage| {
                Arc::new(RemoteTimelineClient::new(
                    remote_storage.clone(),
                    self.conf,
                    self.tenant_id,
                    new_timeline_id,
                ))
            });

            match ancestor_timeline_id {
                Some(ancestor_timeline_id) => {
                    let ancestor_timeline =
                        self.get_timeline(ancestor_timeline_id, false).context(
                            "Cannot branch off the timeline that's not present in pageserver",
                        )?;

                    if let Some(lsn) = ancestor_start_lsn.as_mut() {
                        *lsn = lsn.align();

                        let ancestor_ancestor_lsn = ancestor_timeline.get_ancestor_lsn();
                        if ancestor_ancestor_lsn > *lsn {
                            // can we safely just branch from the ancestor instead?
                            bail!(
                                "invalid start lsn {} for ancestor timeline {}: less than timeline ancestor lsn {}",
                                lsn,
                                ancestor_timeline_id,
                                ancestor_ancestor_lsn,
                            );
                        }

                        // Wait for the WAL to arrive and be processed on the parent branch up
                        // to the requested branch point. The repository code itself doesn't
                        // require it, but if we start to receive WAL on the new timeline,
                        // decoding the new WAL might need to look up previous pages, relation
                        // sizes etc. and that would get confused if the previous page versions
                        // are not in the repository yet.
                        ancestor_timeline.wait_lsn(*lsn, ctx).await?;
                    }

                    self.branch_timeline(
                        &ancestor_timeline,
                        new_timeline_id,
                        ancestor_start_lsn,
                        remote_client,
                        &guard,
                        ctx,
                    )
                    .await?;
                    Ok(AncestorArg::ancestor(ancestor_timeline))
                }
                None => {
                    self.bootstrap_timeline(
                        new_timeline_id,
                        pg_version,
                        &guard,
                        remote_client,
                        ctx,
                    )
                    .await?;
                    Ok(AncestorArg::no_ancestor())
                }
            }
            // XXX do we need to remove uninit mark before the self.branch_timeline / self.bootstrap_timeline start the uploads?
            // If we die with uninit mark present, we'll leak the uploaded state in S3.
        };
        let (placeholder_timeline, ancestor) = match create_ondisk_state.await {
            Ok(ancestor) => {
                match guard.creation_complete_remove_uninit_marker_and_get_placeholder_timeline() {
                    Ok(placeholder_timeline) => (placeholder_timeline, ancestor),
                    Err(err) => {
                        error!(
                            "failed to remove uninit marker for new_timeline_id={new_timeline_id}: {err:#}"
                        );
                        return Err(err);
                    }
                }
            }
            Err(err) => {
                error!(
                    "failed to create on-disk state for new_timeline_id={new_timeline_id}: {err:#}"
                );
                guard.creation_failed();
                return Err(err);
            }
        };

        // From here on, it's just like during pageserver startup.
        let metadata = load_metadata(self.conf, new_timeline_id, self.tenant_id)
            .context("load newly created on-disk timeline metadata")?;

        let load_cause = TimelineLoadCause::TimelineCreate {
            placeholder_timeline: Arc::clone(&placeholder_timeline),
            expxect_layer_files: if ancestor.0.is_some() {
                // branched timelines just have the metadata file
                false
            } else {
                // bootstrapped timelines have layers
                true
            },
        };
        let real_timeline = self
            .load_local_timeline(new_timeline_id, metadata, ancestor, load_cause, ctx)
            .await
            .context("load newly created on-disk timeline state")?;

        let Some(real_timeline) = real_timeline else {
            anyhow::bail!("we just created this timeline's local files, but load_local_timeline did not load it");
        };

        match self.timelines.lock().unwrap().entry(new_timeline_id) {
            Entry::Vacant(_) => unreachable!("we created a placeholder earlier, and load_local_timeline should have inserted the real timeline"),
            Entry::Occupied(mut o) => {
                info!("replacing placeholder timeline with the real one");
                assert_eq!(placeholder_timeline.current_state(), TimelineState::Creating);
                assert!(compare_arced_timeline(&placeholder_timeline, o.get()));
                let replaced_placeholder = o.insert(Arc::clone(&real_timeline));
                assert!(compare_arced_timeline(&replaced_placeholder, &placeholder_timeline));
            },
        }

        real_timeline.activate(broker_client, ctx);

        Ok(Some(real_timeline))
    }

    /// perform one garbage collection iteration, removing old data files from disk.
    /// this function is periodically called by gc task.
    /// also it can be explicitly requested through page server api 'do_gc' command.
    ///
    /// `target_timeline_id` specifies the timeline to GC, or None for all.
    ///
    /// The `horizon` an `pitr` parameters determine how much WAL history needs to be retained.
    /// Also known as the retention period, or the GC cutoff point. `horizon` specifies
    /// the amount of history, as LSN difference from current latest LSN on each timeline.
    /// `pitr` specifies the same as a time difference from the current time. The effective
    /// GC cutoff point is determined conservatively by either `horizon` and `pitr`, whichever
    /// requires more history to be retained.
    //
    pub async fn gc_iteration(
        &self,
        target_timeline_id: Option<TimelineId>,
        horizon: u64,
        pitr: Duration,
        ctx: &RequestContext,
    ) -> anyhow::Result<GcResult> {
        anyhow::ensure!(
            self.is_active(),
            "Cannot run GC iteration on inactive tenant"
        );

        self.gc_iteration_internal(target_timeline_id, horizon, pitr, ctx)
            .await
    }

    /// Perform one compaction iteration.
    /// This function is periodically called by compactor task.
    /// Also it can be explicitly requested per timeline through page server
    /// api's 'compact' command.
    pub async fn compaction_iteration(&self, ctx: &RequestContext) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.is_active(),
            "Cannot run compaction iteration on inactive tenant"
        );

        // Scan through the hashmap and collect a list of all the timelines,
        // while holding the lock. Then drop the lock and actually perform the
        // compactions.  We don't want to block everything else while the
        // compaction runs.
        let timelines_to_compact = {
            let timelines = self.timelines.lock().unwrap();
            let timelines_to_compact = timelines
                .iter()
                .map(|(timeline_id, timeline)| (*timeline_id, timeline.clone()))
                .collect::<Vec<_>>();
            drop(timelines);
            timelines_to_compact
        };

        for (timeline_id, timeline) in &timelines_to_compact {
            timeline
                .compact(ctx)
                .instrument(info_span!("compact_timeline", timeline = %timeline_id))
                .await?;
        }

        Ok(())
    }

    /// Flush all in-memory data to disk.
    ///
    /// Used at graceful shutdown.
    ///
    // don't have a tenant_id field, freeze_and_flush adds it
    #[instrument(skip_all)]
    pub async fn freeze_and_flush(&self) -> anyhow::Result<()> {
        // Scan through the hashmap and collect a list of all the timelines,
        // while holding the lock. Then drop the lock and actually perform the
        // flushing. We don't want to block everything else while the
        // flushing is performed.
        let timelines_to_flush = {
            let timelines = self.timelines.lock().unwrap();
            timelines
                .iter()
                .map(|(_id, timeline)| Arc::clone(timeline))
                .collect::<Vec<_>>()
        };

        for timeline in &timelines_to_flush {
            match timeline.freeze_and_flush().await {
                Ok(()) => (),
                Err(err) => {
                    tracing::error!(
                        timeline_id=%timeline.timeline_id, err=?err,
                        "freeze_and_flush timeline failed",
                    );
                }
            }
        }

        Ok(())
    }

    /// Removes timeline-related in-memory data
    pub async fn delete_timeline(
        &self,
        timeline_id: TimelineId,
        _ctx: &RequestContext,
    ) -> Result<(), DeleteTimelineError> {
        timeline::debug_assert_current_span_has_tenant_and_timeline_id();

        // Transition the timeline into TimelineState::Stopping.
        // This should prevent new operations from starting.
        let timeline = {
            let mut timelines = self.timelines.lock().unwrap();

            // Ensure that there are no child timelines **attached to that pageserver**,
            // because detach removes files, which will break child branches
            let children_exist = timelines
                .iter()
                .any(|(_, entry)| entry.get_ancestor_timeline_id() == Some(timeline_id));

            if children_exist {
                return Err(DeleteTimelineError::HasChildren);
            }

            let timeline_entry = match timelines.entry(timeline_id) {
                Entry::Occupied(e) => e,
                Entry::Vacant(_) => return Err(DeleteTimelineError::NotFound),
            };

            let timeline = Arc::clone(timeline_entry.get());
            if timeline.current_state() == TimelineState::Creating {
                return Err(DeleteTimelineError::Other(anyhow::anyhow!(
                    "timeline is creating"
                )));
            }
            timeline.set_state(TimelineState::Stopping);

            drop(timelines);
            timeline
        };

        // Now that the Timeline is in Stopping state, request all the related tasks to
        // shut down.
        //
        // NB: If you call delete_timeline multiple times concurrently, they will
        // all go through the motions here. Make sure the code here is idempotent,
        // and don't error out if some of the shutdown tasks have already been
        // completed!

        // Stop the walreceiver first.
        debug!("waiting for wal receiver to shutdown");
        let maybe_started_walreceiver = { timeline.walreceiver.lock().unwrap().take() };
        if let Some(walreceiver) = maybe_started_walreceiver {
            walreceiver.stop().await;
        }
        debug!("wal receiver shutdown confirmed");

        // Prevent new uploads from starting.
        if let Some(remote_client) = timeline.remote_client.as_ref() {
            let res = remote_client.stop();
            match res {
                Ok(()) => {}
                Err(e) => match e {
                    remote_timeline_client::StopError::QueueUninitialized => {
                        // This case shouldn't happen currently because the
                        // load and attach code bails out if _any_ of the timeline fails to fetch its IndexPart.
                        // That is, before we declare the Tenant as Active.
                        // But we only allow calls to delete_timeline on Active tenants.
                        return Err(DeleteTimelineError::Other(anyhow::anyhow!("upload queue is uninitialized, likely the timeline was in Broken state prior to this call because it failed to fetch IndexPart during load or attach, check the logs")));
                    }
                },
            }
        }

        // Stop & wait for the remaining timeline tasks, including upload tasks.
        // NB: This and other delete_timeline calls do not run as a task_mgr task,
        //     so, they are not affected by this shutdown_tasks() call.
        info!("waiting for timeline tasks to shutdown");
        task_mgr::shutdown_tasks(None, Some(self.tenant_id), Some(timeline_id)).await;

        // Mark timeline as deleted in S3 so we won't pick it up next time
        // during attach or pageserver restart.
        // See comment in persist_index_part_with_deleted_flag.
        if let Some(remote_client) = timeline.remote_client.as_ref() {
            match remote_client.persist_index_part_with_deleted_flag().await {
                // If we (now, or already) marked it successfully as deleted, we can proceed
                Ok(()) | Err(PersistIndexPartWithDeletedFlagError::AlreadyDeleted(_)) => (),
                // Bail out otherwise
                Err(e @ PersistIndexPartWithDeletedFlagError::AlreadyInProgress(_))
                | Err(e @ PersistIndexPartWithDeletedFlagError::Other(_)) => {
                    return Err(DeleteTimelineError::Other(anyhow::anyhow!(e)));
                }
            }
        }

        {
            // Grab the layer_removal_cs lock, and actually perform the deletion.
            //
            // This lock prevents multiple concurrent delete_timeline calls from
            // stepping on each other's toes, while deleting the files. It also
            // prevents GC or compaction from running at the same time.
            //
            // Note that there are still other race conditions between
            // GC, compaction and timeline deletion. GC task doesn't
            // register itself properly with the timeline it's
            // operating on. See
            // https://github.com/neondatabase/neon/issues/2671
            //
            // No timeout here, GC & Compaction should be responsive to the
            // `TimelineState::Stopping` change.
            info!("waiting for layer_removal_cs.lock()");
            let layer_removal_guard = timeline.layer_removal_cs.lock().await;
            info!("got layer_removal_cs.lock(), deleting layer files");

            // NB: storage_sync upload tasks that reference these layers have been cancelled
            //     by the caller.

            let local_timeline_directory = self.conf.timeline_path(&timeline_id, &self.tenant_id);

            fail::fail_point!("timeline-delete-before-rm", |_| {
                Err(anyhow::anyhow!("failpoint: timeline-delete-before-rm"))?
            });

            // NB: This need not be atomic because the deleted flag in the IndexPart
            // will be observed during tenant/timeline load. The deletion will be resumed there.
            //
            // For configurations without remote storage, we tolerate that we're not crash-safe here.
            // The timeline may come up Active but with missing layer files, in such setups.
            // See https://github.com/neondatabase/neon/pull/3919#issuecomment-1531726720
            match std::fs::remove_dir_all(&local_timeline_directory) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // This can happen if we're called a second time, e.g.,
                    // because of a previous failure/cancellation at/after
                    // failpoint timeline-delete-after-rm.
                    //
                    // It can also happen if we race with tenant detach, because,
                    // it doesn't grab the layer_removal_cs lock.
                    //
                    // For now, log and continue.
                    // warn! level is technically not appropriate for the
                    // first case because we should expect retries to happen.
                    // But the error is so rare, it seems better to get attention if it happens.
                    let tenant_state = self.current_state();
                    warn!(
                        timeline_dir=?local_timeline_directory,
                        ?tenant_state,
                        "timeline directory not found, proceeding anyway"
                    );
                    // continue with the rest of the deletion
                }
                res => res.with_context(|| {
                    format!(
                        "Failed to remove local timeline directory '{}'",
                        local_timeline_directory.display()
                    )
                })?,
            }

            info!("finished deleting layer files, releasing layer_removal_cs.lock()");
            drop(layer_removal_guard);
        }

        fail::fail_point!("timeline-delete-after-rm", |_| {
            Err(anyhow::anyhow!("failpoint: timeline-delete-after-rm"))?
        });

        // Remove the timeline from the map.
        let mut timelines = self.timelines.lock().unwrap();
        let children_exist = timelines
            .iter()
            .any(|(_, entry)| entry.get_ancestor_timeline_id() == Some(timeline_id));
        // XXX this can happen because `branch_timeline` doesn't check `TimelineState::Stopping`.
        // We already deleted the layer files, so it's probably best to panic.
        // (Ideally, above remove_dir_all is atomic so we don't see this timeline after a restart)
        if children_exist {
            panic!("Timeline grew children while we removed layer files");
        }
        let removed_timeline = timelines.remove(&timeline_id);
        if removed_timeline.is_none() {
            // This can legitimately happen if there's a concurrent call to this function.
            //   T1                                             T2
            //   lock
            //   unlock
            //                                                  lock
            //                                                  unlock
            //                                                  remove files
            //                                                  lock
            //                                                  remove from map
            //                                                  unlock
            //                                                  return
            //   remove files
            //   lock
            //   remove from map observes empty map
            //   unlock
            //   return
            debug!("concurrent call to this function won the race");
        }
        drop(timelines);

        Ok(())
    }

    pub fn current_state(&self) -> TenantState {
        self.state.borrow().clone()
    }

    pub fn is_active(&self) -> bool {
        self.current_state() == TenantState::Active
    }

    /// Changes tenant status to active, unless shutdown was already requested.
    fn activate(self: &Arc<Self>, broker_client: BrokerClientChannel, ctx: &RequestContext) {
        debug_assert_current_span_has_tenant_id();

        let mut activating = false;
        self.state.send_modify(|current_state| {
            match &*current_state {
                TenantState::Activating | TenantState::Active | TenantState::Broken { .. } | TenantState::Stopping => {
                    panic!("caller is responsible for calling activate() only on Loading / Attaching tenants, got {state:?}", state = current_state);
                }
               TenantState::Loading | TenantState::Attaching => {
                    *current_state = TenantState::Activating;
                    debug!(tenant_id = %self.tenant_id, "Activating tenant");
                    activating = true;
                    // Continue outside the closure. We need to grab timelines.lock()
                    // and we plan to turn it into a tokio::sync::Mutex in a future patch.
                }
            }
        });

        if activating {
            let timelines_accessor = self.timelines.lock().unwrap();
            let not_broken_timelines = timelines_accessor
                .values()
                .filter(|timeline| timeline.current_state() != TimelineState::Broken);

            // Spawn gc and compaction loops. The loops will shut themselves
            // down when they notice that the tenant is inactive.
            tasks::start_background_loops(self);

            let mut activated_timelines = 0;

            for timeline in not_broken_timelines {
                timeline.activate(broker_client.clone(), ctx);
                activated_timelines += 1;
            }

            self.state.send_modify(move |current_state| {
                assert_eq!(
                    *current_state,
                    TenantState::Activating,
                    "set_stopping and set_broken wait for us to leave Activating state",
                );
                *current_state = TenantState::Active;

                let elapsed = self.loading_started_at.elapsed();
                let total_timelines = timelines_accessor.len();

                // log a lot of stuff, because some tenants sometimes suffer from user-visible
                // times to activate. see https://github.com/neondatabase/neon/issues/4025
                info!(
                    since_creation_millis = elapsed.as_millis(),
                    tenant_id = %self.tenant_id,
                    activated_timelines,
                    total_timelines,
                    post_state = <&'static str>::from(&*current_state),
                    "activation attempt finished"
                );
            });
        }
    }

    /// Change tenant status to Stopping, to mark that it is being shut down.
    ///
    /// This function waits for the tenant to become active if it isn't already, before transitioning it into Stopping state.
    ///
    /// This function is not cancel-safe!
    pub async fn set_stopping(&self) -> Result<(), SetStoppingError> {
        let mut rx = self.state.subscribe();

        // cannot stop before we're done activating, so wait out until we're done activating
        rx.wait_for(|state| match state {
            TenantState::Activating | TenantState::Loading | TenantState::Attaching => {
                info!(
                    "waiting for {} to turn Active|Broken|Stopping",
                    <&'static str>::from(state)
                );
                false
            }
            TenantState::Active | TenantState::Broken { .. } | TenantState::Stopping {} => true,
        })
        .await
        .expect("cannot drop self.state while on a &self method");

        // we now know we're done activating, let's see whether this task is the winner to transition into Stopping
        let mut err = None;
        let stopping = self.state.send_if_modified(|current_state| match current_state {
            TenantState::Activating | TenantState::Loading | TenantState::Attaching => {
                unreachable!("we ensured above that we're done with activation, and, there is no re-activation")
            }
            TenantState::Active => {
                // FIXME: due to time-of-check vs time-of-use issues, it can happen that new timelines
                // are created after the transition to Stopping. That's harmless, as the Timelines
                // won't be accessible to anyone afterwards, because the Tenant is in Stopping state.
                *current_state = TenantState::Stopping;
                // Continue stopping outside the closure. We need to grab timelines.lock()
                // and we plan to turn it into a tokio::sync::Mutex in a future patch.
                true
            }
            TenantState::Broken { reason, .. } => {
                info!(
                    "Cannot set tenant to Stopping state, it is in Broken state due to: {reason}"
                );
                err = Some(SetStoppingError::Broken);
                false
            }
            TenantState::Stopping => {
                info!("Tenant is already in Stopping state");
                err = Some(SetStoppingError::AlreadyStopping);
                false
            }
        });
        match (stopping, err) {
            (true, None) => {} // continue
            (false, Some(err)) => return Err(err),
            (true, Some(_)) => unreachable!(
                "send_if_modified closure must error out if not transitioning to Stopping"
            ),
            (false, None) => unreachable!(
                "send_if_modified closure must return true if transitioning to Stopping"
            ),
        }

        let timelines_accessor = self.timelines.lock().unwrap();
        let not_broken_timelines = timelines_accessor
            .values()
            .filter(|timeline| timeline.current_state() != TimelineState::Broken);
        for timeline in not_broken_timelines {
            timeline.set_state(TimelineState::Stopping);
        }
        Ok(())
    }

    /// Method for tenant::mgr to transition us into Broken state in case of a late failure in
    /// `remove_tenant_from_memory`
    ///
    /// This function waits for the tenant to become active if it isn't already, before transitioning it into Stopping state.
    ///
    /// In tests, we also use this to set tenants to Broken state on purpose.
    pub(crate) async fn set_broken(&self, reason: String) {
        let mut rx = self.state.subscribe();

        // The load & attach routines own the tenant state until it has reached `Active`.
        // So, wait until it's done.
        rx.wait_for(|state| match state {
            TenantState::Activating | TenantState::Loading | TenantState::Attaching => {
                info!(
                    "waiting for {} to turn Active|Broken|Stopping",
                    <&'static str>::from(state)
                );
                false
            }
            TenantState::Active | TenantState::Broken { .. } | TenantState::Stopping {} => true,
        })
        .await
        .expect("cannot drop self.state while on a &self method");

        // we now know we're done activating, let's see whether this task is the winner to transition into Broken
        self.state.send_modify(|current_state| {
            match *current_state {
                TenantState::Activating | TenantState::Loading | TenantState::Attaching => {
                    unreachable!("we ensured above that we're done with activation, and, there is no re-activation")
                }
                TenantState::Active => {
                    if cfg!(feature = "testing") {
                        warn!("Changing Active tenant to Broken state, reason: {}", reason);
                        *current_state = TenantState::broken_from_reason(reason);
                    } else {
                        unreachable!("not allowed to call set_broken on Active tenants in non-testing builds")
                    }
                }
                TenantState::Broken { .. } => {
                    warn!("Tenant is already in Broken state");
                }
                // This is the only "expected" path, any other path is a bug.
                TenantState::Stopping => {
                    warn!(
                        "Marking Stopping tenant as Broken state, reason: {}",
                        reason
                    );
                    *current_state = TenantState::broken_from_reason(reason);
                }
           }
        });
    }

    pub fn subscribe_for_state_updates(&self) -> watch::Receiver<TenantState> {
        self.state.subscribe()
    }

    pub(crate) async fn wait_to_become_active(&self) -> Result<(), WaitToBecomeActiveError> {
        let mut receiver = self.state.subscribe();
        loop {
            let current_state = receiver.borrow_and_update().clone();
            match current_state {
                TenantState::Loading | TenantState::Attaching | TenantState::Activating => {
                    // in these states, there's a chance that we can reach ::Active
                    receiver.changed().await.map_err(
                        |_e: tokio::sync::watch::error::RecvError| {
                            WaitToBecomeActiveError::TenantDropped {
                                tenant_id: self.tenant_id,
                            }
                        },
                    )?;
                }
                TenantState::Active { .. } => {
                    return Ok(());
                }
                TenantState::Broken { .. } | TenantState::Stopping => {
                    // There's no chance the tenant can transition back into ::Active
                    return Err(WaitToBecomeActiveError::WillNotBecomeActive {
                        tenant_id: self.tenant_id,
                        state: current_state,
                    });
                }
            }
        }
    }
}

/// Given a Vec of timelines and their ancestors (timeline_id, ancestor_id),
/// perform a topological sort, so that the parent of each timeline comes
/// before the children.
fn tree_sort_timelines(
    timelines: HashMap<TimelineId, TimelineMetadata>,
) -> anyhow::Result<Vec<(TimelineId, TimelineMetadata)>> {
    let mut result = Vec::with_capacity(timelines.len());

    let mut now = Vec::with_capacity(timelines.len());
    // (ancestor, children)
    let mut later: HashMap<TimelineId, Vec<(TimelineId, TimelineMetadata)>> =
        HashMap::with_capacity(timelines.len());

    for (timeline_id, metadata) in timelines {
        if let Some(ancestor_id) = metadata.ancestor_timeline() {
            let children = later.entry(ancestor_id).or_default();
            children.push((timeline_id, metadata));
        } else {
            now.push((timeline_id, metadata));
        }
    }

    while let Some((timeline_id, metadata)) = now.pop() {
        result.push((timeline_id, metadata));
        // All children of this can be loaded now
        if let Some(mut children) = later.remove(&timeline_id) {
            now.append(&mut children);
        }
    }

    // All timelines should be visited now. Unless there were timelines with missing ancestors.
    if !later.is_empty() {
        for (missing_id, orphan_ids) in later {
            for (orphan_id, _) in orphan_ids {
                error!("could not load timeline {orphan_id} because its ancestor timeline {missing_id} could not be loaded");
            }
        }
        bail!("could not load tenant because some timelines are missing ancestors");
    }

    Ok(result)
}

impl Tenant {
    pub fn tenant_specific_overrides(&self) -> TenantConfOpt {
        *self.tenant_conf.read().unwrap()
    }

    pub fn effective_config(&self) -> TenantConf {
        self.tenant_specific_overrides()
            .merge(self.conf.default_tenant_conf)
    }

    pub fn get_checkpoint_distance(&self) -> u64 {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .checkpoint_distance
            .unwrap_or(self.conf.default_tenant_conf.checkpoint_distance)
    }

    pub fn get_checkpoint_timeout(&self) -> Duration {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .checkpoint_timeout
            .unwrap_or(self.conf.default_tenant_conf.checkpoint_timeout)
    }

    pub fn get_compaction_target_size(&self) -> u64 {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .compaction_target_size
            .unwrap_or(self.conf.default_tenant_conf.compaction_target_size)
    }

    pub fn get_compaction_period(&self) -> Duration {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .compaction_period
            .unwrap_or(self.conf.default_tenant_conf.compaction_period)
    }

    pub fn get_compaction_threshold(&self) -> usize {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .compaction_threshold
            .unwrap_or(self.conf.default_tenant_conf.compaction_threshold)
    }

    pub fn get_gc_horizon(&self) -> u64 {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .gc_horizon
            .unwrap_or(self.conf.default_tenant_conf.gc_horizon)
    }

    pub fn get_gc_period(&self) -> Duration {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .gc_period
            .unwrap_or(self.conf.default_tenant_conf.gc_period)
    }

    pub fn get_image_creation_threshold(&self) -> usize {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .image_creation_threshold
            .unwrap_or(self.conf.default_tenant_conf.image_creation_threshold)
    }

    pub fn get_pitr_interval(&self) -> Duration {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .pitr_interval
            .unwrap_or(self.conf.default_tenant_conf.pitr_interval)
    }

    pub fn get_trace_read_requests(&self) -> bool {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .trace_read_requests
            .unwrap_or(self.conf.default_tenant_conf.trace_read_requests)
    }

    pub fn get_min_resident_size_override(&self) -> Option<u64> {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .min_resident_size_override
            .or(self.conf.default_tenant_conf.min_resident_size_override)
    }

    pub fn set_new_tenant_config(&self, new_tenant_conf: TenantConfOpt) {
        *self.tenant_conf.write().unwrap() = new_tenant_conf;
        // Don't hold self.timelines.lock() during the notifies.
        // There's no risk of deadlock right now, but there could be if we consolidate
        // mutexes in struct Timeline in the future.
        let timelines = self.list_timelines();
        for timeline in timelines {
            timeline.tenant_conf_updated();
        }
    }

    fn create_timeline_data(
        &self,
        new_timeline_id: TimelineId,
        new_metadata: &TimelineMetadata,
        ancestor: Option<Arc<Timeline>>,
        remote_client: Option<Arc<RemoteTimelineClient>>,
    ) -> anyhow::Result<Arc<Timeline>> {
        if let Some(ancestor_timeline_id) = new_metadata.ancestor_timeline() {
            anyhow::ensure!(
                ancestor.is_some(),
                "Timeline's {new_timeline_id} ancestor {ancestor_timeline_id} was not found"
            )
        }

        let pg_version = new_metadata.pg_version();
        Ok(Timeline::new(
            self.conf,
            Arc::clone(&self.tenant_conf),
            new_metadata,
            ancestor,
            new_timeline_id,
            self.tenant_id,
            Arc::clone(&self.walredo_mgr),
            remote_client,
            pg_version,
            false,
        ))
    }

    /// See the error variants for how to handle errors from this function.
    fn start_creating_timeline(
        &self,
        timeline_id: TimelineId,
    ) -> Result<CreatingTimelineGuard, StartCreatingTimelineError> {
        // copied this from unit tests
        let dummy_metadata = TimelineMetadata::new(
            Lsn(0),
            None,
            None,
            Lsn(0),
            Lsn(0),
            Lsn(0),
            // Any version will do
            // but it should be consistent with the one in the tests
            crate::DEFAULT_PG_VERSION,
        );
        let placeholder = Timeline::new(
            self.conf,
            Arc::clone(&self.tenant_conf),
            &dummy_metadata,
            None,
            timeline_id,
            self.tenant_id,
            Arc::clone(&self.walredo_mgr),
            None,
            crate::DEFAULT_PG_VERSION,
            true,
        );

        let timeline_path = self.conf.timeline_path(&timeline_id, &self.tenant_id);
        let uninit_mark_path = self
            .conf
            .timeline_uninit_mark_file_path(self.tenant_id, timeline_id);

        let check_uninit_mark_not_exist = || {
            let exists = uninit_mark_path
                .try_exists()
                .context("check uninit mark file existence")?;
            if exists {
                return Err(StartCreatingTimelineError::AlreadyExists {
                    timeline_id,
                    existing_state: "uninit mark file",
                });
            }
            Ok(())
        };

        let check_timeline_path_not_exist = || {
            let exists = timeline_path
                .try_exists()
                .context("check timeline directory existence")?;
            if exists {
                return Err(StartCreatingTimelineError::AlreadyExists {
                    timeline_id,
                    existing_state: "timeline directory",
                });
            }
            Ok(())
        };

        // TODO should we check for state in s3 as well?
        // Right now we're overwriting IndexPart but other layer files would remain.

        // do a few opportunistic checks before trying to get out spot
        check_uninit_mark_not_exist()?;
        check_timeline_path_not_exist()?;

        // Put the placeholder into the map.
        let placeholder_timeline: Arc<Timeline> = {
            match self.timelines.lock().unwrap().entry(timeline_id) {
                Entry::Occupied(_) => {
                    return Err(StartCreatingTimelineError::AlreadyExists {
                        timeline_id,
                        existing_state: "timelines map entry",
                    });
                }
                Entry::Vacant(v) => {
                    v.insert(Arc::clone(&placeholder));
                    placeholder
                }
            }
        };

        // Do all the checks again, now we know that we won.
        check_timeline_path_not_exist()?;
        check_uninit_mark_not_exist()?;

        let create_uninit_mark_file = || {
            fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&uninit_mark_path)
                .context("create uninit mark file")?;
            crashsafe::fsync_file_and_parent(&uninit_mark_path)
                .context("fsync uninit mark file and parent dir")?;
            Ok(uninit_mark_path)
        };

        let uninit_mark_path = match create_uninit_mark_file() {
            Ok(uninit_mark_path) => uninit_mark_path,
            Err(err) => {
                // If we failed to create the uninit mark, remove the placeholder
                // timeline from the map.
                let removed = self.timelines.lock().unwrap().remove(&timeline_id);
                assert!(removed.is_some());
                assert!(compare_arced_timeline(
                    &removed.unwrap(),
                    &placeholder_timeline
                ));
                return Err(err);
            }
        };

        Ok(CreatingTimelineGuard {
            owning_tenant: self,
            timeline_id,
            placeholder_timeline,
            uninit_mark_path,
            timeline_path,
        })
    }

    fn new(
        state: TenantState,
        conf: &'static PageServerConf,
        tenant_conf: TenantConfOpt,
        walredo_mgr: Arc<dyn WalRedoManager + Send + Sync>,
        tenant_id: TenantId,
        remote_storage: Option<GenericRemoteStorage>,
    ) -> Tenant {
        let (state, mut rx) = watch::channel(state);

        tokio::spawn(async move {
            let mut current_state: &'static str = From::from(&*rx.borrow_and_update());
            let tid = tenant_id.to_string();
            TENANT_STATE_METRIC
                .with_label_values(&[&tid, current_state])
                .inc();
            loop {
                match rx.changed().await {
                    Ok(()) => {
                        let new_state: &'static str = From::from(&*rx.borrow_and_update());
                        TENANT_STATE_METRIC
                            .with_label_values(&[&tid, current_state])
                            .dec();
                        TENANT_STATE_METRIC
                            .with_label_values(&[&tid, new_state])
                            .inc();

                        current_state = new_state;
                    }
                    Err(_sender_dropped_error) => {
                        info!("Tenant dropped the state updates sender, quitting waiting for tenant state change");
                        return;
                    }
                }
            }
        });

        Tenant {
            tenant_id,
            conf,
            // using now here is good enough approximation to catch tenants with really long
            // activation times.
            loading_started_at: Instant::now(),
            tenant_conf: Arc::new(RwLock::new(tenant_conf)),
            timelines: Mutex::new(HashMap::new()),
            gc_cs: tokio::sync::Mutex::new(()),
            walredo_mgr,
            remote_storage,
            state,
            cached_logical_sizes: tokio::sync::Mutex::new(HashMap::new()),
            cached_synthetic_tenant_size: Arc::new(AtomicU64::new(0)),
            eviction_task_tenant_state: tokio::sync::Mutex::new(EvictionTaskTenantState::default()),
        }
    }

    /// Locate and load config
    pub(super) fn load_tenant_config(
        conf: &'static PageServerConf,
        tenant_id: TenantId,
    ) -> anyhow::Result<TenantConfOpt> {
        let target_config_path = conf.tenant_config_path(tenant_id);
        let target_config_display = target_config_path.display();

        info!("loading tenantconf from {target_config_display}");

        // FIXME If the config file is not found, assume that we're attaching
        // a detached tenant and config is passed via attach command.
        // https://github.com/neondatabase/neon/issues/1555
        if !target_config_path.exists() {
            info!("tenant config not found in {target_config_display}");
            return Ok(TenantConfOpt::default());
        }

        // load and parse file
        let config = fs::read_to_string(&target_config_path).with_context(|| {
            format!("Failed to load config from path '{target_config_display}'")
        })?;

        let toml = config.parse::<toml_edit::Document>().with_context(|| {
            format!("Failed to parse config from file '{target_config_display}' as toml file")
        })?;

        let mut tenant_conf = TenantConfOpt::default();
        for (key, item) in toml.iter() {
            match key {
                "tenant_config" => {
                    tenant_conf = PageServerConf::parse_toml_tenant_conf(item).with_context(|| {
                        format!("Failed to parse config from file '{target_config_display}' as pageserver config")
                    })?;
                }
                _ => bail!("config file {target_config_display} has unrecognized pageserver option '{key}'"),

            }
        }

        Ok(tenant_conf)
    }

    pub(super) fn persist_tenant_config(
        tenant_id: &TenantId,
        target_config_path: &Path,
        tenant_conf: TenantConfOpt,
        creating_tenant: bool,
    ) -> anyhow::Result<()> {
        let _enter = info_span!("saving tenantconf").entered();

        // imitate a try-block with a closure
        let do_persist = |target_config_path: &Path| -> anyhow::Result<()> {
            let target_config_parent = target_config_path.parent().with_context(|| {
                format!(
                    "Config path does not have a parent: {}",
                    target_config_path.display()
                )
            })?;

            info!("persisting tenantconf to {}", target_config_path.display());

            let mut conf_content = r#"# This file contains a specific per-tenant's config.
#  It is read in case of pageserver restart.

[tenant_config]
"#
            .to_string();

            // Convert the config to a toml file.
            conf_content += &toml_edit::ser::to_string(&tenant_conf)?;

            let mut target_config_file = VirtualFile::open_with_options(
                target_config_path,
                OpenOptions::new()
                    .truncate(true) // This needed for overwriting with small config files
                    .write(true)
                    .create_new(creating_tenant)
                    // when creating a new tenant, first_save will be true and `.create(true)` will be
                    // ignored (per rust std docs).
                    //
                    // later when updating the config of created tenant, or persisting config for the
                    // first time for attached tenant, the `.create(true)` is used.
                    .create(true),
            )?;

            target_config_file
                .write(conf_content.as_bytes())
                .context("write toml bytes into file")
                .and_then(|_| target_config_file.sync_all().context("fsync config file"))
                .context("write config file")?;

            // fsync the parent directory to ensure the directory entry is durable.
            // before this was done conditionally on creating_tenant, but these management actions are rare
            // enough to just fsync it always.

            crashsafe::fsync(target_config_parent)?;
            // XXX we're not fsyncing the parent dir, need to do that in case `creating_tenant`
            Ok(())
        };

        // this function is called from creating the tenant and updating the tenant config, which
        // would otherwise share this context, so keep it here in one place.
        do_persist(target_config_path).with_context(|| {
            format!(
                "write tenant {tenant_id} config to {}",
                target_config_path.display()
            )
        })
    }

    //
    // How garbage collection works:
    //
    //                    +--bar------------->
    //                   /
    //             +----+-----foo---------------->
    //            /
    // ----main--+-------------------------->
    //                \
    //                 +-----baz-------->
    //
    //
    // 1. Grab 'gc_cs' mutex to prevent new timelines from being created while Timeline's
    //    `gc_infos` are being refreshed
    // 2. Scan collected timelines, and on each timeline, make note of the
    //    all the points where other timelines have been branched off.
    //    We will refrain from removing page versions at those LSNs.
    // 3. For each timeline, scan all layer files on the timeline.
    //    Remove all files for which a newer file exists and which
    //    don't cover any branch point LSNs.
    //
    // TODO:
    // - if a relation has a non-incremental persistent layer on a child branch, then we
    //   don't need to keep that in the parent anymore. But currently
    //   we do.
    async fn gc_iteration_internal(
        &self,
        target_timeline_id: Option<TimelineId>,
        horizon: u64,
        pitr: Duration,
        ctx: &RequestContext,
    ) -> anyhow::Result<GcResult> {
        let mut totals: GcResult = Default::default();
        let now = Instant::now();

        let gc_timelines = self
            .refresh_gc_info_internal(target_timeline_id, horizon, pitr, ctx)
            .await?;

        utils::failpoint_sleep_millis_async!("gc_iteration_internal_after_getting_gc_timelines");

        // If there is nothing to GC, we don't want any messages in the INFO log.
        if !gc_timelines.is_empty() {
            info!("{} timelines need GC", gc_timelines.len());
        } else {
            debug!("{} timelines need GC", gc_timelines.len());
        }

        // Perform GC for each timeline.
        //
        // Note that we don't hold the GC lock here because we don't want
        // to delay the branch creation task, which requires the GC lock.
        // A timeline GC iteration can be slow because it may need to wait for
        // compaction (both require `layer_removal_cs` lock),
        // but the GC iteration can run concurrently with branch creation.
        //
        // See comments in [`Tenant::branch_timeline`] for more information
        // about why branch creation task can run concurrently with timeline's GC iteration.
        for timeline in gc_timelines {
            if task_mgr::is_shutdown_requested() {
                // We were requested to shut down. Stop and return with the progress we
                // made.
                break;
            }
            let result = timeline.gc().await?;
            totals += result;
        }

        totals.elapsed = now.elapsed();
        Ok(totals)
    }

    /// Refreshes the Timeline::gc_info for all timelines, returning the
    /// vector of timelines which have [`Timeline::get_last_record_lsn`] past
    /// [`Tenant::get_gc_horizon`].
    ///
    /// This is usually executed as part of periodic gc, but can now be triggered more often.
    pub async fn refresh_gc_info(
        &self,
        ctx: &RequestContext,
    ) -> anyhow::Result<Vec<Arc<Timeline>>> {
        // since this method can now be called at different rates than the configured gc loop, it
        // might be that these configuration values get applied faster than what it was previously,
        // since these were only read from the gc task.
        let horizon = self.get_gc_horizon();
        let pitr = self.get_pitr_interval();

        // refresh all timelines
        let target_timeline_id = None;

        self.refresh_gc_info_internal(target_timeline_id, horizon, pitr, ctx)
            .await
    }

    async fn refresh_gc_info_internal(
        &self,
        target_timeline_id: Option<TimelineId>,
        horizon: u64,
        pitr: Duration,
        ctx: &RequestContext,
    ) -> anyhow::Result<Vec<Arc<Timeline>>> {
        // grab mutex to prevent new timelines from being created here.
        let gc_cs = self.gc_cs.lock().await;

        // Scan all timelines. For each timeline, remember the timeline ID and
        // the branch point where it was created.
        let (all_branchpoints, timeline_ids): (BTreeSet<(TimelineId, Lsn)>, _) = {
            let timelines = self.timelines.lock().unwrap();
            let mut all_branchpoints = BTreeSet::new();
            let timeline_ids = {
                if let Some(target_timeline_id) = target_timeline_id.as_ref() {
                    if timelines.get(target_timeline_id).is_none() {
                        bail!("gc target timeline does not exist")
                    }
                };

                timelines
                    .iter()
                    .map(|(timeline_id, timeline_entry)| {
                        if let Some(ancestor_timeline_id) =
                            &timeline_entry.get_ancestor_timeline_id()
                        {
                            // If target_timeline is specified, we only need to know branchpoints of its children
                            if let Some(timeline_id) = target_timeline_id {
                                if ancestor_timeline_id == &timeline_id {
                                    all_branchpoints.insert((
                                        *ancestor_timeline_id,
                                        timeline_entry.get_ancestor_lsn(),
                                    ));
                                }
                            }
                            // Collect branchpoints for all timelines
                            else {
                                all_branchpoints.insert((
                                    *ancestor_timeline_id,
                                    timeline_entry.get_ancestor_lsn(),
                                ));
                            }
                        }

                        *timeline_id
                    })
                    .collect::<Vec<_>>()
            };
            (all_branchpoints, timeline_ids)
        };

        // Ok, we now know all the branch points.
        // Update the GC information for each timeline.
        let mut gc_timelines = Vec::with_capacity(timeline_ids.len());
        for timeline_id in timeline_ids {
            // Timeline is known to be local and loaded.
            let timeline = self
                .get_timeline(timeline_id, false)
                .with_context(|| format!("Timeline {timeline_id} was not found"))?;

            // If target_timeline is specified, ignore all other timelines
            if let Some(target_timeline_id) = target_timeline_id {
                if timeline_id != target_timeline_id {
                    continue;
                }
            }

            if let Some(cutoff) = timeline.get_last_record_lsn().checked_sub(horizon) {
                let branchpoints: Vec<Lsn> = all_branchpoints
                    .range((
                        Included((timeline_id, Lsn(0))),
                        Included((timeline_id, Lsn(u64::MAX))),
                    ))
                    .map(|&x| x.1)
                    .collect();
                timeline
                    .update_gc_info(branchpoints, cutoff, pitr, ctx)
                    .await?;

                gc_timelines.push(timeline);
            }
        }
        drop(gc_cs);
        Ok(gc_timelines)
    }

    /// A substitute for `branch_timeline` for use in unit tests.
    /// The returned timeline will have state value `Active` to make various `anyhow::ensure!()`
    /// calls pass, but, we do not actually call `.activate()` under the hood. So, none of the
    /// timeline background tasks are launched, except the flush loop.
    #[cfg(test)]
    #[instrument(skip_all, fields(tenant_id=%self.tenant_id))]
    async fn branch_timeline_test(
        &self,
        src_timeline: &Arc<Timeline>,
        dst_id: TimelineId,
        start_lsn: Option<Lsn>,
        ctx: &RequestContext,
    ) -> anyhow::Result<Arc<Timeline>> {
        //TODO can't we just use create_timeline here?

        let guard = self
            .start_creating_timeline(dst_id)
            .context("create creating placeholder timeline")?;

        let create_ondisk_state = async {
            self.branch_timeline_impl(src_timeline, dst_id, start_lsn, None, &guard, ctx)
                .await
                .context("branch_timeline_impl")?;
            anyhow::Ok(())
        };
        let placeholder_timeline = match create_ondisk_state.await {
            Ok(()) => {
                match guard.creation_complete_remove_uninit_marker_and_get_placeholder_timeline() {
                    Ok(placeholder_timeline) => placeholder_timeline,
                    Err(err) => {
                        error!(
                            "failed to remove uninit marker for new_timeline_id={dst_id}: {err:#}"
                        );
                        return Err(err);
                    }
                }
            }
            Err(err) => {
                error!("failed to create on-disk state for new_timeline_id={dst_id}: {err:#}");
                guard.creation_failed();
                return Err(err);
            }
        };

        // From here on, it's just like during pageserver startup.
        let metadata = load_metadata(self.conf, dst_id, self.tenant_id)
            .context("load newly created on-disk timeline metadata")?;

        let real_timeline = self
            .load_local_timeline(
                dst_id,
                metadata,
                AncestorArg::ancestor(Arc::clone(src_timeline)),
                TimelineLoadCause::Test,
                ctx,
            )
            .instrument(info_span!("load_local_timeline", timeline_id=%dst_id))
            .await
            .context("load newly created on-disk timeline state")?
            .unwrap();

        match self.timelines.lock().unwrap().entry(dst_id) {
            Entry::Vacant(_) => unreachable!("we created a placeholder earlier, and load_local_timeline should have inserted the real timeline"),
            Entry::Occupied(mut o) => {
                info!("replacing placeholder timeline with the real one");
                assert_eq!(placeholder_timeline.current_state(), TimelineState::Creating);
                assert!(compare_arced_timeline(&placeholder_timeline, o.get()));
                let replaced_placeholder = o.insert(Arc::clone(&real_timeline));
                assert!(compare_arced_timeline(&replaced_placeholder, &placeholder_timeline));
            },
        }

        real_timeline.set_state(TimelineState::Active);
        real_timeline.maybe_spawn_flush_loop();
        Ok(real_timeline)
    }

    /// Branch an existing timeline, creating local and remote files.
    async fn branch_timeline(
        &self,
        src_timeline: &Arc<Timeline>,
        dst_id: TimelineId,
        start_lsn: Option<Lsn>,
        remote_client: Option<Arc<RemoteTimelineClient>>,
        guard: &CreatingTimelineGuard<'_>,
        ctx: &RequestContext,
    ) -> anyhow::Result<()> {
        self.branch_timeline_impl(src_timeline, dst_id, start_lsn, remote_client, guard, ctx)
            .await
    }

    async fn branch_timeline_impl(
        &self,
        src_timeline: &Arc<Timeline>,
        dst_id: TimelineId,
        start_lsn: Option<Lsn>,
        remote_client: Option<Arc<RemoteTimelineClient>>,
        guard: &CreatingTimelineGuard<'_>,
        _ctx: &RequestContext,
    ) -> anyhow::Result<()> {
        let src_id = src_timeline.timeline_id;

        // If no start LSN is specified, we branch the new timeline from the source timeline's last record LSN
        let start_lsn = start_lsn.unwrap_or_else(|| {
            let lsn = src_timeline.get_last_record_lsn();
            info!("branching timeline {dst_id} from timeline {src_id} at last record LSN: {lsn}");
            lsn
        });

        // First acquire the GC lock so that another task cannot advance the GC
        // cutoff in 'gc_info', and make 'start_lsn' invalid, while we are
        // creating the branch.
        let _gc_cs = self.gc_cs.lock().await;

        // Ensure that `start_lsn` is valid, i.e. the LSN is within the PITR
        // horizon on the source timeline
        //
        // We check it against both the planned GC cutoff stored in 'gc_info',
        // and the 'latest_gc_cutoff' of the last GC that was performed.  The
        // planned GC cutoff in 'gc_info' is normally larger than
        // 'latest_gc_cutoff_lsn', but beware of corner cases like if you just
        // changed the GC settings for the tenant to make the PITR window
        // larger, but some of the data was already removed by an earlier GC
        // iteration.

        // check against last actual 'latest_gc_cutoff' first
        let latest_gc_cutoff_lsn = src_timeline.get_latest_gc_cutoff_lsn();
        src_timeline
            .check_lsn_is_in_scope(start_lsn, &latest_gc_cutoff_lsn)
            .context(format!(
                "invalid branch start lsn: less than latest GC cutoff {}",
                *latest_gc_cutoff_lsn,
            ))?;

        // and then the planned GC cutoff
        {
            let gc_info = src_timeline.gc_info.read().unwrap();
            let cutoff = min(gc_info.pitr_cutoff, gc_info.horizon_cutoff);
            if start_lsn < cutoff {
                bail!(format!(
                    "invalid branch start lsn: less than planned GC cutoff {cutoff}"
                ));
            }
        }

        //
        // The branch point is valid, and we are still holding the 'gc_cs' lock
        // so that GC cannot advance the GC cutoff until we are finished.
        // Proceed with the branch creation.
        //

        // Determine prev-LSN for the new timeline. We can only determine it if
        // the timeline was branched at the current end of the source timeline.
        let RecordLsn {
            last: src_last,
            prev: src_prev,
        } = src_timeline.get_last_record_rlsn();
        let dst_prev = if src_last == start_lsn {
            Some(src_prev)
        } else {
            None
        };

        // Create the metadata file, noting the ancestor of the new timeline.
        // There is initially no data in it, but all the read-calls know to look
        // into the ancestor.
        let metadata = TimelineMetadata::new(
            start_lsn,
            dst_prev,
            Some(src_id),
            start_lsn,
            *src_timeline.latest_gc_cutoff_lsn.read(), // FIXME: should we hold onto this guard longer?
            src_timeline.initdb_lsn,
            src_timeline.pg_version,
        );

        self.create_timeline_files(&guard.timeline_path, dst_id, &metadata)
            .context("create timeline files")?;

        // Root timeline gets its layers during creation and uploads them along with the metadata.
        // A branch timeline though, when created, can get no writes for some time, hence won't get any layers created.
        // We still need to upload its metadata eagerly: if other nodes `attach` the tenant and miss this timeline, their GC
        // could get incorrect information and remove more layers, than needed.
        // See also https://github.com/neondatabase/neon/issues/3865
        if let Some(remote_client) = remote_client.as_ref() {
            remote_client.init_upload_queue_for_empty_remote(&metadata)?;
            remote_client
                .schedule_index_upload_for_metadata_update(&metadata)
                .context("branch initial metadata upload")?;
            remote_client
                .wait_completion()
                .await
                .context("wait for initial uploads to complete")?;
        }

        // XXX log message is a little too early, see caller for context
        info!("branched timeline {dst_id} from {src_id} at {start_lsn}");

        Ok(())
    }

    /// - run initdb to init temporary instance and get bootstrap data
    /// - after initialization complete, remove the temp dir.
    ///
    /// This method takes ownership of the remote_client and finishes uploads itself.
    async fn bootstrap_timeline(
        &self,
        timeline_id: TimelineId,
        pg_version: u32,
        guard: &CreatingTimelineGuard<'_>,
        remote_client: Option<Arc<RemoteTimelineClient>>,
        ctx: &RequestContext,
    ) -> anyhow::Result<()> {
        let tenant_id = self.tenant_id;

        // create a `tenant/{tenant_id}/timelines/basebackup-{timeline_id}.{TEMP_FILE_SUFFIX}/`
        // temporary directory for basebackup files for the given timeline.
        let initdb_path = path_with_suffix_extension(
            self.conf
                .timelines_path(&self.tenant_id)
                .join(format!("basebackup-{timeline_id}")),
            TEMP_FILE_SUFFIX,
        );

        // an uninit mark was placed before, nothing else can access this timeline files
        // current initdb was not run yet, so remove whatever was left from the previous runs
        if initdb_path.exists() {
            fs::remove_dir_all(&initdb_path).with_context(|| {
                format!(
                    "Failed to remove already existing initdb directory: {}",
                    initdb_path.display()
                )
            })?;
        }
        // Init temporarily repo to get bootstrap data, this creates a directory in the `initdb_path` path
        run_initdb(self.conf, &initdb_path, pg_version)?;
        // this new directory is very temporary, set to remove it immediately after bootstrap, we don't need it
        scopeguard::defer! {
            if let Err(e) = fs::remove_dir_all(&initdb_path) {
                // this is unlikely, but we will remove the directory on pageserver restart or another bootstrap call
                error!("Failed to remove temporary initdb directory '{}': {}", initdb_path.display(), e);
            }
        }
        let pgdata_path = &initdb_path;
        let pgdata_lsn = import_datadir::get_lsn_from_controlfile(pgdata_path)?.align();

        // Import the contents of the data directory at the initial checkpoint
        // LSN, and any WAL after that.
        // Initdb lsn will be equal to last_record_lsn which will be set after import.
        // Because we know it upfront avoid having an option or dummy zero value by passing it to the metadata.
        let new_metadata = TimelineMetadata::new(
            Lsn(0),
            None,
            None,
            Lsn(0),
            pgdata_lsn,
            pgdata_lsn,
            pg_version,
        );

        self.create_timeline_files(&guard.timeline_path, timeline_id, &new_metadata)
            .context("create timeline files")?;

        if let Some(remote_client) = remote_client.as_ref() {
            remote_client.init_upload_queue_for_empty_remote(&new_metadata)?;
            // the freeze_and_flush below will schedule the metadata upload
        }

        // Temporarily create a timeline object to allow the import to run in it.

        let remote_client_refcount_before = remote_client
            .as_ref()
            .map(|rc| (Arc::strong_count(rc), Arc::weak_count(rc)));
        // Ensure the remote_client hasn't leaked into some global state.
        // TODO: move ownership into the unfinished_timeline and back out.
        scopeguard::defer!(
            let remote_client_refcount_after = remote_client.as_ref().map(|rc| (Arc::strong_count(rc), Arc::weak_count(rc)));
            assert_eq!(remote_client_refcount_before, remote_client_refcount_after, "the remote_client must not leak this function call graph")
        );

        let unfinished_timeline = self
            .create_timeline_data(timeline_id, &new_metadata, None, remote_client.clone())
            .context("Failed to create timeline data structure")?;

        unfinished_timeline
            .layers
            .write()
            .unwrap()
            .next_open_layer_at = Some(pgdata_lsn); // pgdata_lsn == initdb_lsn

        import_datadir::import_timeline_from_postgres_datadir(
            &unfinished_timeline,
            pgdata_path,
            pgdata_lsn,
            ctx,
        )
        .await
        .with_context(|| {
            format!("Failed to import pgdatadir for timeline {tenant_id}/{timeline_id}")
        })?;

        // Flush the new layer files to disk, before we mark the timeline as available to
        // the outside world.
        //
        // Thus spawn flush loop manually and skip flush_loop setup in initialize_with_lock
        unfinished_timeline.maybe_spawn_flush_loop();

        fail::fail_point!("before-checkpoint-new-timeline", |_| {
            anyhow::bail!("failpoint before-checkpoint-new-timeline");
        });

        unfinished_timeline
            .freeze_and_flush()
            .await
            .with_context(|| {
                format!(
                    "Failed to flush after pgdatadir import for timeline {tenant_id}/{timeline_id}"
                )
            })?;

        let last_record_lsn = unfinished_timeline.get_last_record_lsn();

        // Tear down the temporary timeline.
        // XXX this should be a shared Timeline::shutdown method.

        if let Some(remote_client) = remote_client.as_ref() {
            remote_client
                .wait_completion()
                .await
                .context("wait for uploads to complete so we can stop the unfinished_timeline")?;
        }

        // XXX this is same shutdown code as in Timeline::delete, share it.
        unfinished_timeline.set_state(TimelineState::Stopping);
        task_mgr::shutdown_tasks(None, Some(self.tenant_id), Some(timeline_id)).await;

        // XXX log message is a little too early, see caller for context
        info!(
            "created root timeline {} timeline.lsn {}",
            timeline_id, last_record_lsn,
        );

        Ok(())
    }

    fn create_timeline_files(
        &self,
        timeline_path: &Path,
        new_timeline_id: TimelineId,
        new_metadata: &TimelineMetadata,
    ) -> anyhow::Result<()> {
        crashsafe::create_dir_all(timeline_path).context("Failed to create timeline directory")?;

        fail::fail_point!("after-timeline-uninit-mark-creation", |_| {
            error!("hitting failpoint after-timeline-uninit-mark-creation");
            anyhow::bail!("failpoint after-timeline-uninit-mark-creation");
        });

        save_metadata(
            self.conf,
            new_timeline_id,
            self.tenant_id,
            new_metadata,
            true,
        )
        .context("Failed to create timeline metadata")?;

        Ok(())
    }

    /// Gathers inputs from all of the timelines to produce a sizing model input.
    ///
    /// Future is cancellation safe. Only one calculation can be running at once per tenant.
    #[instrument(skip_all, fields(tenant_id=%self.tenant_id))]
    pub async fn gather_size_inputs(
        &self,
        // `max_retention_period` overrides the cutoff that is used to calculate the size
        // (only if it is shorter than the real cutoff).
        max_retention_period: Option<u64>,
        cause: LogicalSizeCalculationCause,
        ctx: &RequestContext,
    ) -> anyhow::Result<size::ModelInputs> {
        let logical_sizes_at_once = self
            .conf
            .concurrent_tenant_size_logical_size_queries
            .inner();

        // TODO: Having a single mutex block concurrent reads is not great for performance.
        //
        // But the only case where we need to run multiple of these at once is when we
        // request a size for a tenant manually via API, while another background calculation
        // is in progress (which is not a common case).
        //
        // See more for on the issue #2748 condenced out of the initial PR review.
        let mut shared_cache = self.cached_logical_sizes.lock().await;

        size::gather_inputs(
            self,
            logical_sizes_at_once,
            max_retention_period,
            &mut shared_cache,
            cause,
            ctx,
        )
        .await
    }

    /// Calculate synthetic tenant size and cache the result.
    /// This is periodically called by background worker.
    /// result is cached in tenant struct
    #[instrument(skip_all, fields(tenant_id=%self.tenant_id))]
    pub async fn calculate_synthetic_size(
        &self,
        cause: LogicalSizeCalculationCause,
        ctx: &RequestContext,
    ) -> anyhow::Result<u64> {
        let inputs = self.gather_size_inputs(None, cause, ctx).await?;

        let size = inputs.calculate()?;

        self.set_cached_synthetic_size(size);

        Ok(size)
    }

    /// Cache given synthetic size and update the metric value
    pub fn set_cached_synthetic_size(&self, size: u64) {
        self.cached_synthetic_tenant_size
            .store(size, Ordering::Relaxed);

        TENANT_SYNTHETIC_SIZE_METRIC
            .get_metric_with_label_values(&[&self.tenant_id.to_string()])
            .unwrap()
            .set(size);
    }

    pub fn get_cached_synthetic_size(&self) -> u64 {
        self.cached_synthetic_tenant_size.load(Ordering::Relaxed)
    }
}

fn remove_timeline_and_uninit_mark(timeline_dir: &Path, uninit_mark: &Path) -> anyhow::Result<()> {
    fs::remove_dir_all(timeline_dir)
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                // we can leave the uninit mark without a timeline dir,
                // just remove the mark then
                Ok(())
            } else {
                Err(e)
            }
        })
        .with_context(|| {
            format!(
                "Failed to remove unit marked timeline directory {}",
                timeline_dir.display()
            )
        })?;
    fs::remove_file(uninit_mark).with_context(|| {
        format!(
            "Failed to remove timeline uninit mark file {}",
            uninit_mark.display()
        )
    })?;

    Ok(())
}

pub(crate) enum CreateTenantFilesMode {
    Create,
    Attach,
}

pub(crate) fn create_tenant_files(
    conf: &'static PageServerConf,
    tenant_conf: TenantConfOpt,
    tenant_id: TenantId,
    mode: CreateTenantFilesMode,
) -> anyhow::Result<PathBuf> {
    let target_tenant_directory = conf.tenant_path(&tenant_id);
    anyhow::ensure!(
        !target_tenant_directory
            .try_exists()
            .context("check existence of tenant directory")?,
        "tenant directory already exists",
    );

    let temporary_tenant_dir =
        path_with_suffix_extension(&target_tenant_directory, TEMP_FILE_SUFFIX);
    debug!(
        "Creating temporary directory structure in {}",
        temporary_tenant_dir.display()
    );

    // top-level dir may exist if we are creating it through CLI
    crashsafe::create_dir_all(&temporary_tenant_dir).with_context(|| {
        format!(
            "could not create temporary tenant directory {}",
            temporary_tenant_dir.display()
        )
    })?;

    let creation_result = try_create_target_tenant_dir(
        conf,
        tenant_conf,
        tenant_id,
        mode,
        &temporary_tenant_dir,
        &target_tenant_directory,
    );

    if creation_result.is_err() {
        error!("Failed to create directory structure for tenant {tenant_id}, cleaning tmp data");
        if let Err(e) = fs::remove_dir_all(&temporary_tenant_dir) {
            error!("Failed to remove temporary tenant directory {temporary_tenant_dir:?}: {e}")
        } else if let Err(e) = crashsafe::fsync(&temporary_tenant_dir) {
            error!(
                "Failed to fsync removed temporary tenant directory {temporary_tenant_dir:?}: {e}"
            )
        }
    }

    creation_result?;

    Ok(target_tenant_directory)
}

fn try_create_target_tenant_dir(
    conf: &'static PageServerConf,
    tenant_conf: TenantConfOpt,
    tenant_id: TenantId,
    mode: CreateTenantFilesMode,
    temporary_tenant_dir: &Path,
    target_tenant_directory: &Path,
) -> Result<(), anyhow::Error> {
    match mode {
        CreateTenantFilesMode::Create => {} // needs no attach marker, writing tenant conf + atomic rename of dir is good enough
        CreateTenantFilesMode::Attach => {
            let attach_marker_path = temporary_tenant_dir.join(TENANT_ATTACHING_MARKER_FILENAME);
            let file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&attach_marker_path)
                .with_context(|| {
                    format!("could not create attach marker file {attach_marker_path:?}")
                })?;
            file.sync_all().with_context(|| {
                format!("could not sync attach marker file: {attach_marker_path:?}")
            })?;
            // fsync of the directory in which the file resides comes later in this function
        }
    }

    let temporary_tenant_timelines_dir = rebase_directory(
        &conf.timelines_path(&tenant_id),
        target_tenant_directory,
        temporary_tenant_dir,
    )
    .with_context(|| format!("resolve tenant {tenant_id} temporary timelines dir"))?;
    let temporary_tenant_config_path = rebase_directory(
        &conf.tenant_config_path(tenant_id),
        target_tenant_directory,
        temporary_tenant_dir,
    )
    .with_context(|| format!("resolve tenant {tenant_id} temporary config path"))?;

    Tenant::persist_tenant_config(&tenant_id, &temporary_tenant_config_path, tenant_conf, true)?;

    crashsafe::create_dir(&temporary_tenant_timelines_dir).with_context(|| {
        format!(
            "create tenant {} temporary timelines directory {}",
            tenant_id,
            temporary_tenant_timelines_dir.display()
        )
    })?;
    fail::fail_point!("tenant-creation-before-tmp-rename", |_| {
        anyhow::bail!("failpoint tenant-creation-before-tmp-rename");
    });

    // Make sure the current tenant directory entries are durable before renaming.
    // Without this, a crash may reorder any of the directory entry creations above.
    crashsafe::fsync(temporary_tenant_dir)
        .with_context(|| format!("sync temporary tenant directory {temporary_tenant_dir:?}"))?;

    fs::rename(temporary_tenant_dir, target_tenant_directory).with_context(|| {
        format!(
            "move tenant {} temporary directory {} into the permanent one {}",
            tenant_id,
            temporary_tenant_dir.display(),
            target_tenant_directory.display()
        )
    })?;
    let target_dir_parent = target_tenant_directory.parent().with_context(|| {
        format!(
            "get tenant {} dir parent for {}",
            tenant_id,
            target_tenant_directory.display()
        )
    })?;
    crashsafe::fsync(target_dir_parent).with_context(|| {
        format!(
            "fsync renamed directory's parent {} for tenant {}",
            target_dir_parent.display(),
            tenant_id,
        )
    })?;

    Ok(())
}

fn rebase_directory(original_path: &Path, base: &Path, new_base: &Path) -> anyhow::Result<PathBuf> {
    let relative_path = original_path.strip_prefix(base).with_context(|| {
        format!(
            "Failed to strip base prefix '{}' off path '{}'",
            base.display(),
            original_path.display()
        )
    })?;
    Ok(new_base.join(relative_path))
}

/// Create the cluster temporarily in 'initdbpath' directory inside the repository
/// to get bootstrap data for timeline initialization.
fn run_initdb(
    conf: &'static PageServerConf,
    initdb_target_dir: &Path,
    pg_version: u32,
) -> anyhow::Result<()> {
    let initdb_bin_path = conf.pg_bin_dir(pg_version)?.join("initdb");
    let initdb_lib_dir = conf.pg_lib_dir(pg_version)?;
    info!(
        "running {} in {}, libdir: {}",
        initdb_bin_path.display(),
        initdb_target_dir.display(),
        initdb_lib_dir.display(),
    );

    let initdb_output = Command::new(&initdb_bin_path)
        .args(["-D", &initdb_target_dir.to_string_lossy()])
        .args(["-U", &conf.superuser])
        .args(["-E", "utf8"])
        .arg("--no-instructions")
        // This is only used for a temporary installation that is deleted shortly after,
        // so no need to fsync it
        .arg("--no-sync")
        .env_clear()
        .env("LD_LIBRARY_PATH", &initdb_lib_dir)
        .env("DYLD_LIBRARY_PATH", &initdb_lib_dir)
        .stdout(Stdio::null())
        .output()
        .with_context(|| {
            format!(
                "failed to execute {} at target dir {}",
                initdb_bin_path.display(),
                initdb_target_dir.display()
            )
        })?;
    if !initdb_output.status.success() {
        bail!(
            "initdb failed: '{}'",
            String::from_utf8_lossy(&initdb_output.stderr)
        );
    }

    Ok(())
}

impl Drop for Tenant {
    fn drop(&mut self) {
        remove_tenant_metrics(&self.tenant_id);
    }
}
/// Dump contents of a layer file to stdout.
pub fn dump_layerfile_from_path(
    path: &Path,
    verbose: bool,
    ctx: &RequestContext,
) -> anyhow::Result<()> {
    use std::os::unix::fs::FileExt;

    // All layer files start with a two-byte "magic" value, to identify the kind of
    // file.
    let file = File::open(path)?;
    let mut header_buf = [0u8; 2];
    file.read_exact_at(&mut header_buf, 0)?;

    match u16::from_be_bytes(header_buf) {
        crate::IMAGE_FILE_MAGIC => ImageLayer::new_for_path(path, file)?.dump(verbose, ctx)?,
        crate::DELTA_FILE_MAGIC => DeltaLayer::new_for_path(path, file)?.dump(verbose, ctx)?,
        magic => bail!("unrecognized magic identifier: {:?}", magic),
    }

    Ok(())
}

#[cfg(test)]
pub mod harness {
    use bytes::{Bytes, BytesMut};
    use once_cell::sync::Lazy;
    use once_cell::sync::OnceCell;
    use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
    use std::{fs, path::PathBuf};
    use utils::logging;
    use utils::lsn::Lsn;

    use crate::{
        config::PageServerConf,
        repository::Key,
        tenant::Tenant,
        walrecord::NeonWalRecord,
        walredo::{WalRedoError, WalRedoManager},
    };

    use super::*;
    use crate::tenant::config::{TenantConf, TenantConfOpt};
    use hex_literal::hex;
    use utils::id::{TenantId, TimelineId};

    pub const TIMELINE_ID: TimelineId =
        TimelineId::from_array(hex!("11223344556677881122334455667788"));
    pub const NEW_TIMELINE_ID: TimelineId =
        TimelineId::from_array(hex!("AA223344556677881122334455667788"));

    /// Convenience function to create a page image with given string as the only content
    #[allow(non_snake_case)]
    pub fn TEST_IMG(s: &str) -> Bytes {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(s.as_bytes());
        buf.resize(64, 0);

        buf.freeze()
    }

    static LOCK: Lazy<RwLock<()>> = Lazy::new(|| RwLock::new(()));

    impl From<TenantConf> for TenantConfOpt {
        fn from(tenant_conf: TenantConf) -> Self {
            Self {
                checkpoint_distance: Some(tenant_conf.checkpoint_distance),
                checkpoint_timeout: Some(tenant_conf.checkpoint_timeout),
                compaction_target_size: Some(tenant_conf.compaction_target_size),
                compaction_period: Some(tenant_conf.compaction_period),
                compaction_threshold: Some(tenant_conf.compaction_threshold),
                gc_horizon: Some(tenant_conf.gc_horizon),
                gc_period: Some(tenant_conf.gc_period),
                image_creation_threshold: Some(tenant_conf.image_creation_threshold),
                pitr_interval: Some(tenant_conf.pitr_interval),
                walreceiver_connect_timeout: Some(tenant_conf.walreceiver_connect_timeout),
                lagging_wal_timeout: Some(tenant_conf.lagging_wal_timeout),
                max_lsn_wal_lag: Some(tenant_conf.max_lsn_wal_lag),
                trace_read_requests: Some(tenant_conf.trace_read_requests),
                eviction_policy: Some(tenant_conf.eviction_policy),
                min_resident_size_override: tenant_conf.min_resident_size_override,
                evictions_low_residence_duration_metric_threshold: Some(
                    tenant_conf.evictions_low_residence_duration_metric_threshold,
                ),
            }
        }
    }

    pub struct TenantHarness<'a> {
        pub conf: &'static PageServerConf,
        pub tenant_conf: TenantConf,
        pub tenant_id: TenantId,

        pub lock_guard: (
            Option<RwLockReadGuard<'a, ()>>,
            Option<RwLockWriteGuard<'a, ()>>,
        ),
    }

    static LOG_HANDLE: OnceCell<()> = OnceCell::new();

    impl<'a> TenantHarness<'a> {
        pub fn create(test_name: &'static str) -> anyhow::Result<Self> {
            Self::create_internal(test_name, false)
        }
        pub fn create_exclusive(test_name: &'static str) -> anyhow::Result<Self> {
            Self::create_internal(test_name, true)
        }
        fn create_internal(test_name: &'static str, exclusive: bool) -> anyhow::Result<Self> {
            let lock_guard = if exclusive {
                (None, Some(LOCK.write().unwrap()))
            } else {
                (Some(LOCK.read().unwrap()), None)
            };

            LOG_HANDLE.get_or_init(|| {
                logging::init(
                    logging::LogFormat::Test,
                    // enable it in case in case the tests exercise code paths that use
                    // debug_assert_current_span_has_tenant_and_timeline_id
                    logging::TracingErrorLayerEnablement::EnableWithRustLogFilter,
                )
                .expect("Failed to init test logging")
            });

            let repo_dir = PageServerConf::test_repo_dir(test_name);
            let _ = fs::remove_dir_all(&repo_dir);
            fs::create_dir_all(&repo_dir)?;

            let conf = PageServerConf::dummy_conf(repo_dir);
            // Make a static copy of the config. This can never be free'd, but that's
            // OK in a test.
            let conf: &'static PageServerConf = Box::leak(Box::new(conf));

            // Disable automatic GC and compaction to make the unit tests more deterministic.
            // The tests perform them manually if needed.
            let tenant_conf = TenantConf {
                gc_period: Duration::ZERO,
                compaction_period: Duration::ZERO,
                ..TenantConf::default()
            };

            let tenant_id = TenantId::generate();
            fs::create_dir_all(conf.tenant_path(&tenant_id))?;
            fs::create_dir_all(conf.timelines_path(&tenant_id))?;

            Ok(Self {
                conf,
                tenant_conf,
                tenant_id,
                lock_guard,
            })
        }

        pub async fn load(&self) -> (Arc<Tenant>, RequestContext) {
            let ctx = RequestContext::new(TaskKind::UnitTest, DownloadBehavior::Error);
            (
                self.try_load(&ctx)
                    .await
                    .expect("failed to load test tenant"),
                ctx,
            )
        }

        pub async fn try_load(&self, ctx: &RequestContext) -> anyhow::Result<Arc<Tenant>> {
            let walredo_mgr = Arc::new(TestRedoManager);

            let tenant = Arc::new(Tenant::new(
                TenantState::Loading,
                self.conf,
                TenantConfOpt::from(self.tenant_conf),
                walredo_mgr,
                self.tenant_id,
                None,
            ));
            // populate tenant with locally available timelines
            let mut timelines_to_load = HashMap::new();
            for timeline_dir_entry in fs::read_dir(self.conf.timelines_path(&self.tenant_id))
                .expect("should be able to read timelines dir")
            {
                let timeline_dir_entry = timeline_dir_entry?;
                let timeline_id: TimelineId = timeline_dir_entry
                    .path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .parse()?;

                let timeline_metadata = load_metadata(self.conf, timeline_id, self.tenant_id)?;
                timelines_to_load.insert(timeline_id, timeline_metadata);
            }
            tenant
                .load(TimelineLoadCause::Test, ctx)
                .instrument(info_span!("try_load", tenant_id=%self.tenant_id))
                .await?;
            tenant.state.send_replace(TenantState::Active);
            for timeline in tenant.timelines.lock().unwrap().values() {
                timeline.set_state(TimelineState::Active);
            }
            Ok(tenant)
        }

        pub fn timeline_path(&self, timeline_id: &TimelineId) -> PathBuf {
            self.conf.timeline_path(timeline_id, &self.tenant_id)
        }
    }

    // Mock WAL redo manager that doesn't do much
    pub struct TestRedoManager;

    impl WalRedoManager for TestRedoManager {
        fn request_redo(
            &self,
            key: Key,
            lsn: Lsn,
            base_img: Option<(Lsn, Bytes)>,
            records: Vec<(Lsn, NeonWalRecord)>,
            _pg_version: u32,
        ) -> Result<Bytes, WalRedoError> {
            let s = format!(
                "redo for {} to get to {}, with {} and {} records",
                key,
                lsn,
                if base_img.is_some() {
                    "base image"
                } else {
                    "no base image"
                },
                records.len()
            );
            println!("{s}");

            Ok(TEST_IMG(&s))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyspace::KeySpaceAccum;
    use crate::repository::{Key, Value};
    use crate::tenant::harness::*;
    use crate::DEFAULT_PG_VERSION;
    use crate::METADATA_FILE_NAME;
    use bytes::BytesMut;
    use hex_literal::hex;
    use once_cell::sync::Lazy;
    use rand::{thread_rng, Rng};

    static TEST_KEY: Lazy<Key> =
        Lazy::new(|| Key::from_slice(&hex!("112222222233333333444444445500000001")));

    #[tokio::test]
    async fn test_basic() -> anyhow::Result<()> {
        let (tenant, ctx) = TenantHarness::create("test_basic")?.load().await;
        let tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;

        let writer = tline.writer();
        writer.put(*TEST_KEY, Lsn(0x10), &Value::Image(TEST_IMG("foo at 0x10")))?;
        writer.finish_write(Lsn(0x10));
        drop(writer);

        let writer = tline.writer();
        writer.put(*TEST_KEY, Lsn(0x20), &Value::Image(TEST_IMG("foo at 0x20")))?;
        writer.finish_write(Lsn(0x20));
        drop(writer);

        assert_eq!(
            tline.get(*TEST_KEY, Lsn(0x10), &ctx).await?,
            TEST_IMG("foo at 0x10")
        );
        assert_eq!(
            tline.get(*TEST_KEY, Lsn(0x1f), &ctx).await?,
            TEST_IMG("foo at 0x10")
        );
        assert_eq!(
            tline.get(*TEST_KEY, Lsn(0x20), &ctx).await?,
            TEST_IMG("foo at 0x20")
        );

        Ok(())
    }

    #[tokio::test]
    async fn no_duplicate_timelines() -> anyhow::Result<()> {
        let (tenant, ctx) = TenantHarness::create("no_duplicate_timelines")?
            .load()
            .await;
        let _ = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;

        match tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await
        {
            Ok(_) => panic!("duplicate timeline creation should fail"),
            Err(e) => assert_eq!(
                e.to_string(),
                format!(
                    "timeline {} already exists (\"timeline directory\")",
                    TIMELINE_ID
                )
            ),
        }

        Ok(())
    }

    /// Convenience function to create a page image with given string as the only content
    pub fn test_value(s: &str) -> Value {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(s.as_bytes());
        Value::Image(buf.freeze())
    }

    ///
    /// Test branch creation
    ///
    #[tokio::test]
    async fn test_branch() -> anyhow::Result<()> {
        use std::str::from_utf8;

        let (tenant, ctx) = TenantHarness::create("test_branch")?.load().await;
        let tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;
        let writer = tline.writer();

        #[allow(non_snake_case)]
        let TEST_KEY_A: Key = Key::from_hex("112222222233333333444444445500000001").unwrap();
        #[allow(non_snake_case)]
        let TEST_KEY_B: Key = Key::from_hex("112222222233333333444444445500000002").unwrap();

        // Insert a value on the timeline
        writer.put(TEST_KEY_A, Lsn(0x20), &test_value("foo at 0x20"))?;
        writer.put(TEST_KEY_B, Lsn(0x20), &test_value("foobar at 0x20"))?;
        writer.finish_write(Lsn(0x20));

        writer.put(TEST_KEY_A, Lsn(0x30), &test_value("foo at 0x30"))?;
        writer.finish_write(Lsn(0x30));
        writer.put(TEST_KEY_A, Lsn(0x40), &test_value("foo at 0x40"))?;
        writer.finish_write(Lsn(0x40));

        //assert_current_logical_size(&tline, Lsn(0x40));

        // Branch the history, modify relation differently on the new timeline
        tenant
            .branch_timeline_test(&tline, NEW_TIMELINE_ID, Some(Lsn(0x30)), &ctx)
            .await?;
        let newtline = tenant
            .get_timeline(NEW_TIMELINE_ID, true)
            .expect("Should have a local timeline");
        let new_writer = newtline.writer();
        new_writer.put(TEST_KEY_A, Lsn(0x40), &test_value("bar at 0x40"))?;
        new_writer.finish_write(Lsn(0x40));

        // Check page contents on both branches
        assert_eq!(
            from_utf8(&tline.get(TEST_KEY_A, Lsn(0x40), &ctx).await?)?,
            "foo at 0x40"
        );
        assert_eq!(
            from_utf8(&newtline.get(TEST_KEY_A, Lsn(0x40), &ctx).await?)?,
            "bar at 0x40"
        );
        assert_eq!(
            from_utf8(&newtline.get(TEST_KEY_B, Lsn(0x40), &ctx).await?)?,
            "foobar at 0x20"
        );

        //assert_current_logical_size(&tline, Lsn(0x40));

        Ok(())
    }

    async fn make_some_layers(tline: &Timeline, start_lsn: Lsn) -> anyhow::Result<()> {
        let mut lsn = start_lsn;
        #[allow(non_snake_case)]
        {
            let writer = tline.writer();
            // Create a relation on the timeline
            writer.put(
                *TEST_KEY,
                lsn,
                &Value::Image(TEST_IMG(&format!("foo at {}", lsn))),
            )?;
            writer.finish_write(lsn);
            lsn += 0x10;
            writer.put(
                *TEST_KEY,
                lsn,
                &Value::Image(TEST_IMG(&format!("foo at {}", lsn))),
            )?;
            writer.finish_write(lsn);
            lsn += 0x10;
        }
        tline.freeze_and_flush().await?;
        {
            let writer = tline.writer();
            writer.put(
                *TEST_KEY,
                lsn,
                &Value::Image(TEST_IMG(&format!("foo at {}", lsn))),
            )?;
            writer.finish_write(lsn);
            lsn += 0x10;
            writer.put(
                *TEST_KEY,
                lsn,
                &Value::Image(TEST_IMG(&format!("foo at {}", lsn))),
            )?;
            writer.finish_write(lsn);
        }
        tline.freeze_and_flush().await
    }

    #[tokio::test]
    async fn test_prohibit_branch_creation_on_garbage_collected_data() -> anyhow::Result<()> {
        let (tenant, ctx) =
            TenantHarness::create("test_prohibit_branch_creation_on_garbage_collected_data")?
                .load()
                .await;
        let tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;
        make_some_layers(tline.as_ref(), Lsn(0x20)).await?;

        // this removes layers before lsn 40 (50 minus 10), so there are two remaining layers, image and delta for 31-50
        // FIXME: this doesn't actually remove any layer currently, given how the flushing
        // and compaction works. But it does set the 'cutoff' point so that the cross check
        // below should fail.
        tenant
            .gc_iteration(Some(TIMELINE_ID), 0x10, Duration::ZERO, &ctx)
            .await?;

        // try to branch at lsn 25, should fail because we already garbage collected the data
        match tenant
            .branch_timeline_test(&tline, NEW_TIMELINE_ID, Some(Lsn(0x25)), &ctx)
            .await
        {
            Ok(_) => panic!("branching should have failed"),
            Err(err) => {
                println!("err: {:?}", err);
                assert!(format!("{err:?}").contains("invalid branch start lsn"));
                assert!(format!("{err:?}").contains("is earlier than latest GC horizon"));
                assert!(format!("{err:?}")
                    .contains("we might've already garbage collected needed data"));
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_prohibit_branch_creation_on_pre_initdb_lsn() -> anyhow::Result<()> {
        let (tenant, ctx) =
            TenantHarness::create("test_prohibit_branch_creation_on_pre_initdb_lsn")?
                .load()
                .await;

        let tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0x50), DEFAULT_PG_VERSION, &ctx)
            .await?;
        // try to branch at lsn 0x25, should fail because initdb lsn is 0x50
        match tenant
            .branch_timeline_test(&tline, NEW_TIMELINE_ID, Some(Lsn(0x25)), &ctx)
            .await
        {
            Ok(_) => panic!("branching should have failed"),
            Err(err) => {
                println!("err: {:?}", err);
                assert!(format!("{err:?}").contains("invalid branch start lsn"));
                assert!(format!("{err:?}").contains("is earlier than latest GC horizon"));
            }
        }

        Ok(())
    }

    /*
    // FIXME: This currently fails to error out. Calling GC doesn't currently
    // remove the old value, we'd need to work a little harder
    #[tokio::test]
    async fn test_prohibit_get_for_garbage_collected_data() -> anyhow::Result<()> {
        let repo =
            RepoHarness::create("test_prohibit_get_for_garbage_collected_data")?
            .load();

        let tline = repo.create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?;
        make_some_layers(tline.as_ref(), Lsn(0x20)).await?;

        repo.gc_iteration(Some(TIMELINE_ID), 0x10, Duration::ZERO)?;
        let latest_gc_cutoff_lsn = tline.get_latest_gc_cutoff_lsn();
        assert!(*latest_gc_cutoff_lsn > Lsn(0x25));
        match tline.get(*TEST_KEY, Lsn(0x25)) {
            Ok(_) => panic!("request for page should have failed"),
            Err(err) => assert!(err.to_string().contains("not found at")),
        }
        Ok(())
    }
     */

    #[tokio::test]
    async fn test_get_branchpoints_from_an_inactive_timeline() -> anyhow::Result<()> {
        let (tenant, ctx) =
            TenantHarness::create("test_get_branchpoints_from_an_inactive_timeline")?
                .load()
                .await;
        let tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;
        make_some_layers(tline.as_ref(), Lsn(0x20)).await?;

        tenant
            .branch_timeline_test(&tline, NEW_TIMELINE_ID, Some(Lsn(0x40)), &ctx)
            .await?;
        let newtline = tenant
            .get_timeline(NEW_TIMELINE_ID, true)
            .expect("Should have a local timeline");

        make_some_layers(newtline.as_ref(), Lsn(0x60)).await?;

        tline.set_state(TimelineState::Broken);

        tenant
            .gc_iteration(Some(TIMELINE_ID), 0x10, Duration::ZERO, &ctx)
            .await?;

        // The branchpoints should contain all timelines, even ones marked
        // as Broken.
        {
            let branchpoints = &tline.gc_info.read().unwrap().retain_lsns;
            assert_eq!(branchpoints.len(), 1);
            assert_eq!(branchpoints[0], Lsn(0x40));
        }

        // You can read the key from the child branch even though the parent is
        // Broken, as long as you don't need to access data from the parent.
        assert_eq!(
            newtline.get(*TEST_KEY, Lsn(0x70), &ctx).await?,
            TEST_IMG(&format!("foo at {}", Lsn(0x70)))
        );

        // This needs to traverse to the parent, and fails.
        let err = newtline.get(*TEST_KEY, Lsn(0x50), &ctx).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("will not become active. Current state: Broken"));

        Ok(())
    }

    #[tokio::test]
    async fn test_retain_data_in_parent_which_is_needed_for_child() -> anyhow::Result<()> {
        let (tenant, ctx) =
            TenantHarness::create("test_retain_data_in_parent_which_is_needed_for_child")?
                .load()
                .await;
        let tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;
        make_some_layers(tline.as_ref(), Lsn(0x20)).await?;

        tenant
            .branch_timeline_test(&tline, NEW_TIMELINE_ID, Some(Lsn(0x40)), &ctx)
            .await?;
        let newtline = tenant
            .get_timeline(NEW_TIMELINE_ID, true)
            .expect("Should have a local timeline");
        // this removes layers before lsn 40 (50 minus 10), so there are two remaining layers, image and delta for 31-50
        tenant
            .gc_iteration(Some(TIMELINE_ID), 0x10, Duration::ZERO, &ctx)
            .await?;
        assert!(newtline.get(*TEST_KEY, Lsn(0x25), &ctx).await.is_ok());

        Ok(())
    }
    #[tokio::test]
    async fn test_parent_keeps_data_forever_after_branching() -> anyhow::Result<()> {
        let (tenant, ctx) =
            TenantHarness::create("test_parent_keeps_data_forever_after_branching")?
                .load()
                .await;
        let tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;
        make_some_layers(tline.as_ref(), Lsn(0x20)).await?;

        tenant
            .branch_timeline_test(&tline, NEW_TIMELINE_ID, Some(Lsn(0x40)), &ctx)
            .await?;
        let newtline = tenant
            .get_timeline(NEW_TIMELINE_ID, true)
            .expect("Should have a local timeline");

        make_some_layers(newtline.as_ref(), Lsn(0x60)).await?;

        // run gc on parent
        tenant
            .gc_iteration(Some(TIMELINE_ID), 0x10, Duration::ZERO, &ctx)
            .await?;

        // Check that the data is still accessible on the branch.
        assert_eq!(
            newtline.get(*TEST_KEY, Lsn(0x50), &ctx).await?,
            TEST_IMG(&format!("foo at {}", Lsn(0x40)))
        );

        Ok(())
    }

    #[tokio::test]
    async fn timeline_load() -> anyhow::Result<()> {
        const TEST_NAME: &str = "timeline_load";
        let harness = TenantHarness::create(TEST_NAME)?;
        {
            let (tenant, ctx) = harness.load().await;
            let tline = tenant
                .create_test_timeline(TIMELINE_ID, Lsn(0x8000), DEFAULT_PG_VERSION, &ctx)
                .await?;
            make_some_layers(tline.as_ref(), Lsn(0x8000)).await?;
        }

        let (tenant, _ctx) = harness.load().await;
        tenant
            .get_timeline(TIMELINE_ID, true)
            .expect("cannot load timeline");

        Ok(())
    }

    #[tokio::test]
    async fn timeline_load_with_ancestor() -> anyhow::Result<()> {
        const TEST_NAME: &str = "timeline_load_with_ancestor";
        let harness = TenantHarness::create(TEST_NAME)?;
        // create two timelines
        {
            let (tenant, ctx) = harness.load().await;
            let tline = tenant
                .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
                .await?;

            make_some_layers(tline.as_ref(), Lsn(0x20)).await?;

            let child_tline = tenant
                .branch_timeline_test(&tline, NEW_TIMELINE_ID, Some(Lsn(0x40)), &ctx)
                .await?;
            child_tline.set_state(TimelineState::Active);

            let newtline = tenant
                .get_timeline(NEW_TIMELINE_ID, true)
                .expect("Should have a local timeline");

            make_some_layers(newtline.as_ref(), Lsn(0x60)).await?;
        }

        // check that both of them are initially unloaded
        let (tenant, _ctx) = harness.load().await;

        // check that both, child and ancestor are loaded
        let _child_tline = tenant
            .get_timeline(NEW_TIMELINE_ID, true)
            .expect("cannot get child timeline loaded");

        let _ancestor_tline = tenant
            .get_timeline(TIMELINE_ID, true)
            .expect("cannot get ancestor timeline loaded");

        Ok(())
    }

    #[tokio::test]
    async fn corrupt_metadata() -> anyhow::Result<()> {
        const TEST_NAME: &str = "corrupt_metadata";
        let harness = TenantHarness::create(TEST_NAME)?;
        let (tenant, ctx) = harness.load().await;

        let tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;
        drop(tline);
        drop(tenant);

        let metadata_path = harness.timeline_path(&TIMELINE_ID).join(METADATA_FILE_NAME);

        assert!(metadata_path.is_file());

        let mut metadata_bytes = std::fs::read(&metadata_path)?;
        assert_eq!(metadata_bytes.len(), 512);
        metadata_bytes[8] ^= 1;
        std::fs::write(metadata_path, metadata_bytes)?;

        let err = harness.try_load(&ctx).await.err().expect("should fail");
        assert!(err
            .to_string()
            .starts_with("Failed to parse metadata bytes from path"));

        let mut found_error_message = false;
        let mut err_source = err.source();
        while let Some(source) = err_source {
            if source.to_string() == "metadata checksum mismatch" {
                found_error_message = true;
                break;
            }
            err_source = source.source();
        }
        assert!(
            found_error_message,
            "didn't find the corrupted metadata error"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_images() -> anyhow::Result<()> {
        let (tenant, ctx) = TenantHarness::create("test_images")?.load().await;
        let tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;

        let writer = tline.writer();
        writer.put(*TEST_KEY, Lsn(0x10), &Value::Image(TEST_IMG("foo at 0x10")))?;
        writer.finish_write(Lsn(0x10));
        drop(writer);

        tline.freeze_and_flush().await?;
        tline.compact(&ctx).await?;

        let writer = tline.writer();
        writer.put(*TEST_KEY, Lsn(0x20), &Value::Image(TEST_IMG("foo at 0x20")))?;
        writer.finish_write(Lsn(0x20));
        drop(writer);

        tline.freeze_and_flush().await?;
        tline.compact(&ctx).await?;

        let writer = tline.writer();
        writer.put(*TEST_KEY, Lsn(0x30), &Value::Image(TEST_IMG("foo at 0x30")))?;
        writer.finish_write(Lsn(0x30));
        drop(writer);

        tline.freeze_and_flush().await?;
        tline.compact(&ctx).await?;

        let writer = tline.writer();
        writer.put(*TEST_KEY, Lsn(0x40), &Value::Image(TEST_IMG("foo at 0x40")))?;
        writer.finish_write(Lsn(0x40));
        drop(writer);

        tline.freeze_and_flush().await?;
        tline.compact(&ctx).await?;

        assert_eq!(
            tline.get(*TEST_KEY, Lsn(0x10), &ctx).await?,
            TEST_IMG("foo at 0x10")
        );
        assert_eq!(
            tline.get(*TEST_KEY, Lsn(0x1f), &ctx).await?,
            TEST_IMG("foo at 0x10")
        );
        assert_eq!(
            tline.get(*TEST_KEY, Lsn(0x20), &ctx).await?,
            TEST_IMG("foo at 0x20")
        );
        assert_eq!(
            tline.get(*TEST_KEY, Lsn(0x30), &ctx).await?,
            TEST_IMG("foo at 0x30")
        );
        assert_eq!(
            tline.get(*TEST_KEY, Lsn(0x40), &ctx).await?,
            TEST_IMG("foo at 0x40")
        );

        Ok(())
    }

    //
    // Insert 1000 key-value pairs with increasing keys, flush, compact, GC.
    // Repeat 50 times.
    //
    #[tokio::test]
    async fn test_bulk_insert() -> anyhow::Result<()> {
        let (tenant, ctx) = TenantHarness::create("test_bulk_insert")?.load().await;
        let tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;

        let mut lsn = Lsn(0x10);

        let mut keyspace = KeySpaceAccum::new();

        let mut test_key = Key::from_hex("012222222233333333444444445500000000").unwrap();
        let mut blknum = 0;
        for _ in 0..50 {
            for _ in 0..10000 {
                test_key.field6 = blknum;
                let writer = tline.writer();
                writer.put(
                    test_key,
                    lsn,
                    &Value::Image(TEST_IMG(&format!("{} at {}", blknum, lsn))),
                )?;
                writer.finish_write(lsn);
                drop(writer);

                keyspace.add_key(test_key);

                lsn = Lsn(lsn.0 + 0x10);
                blknum += 1;
            }

            let cutoff = tline.get_last_record_lsn();

            tline
                .update_gc_info(Vec::new(), cutoff, Duration::ZERO, &ctx)
                .await?;
            tline.freeze_and_flush().await?;
            tline.compact(&ctx).await?;
            tline.gc().await?;
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_random_updates() -> anyhow::Result<()> {
        let (tenant, ctx) = TenantHarness::create("test_random_updates")?.load().await;
        let tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;

        const NUM_KEYS: usize = 1000;

        let mut test_key = Key::from_hex("012222222233333333444444445500000000").unwrap();

        let mut keyspace = KeySpaceAccum::new();

        // Track when each page was last modified. Used to assert that
        // a read sees the latest page version.
        let mut updated = [Lsn(0); NUM_KEYS];

        let mut lsn = Lsn(0);
        #[allow(clippy::needless_range_loop)]
        for blknum in 0..NUM_KEYS {
            lsn = Lsn(lsn.0 + 0x10);
            test_key.field6 = blknum as u32;
            let writer = tline.writer();
            writer.put(
                test_key,
                lsn,
                &Value::Image(TEST_IMG(&format!("{} at {}", blknum, lsn))),
            )?;
            writer.finish_write(lsn);
            updated[blknum] = lsn;
            drop(writer);

            keyspace.add_key(test_key);
        }

        for _ in 0..50 {
            for _ in 0..NUM_KEYS {
                lsn = Lsn(lsn.0 + 0x10);
                let blknum = thread_rng().gen_range(0..NUM_KEYS);
                test_key.field6 = blknum as u32;
                let writer = tline.writer();
                writer.put(
                    test_key,
                    lsn,
                    &Value::Image(TEST_IMG(&format!("{} at {}", blknum, lsn))),
                )?;
                writer.finish_write(lsn);
                drop(writer);
                updated[blknum] = lsn;
            }

            // Read all the blocks
            for (blknum, last_lsn) in updated.iter().enumerate() {
                test_key.field6 = blknum as u32;
                assert_eq!(
                    tline.get(test_key, lsn, &ctx).await?,
                    TEST_IMG(&format!("{} at {}", blknum, last_lsn))
                );
            }

            // Perform a cycle of flush, compact, and GC
            let cutoff = tline.get_last_record_lsn();
            tline
                .update_gc_info(Vec::new(), cutoff, Duration::ZERO, &ctx)
                .await?;
            tline.freeze_and_flush().await?;
            tline.compact(&ctx).await?;
            tline.gc().await?;
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_traverse_branches() -> anyhow::Result<()> {
        let (tenant, ctx) = TenantHarness::create("test_traverse_branches")?
            .load()
            .await;
        let mut tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;

        const NUM_KEYS: usize = 1000;

        let mut test_key = Key::from_hex("012222222233333333444444445500000000").unwrap();

        let mut keyspace = KeySpaceAccum::new();

        // Track when each page was last modified. Used to assert that
        // a read sees the latest page version.
        let mut updated = [Lsn(0); NUM_KEYS];

        let mut lsn = Lsn(0);
        #[allow(clippy::needless_range_loop)]
        for blknum in 0..NUM_KEYS {
            lsn = Lsn(lsn.0 + 0x10);
            test_key.field6 = blknum as u32;
            let writer = tline.writer();
            writer.put(
                test_key,
                lsn,
                &Value::Image(TEST_IMG(&format!("{} at {}", blknum, lsn))),
            )?;
            writer.finish_write(lsn);
            updated[blknum] = lsn;
            drop(writer);

            keyspace.add_key(test_key);
        }

        for _ in 0..50 {
            let new_tline_id = TimelineId::generate();
            tenant
                .branch_timeline_test(&tline, new_tline_id, Some(lsn), &ctx)
                .await?;
            tline = tenant
                .get_timeline(new_tline_id, true)
                .expect("Should have the branched timeline");

            for _ in 0..NUM_KEYS {
                lsn = Lsn(lsn.0 + 0x10);
                let blknum = thread_rng().gen_range(0..NUM_KEYS);
                test_key.field6 = blknum as u32;
                let writer = tline.writer();
                writer.put(
                    test_key,
                    lsn,
                    &Value::Image(TEST_IMG(&format!("{} at {}", blknum, lsn))),
                )?;
                println!("updating {} at {}", blknum, lsn);
                writer.finish_write(lsn);
                drop(writer);
                updated[blknum] = lsn;
            }

            // Read all the blocks
            for (blknum, last_lsn) in updated.iter().enumerate() {
                test_key.field6 = blknum as u32;
                assert_eq!(
                    tline.get(test_key, lsn, &ctx).await?,
                    TEST_IMG(&format!("{} at {}", blknum, last_lsn))
                );
            }

            // Perform a cycle of flush, compact, and GC
            let cutoff = tline.get_last_record_lsn();
            tline
                .update_gc_info(Vec::new(), cutoff, Duration::ZERO, &ctx)
                .await?;
            tline.freeze_and_flush().await?;
            tline.compact(&ctx).await?;
            tline.gc().await?;
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_traverse_ancestors() -> anyhow::Result<()> {
        let (tenant, ctx) = TenantHarness::create("test_traverse_ancestors")?
            .load()
            .await;
        let mut tline = tenant
            .create_test_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION, &ctx)
            .await?;

        const NUM_KEYS: usize = 100;
        const NUM_TLINES: usize = 50;

        let mut test_key = Key::from_hex("012222222233333333444444445500000000").unwrap();
        // Track page mutation lsns across different timelines.
        let mut updated = [[Lsn(0); NUM_KEYS]; NUM_TLINES];

        let mut lsn = Lsn(0);

        #[allow(clippy::needless_range_loop)]
        for idx in 0..NUM_TLINES {
            let new_tline_id = TimelineId::generate();
            tenant
                .branch_timeline_test(&tline, new_tline_id, Some(lsn), &ctx)
                .await?;
            tline = tenant
                .get_timeline(new_tline_id, true)
                .expect("Should have the branched timeline");

            for _ in 0..NUM_KEYS {
                lsn = Lsn(lsn.0 + 0x10);
                let blknum = thread_rng().gen_range(0..NUM_KEYS);
                test_key.field6 = blknum as u32;
                let writer = tline.writer();
                writer.put(
                    test_key,
                    lsn,
                    &Value::Image(TEST_IMG(&format!("{} {} at {}", idx, blknum, lsn))),
                )?;
                println!("updating [{}][{}] at {}", idx, blknum, lsn);
                writer.finish_write(lsn);
                drop(writer);
                updated[idx][blknum] = lsn;
            }
        }

        // Read pages from leaf timeline across all ancestors.
        for (idx, lsns) in updated.iter().enumerate() {
            for (blknum, lsn) in lsns.iter().enumerate() {
                // Skip empty mutations.
                if lsn.0 == 0 {
                    continue;
                }
                println!("checking [{idx}][{blknum}] at {lsn}");
                test_key.field6 = blknum as u32;
                assert_eq!(
                    tline.get(test_key, *lsn, &ctx).await?,
                    TEST_IMG(&format!("{idx} {blknum} at {lsn}"))
                );
            }
        }
        Ok(())
    }
}

#[cfg(not(debug_assertions))]
#[inline]
pub(crate) fn debug_assert_current_span_has_tenant_id() {}

#[cfg(debug_assertions)]
pub static TENANT_ID_EXTRACTOR: once_cell::sync::Lazy<
    utils::tracing_span_assert::MultiNameExtractor<2>,
> = once_cell::sync::Lazy::new(|| {
    utils::tracing_span_assert::MultiNameExtractor::new("TenantId", ["tenant_id", "tenant"])
});

#[cfg(debug_assertions)]
#[inline]
pub(crate) fn debug_assert_current_span_has_tenant_id() {
    use utils::tracing_span_assert;

    match tracing_span_assert::check_fields_present([&*TENANT_ID_EXTRACTOR]) {
        Ok(()) => (),
        Err(missing) => panic!(
            "missing extractors: {:?}",
            missing.into_iter().map(|e| e.name()).collect::<Vec<_>>()
        ),
    }
}
