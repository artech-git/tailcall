
use async_graphql::dataloader::{NoCache, CacheStorage, CacheFactory};
use dashmap::DashMap;
use futures_core::future::BoxFuture;
// use fxhash::{FxHashMap, FxHashSet, FxBuildHasher};
use std::borrow::Cow;
use std::sync::atomic::Ordering;
use std::{sync::atomic::AtomicBool, time::Duration};
use std::hash::BuildHasherDefault;
use std::sync::Arc;

use std::collections::{HashSet, HashMap};
use std::hash::Hash;
use std::any::{TypeId, Any};

use async_stream::stream;
use futures_core::stream::Stream;

use crate::http::{DataLoaderRequest, HttpClient, Response};

use ahash::AHashMap as FxHashMap;
use ahash::AHashSet as FxHashSet;

use flume::bounded;
//=========================================================================================

//=========================================================================================
//  trial implementation for the Dashmap for no locking .. . 

#[allow(clippy::type_complexity)]
struct ResSender<K: Send + Sync + Hash + Eq + Clone + 'static, T: Loader<K>> {
    use_cache_values: FxHashMap<K, T::Value>,
    tx: flume::Sender<Result<FxHashMap<K, T::Value>, T::Error>>,
}

struct Requests<K: Send + Sync + Hash + Eq + Clone + 'static, T: Loader<K>> {
    keys: FxHashSet<K>,
    pending: Vec<(FxHashSet<K>, ResSender<K, T>)>,
    cache_storage: Box<dyn CacheStorage<Key = K, Value = T::Value>>,
    disable_cache: bool,
}

type KeysAndSender<K, T> = (FxHashSet<K>, Vec<(FxHashSet<K>, ResSender<K, T>)>);

impl<K: Send + Sync + Hash + Eq + Clone + 'static, T: Loader<K>> Requests<K, T> {
    fn new<C: CacheFactory>(cache_factory: &C) -> Self {
        Self {
            keys: Default::default(),
            pending: Vec::new(),
            cache_storage: cache_factory.create::<K, T::Value>(),
            disable_cache: false,
        }
    }

    fn take(&mut self) -> KeysAndSender<K, T> {
        (
            std::mem::take(&mut self.keys),
            std::mem::take(&mut self.pending),
        )
    }
}

//=========================================================================================

/// Trait for batch loading.
#[async_trait::async_trait]
pub trait Loader<K: Send + Sync + Hash + Eq + Clone + 'static>: Send + Sync + 'static {
    /// type of value.
    type Value: Send + Sync + Clone + 'static;

    /// Type of error.
    type Error: Send + Clone + 'static;

    /// Load the data set specified by the `keys`.
    async fn load(&self, keys: &[K]) -> Result<FxHashMap<K, Self::Value>, Self::Error>;
}

struct DataLoaderInner<T: Loader<DataLoaderRequest>> {
    requests: Arc<DashMap<TypeId, Requests<DataLoaderRequest, T>>>,
    loader: T,
}

impl<T: Loader<DataLoaderRequest>> DataLoaderInner<T> {
   
    async fn do_load(&self, disable_cache: bool, (keys, senders): KeysAndSender<DataLoaderRequest, T>)
    where
        // K: Send + Sync + Hash + Eq + Clone + 'static,
        T: Loader<DataLoaderRequest>,
    {
        let tid = TypeId::of::<DataLoaderRequest>();
        let keys = keys.into_iter().collect::<Vec<_>>();

        // log::info!(" value type in rust: {}", tid);

        match self.loader.load(&keys).await {
            Ok(values) => { 
                // update cache
                let mut request = self.requests.clone();
                let mut tt = request
                    .get_mut(&tid)
                    .unwrap();
                let typed_requests = tt
                    // .downcast_mut::<Requests<DataLoaderRequest, T>>()
                    .value_mut();
                    // .unwrap();
                let disable_cache = typed_requests.disable_cache || disable_cache;
                if !disable_cache {
                    for (key, value) in &values {
                        typed_requests
                            .cache_storage
                            .insert(Cow::Borrowed(key), Cow::Borrowed(value));
                    }
                }

                // send response
                for (keys, sender) in senders {
                    let mut res = FxHashMap::default();
                    res.extend(sender.use_cache_values);
                    for key in &keys {
                        res.extend(values
                            .get(key)
                            .map(|value| (key.clone(), value.clone()))
                        );
                    }
                    sender.tx.send(Ok(res)).ok();
                }
            }
            Err(err) => {
                for (_, sender) in senders {
                    sender.tx.send(Err(err.clone())).ok();
                }
            }
        }
    }

}

//=========================================================================================

/// Data loader.
///
pub struct DataLoader<T: Loader<DataLoaderRequest>, C = NoCache> {
    inner: Arc<DataLoaderInner<T>>,
    cache_factory: C,
    delay: Duration,
    max_batch_size: usize,
    disable_cache: AtomicBool,
    // spawner: Box<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>,
}

