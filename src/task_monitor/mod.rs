use std::sync::Arc;
use std::time::Duration;

use once_cell::sync::Lazy;
use prometheus::{linear_buckets, register_gauge, register_histogram, Gauge, Histogram};
use tokio::sync::{broadcast, mpsc, Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use tracing::{info, instrument, warn};

use crate::app::App;
use crate::database::query::DatabaseQuery as _;
use crate::database::Database;
use crate::shutdown::Shutdown;

pub mod tasks;

const TREE_INIT_BACKOFF: Duration = Duration::from_secs(5);
const PROCESS_IDENTITIES_BACKOFF: Duration = Duration::from_secs(5);
const FINALIZE_IDENTITIES_BACKOFF: Duration = Duration::from_secs(5);
const QUEUE_MONITOR_BACKOFF: Duration = Duration::from_secs(5);
const MODIFY_TREE_BACKOFF: Duration = Duration::from_secs(5);
const SYNC_TREE_STATE_WITH_DB_BACKOFF: Duration = Duration::from_secs(5);

struct RunningInstance {
    handles:         Vec<JoinHandle<()>>,
    shutdown_sender: broadcast::Sender<()>,
}

static PENDING_IDENTITIES: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!("pending_identities", "Identities not submitted on-chain").unwrap()
});

static UNPROCESSED_IDENTITIES: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!(
        "unprocessed_identities",
        "Identities not processed by identity committer"
    )
    .unwrap()
});

static BATCH_SIZES: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "submitted_batch_sizes",
        "Submitted batch size",
        linear_buckets(f64::from(1), f64::from(1), 100).unwrap()
    )
    .unwrap()
});

impl RunningInstance {
    async fn shutdown(self) -> anyhow::Result<()> {
        info!("Sending a shutdown signal to the committer.");
        // Ignoring errors here, since we have two options: either the channel is full,
        // which is impossible, since this is the only use, and this method takes
        // ownership, or the channel is closed, which means the committer thread is
        // already dead.
        _ = self.shutdown_sender.send(());

        info!("Awaiting tasks to shutdown.");
        for result in futures::future::join_all(self.handles).await {
            result?;
        }

        Ok(())
    }
}

/// A worker that commits identities to the blockchain.
///
/// This uses the database to keep track of identities that need to be
/// committed. It assumes that there's only one such worker spawned at
/// a time. Spawning multiple worker threads will result in undefined behavior,
/// including data duplication.
pub struct TaskMonitor {
    /// The instance is kept behind an RwLock<Option<...>> because
    /// when shutdown is called we want to be able to gracefully
    /// await the join handles - which requires ownership of the handle and by
    /// extension the instance.
    instance: RwLock<Option<RunningInstance>>,
    shutdown: Arc<Shutdown>,
    app:      Arc<App>,
}

impl TaskMonitor {
    pub fn new(app: Arc<App>, shutdown: Arc<Shutdown>) -> Self {
        Self {
            instance: RwLock::new(None),
            shutdown,
            app,
        }
    }

