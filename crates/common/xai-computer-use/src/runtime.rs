use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

/// Immutable observation retained for later state-scoped queries.
#[derive(Debug, Clone)]
pub struct StoredState<T> {
    pub state_id: String,
    pub resource_key: String,
    pub epoch: u64,
    pub value: T,
}

/// Bounded insertion-ordered state store.
pub struct StateStore<T> {
    limit: usize,
    records: RwLock<HashMap<String, Arc<StoredState<T>>>>,
    order: Mutex<Vec<String>>,
}

impl<T> fmt::Debug for StateStore<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StateStore")
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl<T> StateStore<T> {
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            records: RwLock::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
        }
    }

    pub async fn create(&self, resource_key: String, epoch: u64, value: T) -> Arc<StoredState<T>> {
        let record = Arc::new(StoredState {
            state_id: Uuid::now_v7().to_string(),
            resource_key,
            epoch,
            value,
        });
        self.insert(Arc::clone(&record)).await;
        record
    }

    pub async fn insert(&self, record: Arc<StoredState<T>>) {
        let mut records = self.records.write().await;
        let mut order = self.order.lock().await;
        records.remove(&record.state_id);
        order.retain(|id| id != &record.state_id);
        order.push(record.state_id.clone());
        records.insert(record.state_id.clone(), record);
        while order.len() > self.limit {
            let oldest = order.remove(0);
            records.remove(&oldest);
        }
    }

    pub async fn get(&self, state_id: &str) -> Option<Arc<StoredState<T>>> {
        self.records.read().await.get(state_id).cloned()
    }

    pub async fn len(&self) -> usize {
        self.records.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.records.read().await.is_empty()
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error(
    "state is stale for {resource_key}: expected epoch {expected_epoch}, current epoch {actual_epoch}"
)]
pub struct StaleStateError {
    pub resource_key: String,
    pub expected_epoch: u64,
    pub actual_epoch: u64,
}

#[derive(Debug, Default)]
struct ResourceRecord {
    epoch: u64,
}

/// Serializes live operations per physical resource while allowing unrelated
/// windows to proceed concurrently.
#[derive(Debug, Default)]
pub struct ResourceScheduler {
    resources: Mutex<HashMap<String, Arc<Mutex<ResourceRecord>>>>,
}

impl ResourceScheduler {
    async fn resource(&self, key: &str) -> Arc<Mutex<ResourceRecord>> {
        let mut resources = self.resources.lock().await;
        resources
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(ResourceRecord::default())))
            .clone()
    }

    pub async fn epoch(&self, key: &str) -> u64 {
        self.resource(key).await.lock().await.epoch
    }

    pub async fn read<T, F, Fut>(&self, key: &str, work: F) -> (T, u64)
    where
        F: FnOnce(u64) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let resource = self.resource(key).await;
        let record = resource.lock().await;
        let epoch = record.epoch;
        (work(epoch).await, epoch)
    }

    pub async fn read_at<T, F, Fut>(
        &self,
        key: &str,
        expected: u64,
        work: F,
    ) -> Result<(T, u64), StaleStateError>
    where
        F: FnOnce(u64) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let resource = self.resource(key).await;
        let record = resource.lock().await;
        if record.epoch != expected {
            return Err(StaleStateError {
                resource_key: key.to_owned(),
                expected_epoch: expected,
                actual_epoch: record.epoch,
            });
        }
        let epoch = record.epoch;
        Ok((work(epoch).await, epoch))
    }

    pub async fn write<T, F, Fut>(
        &self,
        key: &str,
        expected: u64,
        work: F,
    ) -> Result<(T, u64), StaleStateError>
    where
        F: FnOnce(u64) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let resource = self.resource(key).await;
        let mut record = resource.lock().await;
        if record.epoch != expected {
            return Err(StaleStateError {
                resource_key: key.to_owned(),
                expected_epoch: expected,
                actual_epoch: record.epoch,
            });
        }
        record.epoch += 1;
        let next_epoch = record.epoch;
        Ok((work(next_epoch).await, next_epoch))
    }
}