impl<T: Loader<DataLoaderRequest>> DataLoader<T, NoCache> {
    /// Use `Loader` to create a [DataLoader] that does not cache records.
    pub fn new<S, R>(loader: T) -> Self   
    {
        log::info!(" loader called ");

        Self {
            inner: Arc::new(DataLoaderInner {
                requests: Arc::new(DashMap::default()),
                loader: loader,
            }),
            cache_factory: NoCache,
            delay: Duration::from_millis(1),
            max_batch_size: 1000,
            disable_cache: false.into(),
            // spawner: Box::new(move |fut| {
            //     spawner(fut);
            // }),
        }
    }
}

enum Action<K, T>
where 
    K: Send + Sync + Hash + Eq + Clone + 'static,
    T: Loader<K>  
{
    ImmediateLoad(KeysAndSender<K, T>),
    StartFetch,
    Delay,
}

impl<T: Loader<DataLoaderRequest>, C: CacheFactory> DataLoader<T, C> {
    /// Use this `DataLoader` load a data.
    pub async fn load_one(&self, key: DataLoaderRequest) 
    -> Result<Option<<T as Loader<DataLoaderRequest>>::Value>, <T as Loader<DataLoaderRequest>>::Error>
    where
        // K: Send + Sync + Hash + Eq + Clone + 'static,
        T: Loader<DataLoaderRequest>,
    {
        let mut values = self.load_many(std::iter::once(key.clone())).await?;
        Ok(values.remove(&key))
    }

    /// Use this `DataLoader` to load some data.
    pub async fn load_many<I>(&self, keys: I) 
    -> Result<FxHashMap<DataLoaderRequest, <T as Loader<DataLoaderRequest>>::Value>, <T as Loader<DataLoaderRequest>>::Error>
    where
        // K: Send + Sync + Hash + Eq + Clone + 'static,
        I: IntoIterator<Item = DataLoaderRequest>,
        T: Loader<DataLoaderRequest>,
    {
        let tid = TypeId::of::<DataLoaderRequest>();

        let (action, rx) = {
            let requests = self.inner.requests.clone();
            let mut tt = requests
                .entry(tid)
                .or_insert_with(|| Requests::<DataLoaderRequest, T>::new(&self.cache_factory));

            let typed_requests = tt
                // .downcast_mut::<Requests<DataLoaderRequest, T>>()
                // .unwrap();
                .value_mut();

            let prev_count = typed_requests.keys.len();
            let mut keys_set = FxHashSet::default();
            let mut use_cache_values = FxHashMap::default();

            if typed_requests.disable_cache || self.disable_cache.load(Ordering::SeqCst) {
                keys_set = keys.into_iter().collect();
            } else {
                for key in keys {
                    if let Some(value) = typed_requests.cache_storage.get(&key) {
                        // Already in cache
                        use_cache_values.insert(key.clone(), value.clone());
                    } else {
                        keys_set.insert(key);
                    }
                }
            }

            if !use_cache_values.is_empty() && keys_set.is_empty() {
                return Ok(use_cache_values);
            } else if use_cache_values.is_empty() && keys_set.is_empty() {
                return Ok(Default::default());
            }

            typed_requests.keys.extend(keys_set.clone());
            let (tx, rx) = flume::unbounded();
            typed_requests.pending.push((
                keys_set,
                ResSender {
                    use_cache_values,
                    tx,
                },
            ));

            if typed_requests.keys.len() >= self.max_batch_size {
                (Action::ImmediateLoad(typed_requests.take()), rx)
            } else {
                (
                    if !typed_requests.keys.is_empty() && prev_count == 0 {
                        Action::StartFetch
                    } else {
                        Action::Delay
                    },
                    rx,
                )
            }
        };

        match action {
            Action::ImmediateLoad(keys) => {
                let inner = self.inner.clone();
                let disable_cache = self.disable_cache.load(Ordering::SeqCst);
                let task = async move { inner.do_load(disable_cache, keys).await };
                #[cfg(feature = "tracing")]
                let task = task
                    .instrument(info_span!("immediate_load"))
                    .in_current_span();

                tokio::task::spawn(task);
            }
            Action::StartFetch => {
                let inner = self.inner.clone();
                let disable_cache = self.disable_cache.load(Ordering::SeqCst);
                let delay = self.delay;

                let task = async move {
                    futures_timer::Delay::new(delay).await;

                    let keys = {
                        let mut request = inner.requests.clone();
                        let mut tt = request
                            .get_mut(&tid)
                            .unwrap();
                        let typed_requests = tt
                            // .downcast_mut::<Requests<DataLoaderRequest, T>>()
                            .value_mut();
                            // .unwrap();

                        typed_requests.take()
                    };

                    if !keys.0.is_empty() {
                        inner.do_load(disable_cache, keys).await
                    }
                };
                #[cfg(feature = "tracing")]
                let task = task.instrument(info_span!("start_fetch")).in_current_span();
                tokio::task::spawn(task);
            }
            Action::Delay => {}
        }

        rx.recv().unwrap()
    }
}