    #[instrument(level = "debug", skip_all)]
    pub async fn start(&self) {
        let mut instance = self.instance.write().await;
        if instance.is_some() {
            warn!("Identity committer already running");
        }

        // We could use the second element of the tuple as `mut shutdown_receiver`,
        // but for symmetry's sake we create it for every task with `.subscribe()`
        let (shutdown_sender, _) = broadcast::channel(1);

        let (monitored_txs_sender, monitored_txs_receiver) =
            mpsc::channel(self.app.config.app.monitored_txs_capacity);

        let monitored_txs_sender = Arc::new(monitored_txs_sender);
        let monitored_txs_receiver = Arc::new(Mutex::new(monitored_txs_receiver));

        let base_next_batch_notify = Arc::new(Notify::new());
        // Immediately notify, so we can start processing if we have pending operations
        base_next_batch_notify.notify_one();

        let base_sync_tree_notify = Arc::new(Notify::new());
        // Immediately notify, so we can start processing if we have pending operations
        base_sync_tree_notify.notify_one();

        let base_tree_synced_notify = Arc::new(Notify::new());
        // Immediately notify, so we can start processing if we have pending operations
        base_tree_synced_notify.notify_one();

        let mut handles = Vec::new();

        // Initialize the Tree
        let app = self.app.clone();

        let tree_init = move || app.clone().init_tree();
        let tree_init_handle = crate::utils::spawn_monitored_with_backoff(
            tree_init,
            shutdown_sender.clone(),
            TREE_INIT_BACKOFF,
            self.shutdown.clone(),
        );

        handles.push(tree_init_handle);

        // Finalize identities
        let app = self.app.clone();
        let sync_tree_notify = base_sync_tree_notify.clone();

        let finalize_identities = move || {
            tasks::finalize_identities::finalize_roots(app.clone(), sync_tree_notify.clone())
        };
        let finalize_identities_handle = crate::utils::spawn_monitored_with_backoff(
            finalize_identities,
            shutdown_sender.clone(),
            FINALIZE_IDENTITIES_BACKOFF,
            self.shutdown.clone(),
        );
        handles.push(finalize_identities_handle);

        // Report length of the queue of identities
        let app = self.app.clone();

        let queue_monitor = move || tasks::monitor_queue::monitor_queue(app.clone());
        let queue_monitor_handle = crate::utils::spawn_monitored_with_backoff(
            queue_monitor,
            shutdown_sender.clone(),
            QUEUE_MONITOR_BACKOFF,
            self.shutdown.clone(),
        );
        handles.push(queue_monitor_handle);

        // Create batches
        let app = self.app.clone();
        let next_batch_notify = base_next_batch_notify.clone();
        let sync_tree_notify = base_sync_tree_notify.clone();
        let tree_synced_notify = base_tree_synced_notify.clone();

        let create_batches = move || {
            tasks::create_batches::create_batches(
                app.clone(),
                next_batch_notify.clone(),
                sync_tree_notify.clone(),
                tree_synced_notify.clone(),
            )
        };
        let create_batches_handle = crate::utils::spawn_monitored_with_backoff(
            create_batches,
            shutdown_sender.clone(),
            PROCESS_IDENTITIES_BACKOFF,
            self.shutdown.clone(),
        );
        handles.push(create_batches_handle);

        // Process batches
        let app = self.app.clone();
        let next_batch_notify = base_next_batch_notify.clone();

        let process_identities = move || {
            tasks::process_batches::process_batches(
                app.clone(),
                monitored_txs_sender.clone(),
                next_batch_notify.clone(),
            )
        };
        let process_identities_handle = crate::utils::spawn_monitored_with_backoff(
            process_identities,
            shutdown_sender.clone(),
            PROCESS_IDENTITIES_BACKOFF,
            self.shutdown.clone(),
        );
        handles.push(process_identities_handle);

        // Monitor transactions
        let app = self.app.clone();

        let monitor_txs =
            move || tasks::monitor_txs::monitor_txs(app.clone(), monitored_txs_receiver.clone());
        let monitor_txs_handle = crate::utils::spawn_monitored_with_backoff(
            monitor_txs,
            shutdown_sender.clone(),
            PROCESS_IDENTITIES_BACKOFF,
            self.shutdown.clone(),
        );
        handles.push(monitor_txs_handle);

        // Modify tree
        let app = self.app.clone();
        let sync_tree_notify = base_sync_tree_notify.clone();
        let tree_synced_notify = base_tree_synced_notify.clone();

        let modify_tree = move || {
            tasks::modify_tree::modify_tree(
                app.clone(),
                sync_tree_notify.clone(),
                tree_synced_notify.clone(),
            )
        };
        let modify_tree_handle = crate::utils::spawn_monitored_with_backoff(
            modify_tree,
            shutdown_sender.clone(),
            MODIFY_TREE_BACKOFF,
            self.shutdown.clone(),
        );
        handles.push(modify_tree_handle);

        // Sync tree state with DB
        let app = self.app.clone();
        let sync_tree_notify = base_sync_tree_notify.clone();
        let tree_synced_notify = base_tree_synced_notify.clone();

        let sync_tree_state_with_db = move || {
            tasks::sync_tree_state_with_db::sync_tree_state_with_db(
                app.clone(),
                sync_tree_notify.clone(),
                tree_synced_notify.clone(),
            )
        };
        let sync_tree_state_with_db_handle = crate::utils::spawn_monitored_with_backoff(
            sync_tree_state_with_db,
            shutdown_sender.clone(),
            SYNC_TREE_STATE_WITH_DB_BACKOFF,
            self.shutdown.clone(),
        );
        handles.push(sync_tree_state_with_db_handle);

        // Create the instance
        *instance = Some(RunningInstance {
            handles,
            shutdown_sender,
        });
    }

    async fn log_pending_identities_count(database: &Database) -> anyhow::Result<()> {
        let identities = database.count_pending_identities().await?;
        PENDING_IDENTITIES.set(f64::from(identities));
        Ok(())
    }

    async fn log_unprocessed_identities_count(database: &Database) -> anyhow::Result<()> {
        let identities = database.count_unprocessed_identities().await?;
        UNPROCESSED_IDENTITIES.set(f64::from(identities));
        Ok(())
    }

    async fn log_identities_queues(database: &Database) -> anyhow::Result<()> {
        TaskMonitor::log_unprocessed_identities_count(database).await?;
        TaskMonitor::log_pending_identities_count(database).await?;
        Ok(())
    }

    #[allow(clippy::cast_precision_loss)]
    fn log_batch_size(size: usize) {
        BATCH_SIZES.observe(size as f64);
    }

    /// # Errors
    ///
    /// Will return an Error if the committer thread cannot be shut down
    /// gracefully.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let mut instance = self.instance.write().await;
        if let Some(instance) = instance.take() {
            instance.shutdown().await?;
        } else {
            info!("Committer not running.");
        }
        Ok(())
    }
}
