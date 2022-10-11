use std::borrow::Borrow;
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::ops::Deref;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio::time::{self, Duration};

/// A periodic asynchronous self-rehydrating cache.
pub struct Cache<K, V> {
    items: HashMap<K, CacheVal<V>>,
}

impl<K, V> Default for Cache<K, V> {
    fn default() -> Self {
        Self {
            items: HashMap::default(),
        }
    }
}

impl<K: Eq + Hash, V> Cache<K, V> {
    /// Inserts a new key value pair in the hash table, where the value is a cloned reference of the
    /// [CacheVal] provided as part of the [TaskArgs], and then immediately spawns a new detached
    /// asynchronous task in charge of updating the value as described by the task parameters.
    pub fn insert<UpdateFn, Out>(&mut self, key: K, args: TaskArgs<V, UpdateFn>)
    where
        V: Send + Sync + 'static,
        UpdateFn: Fn(CacheVal<V>) -> Out + Send + Sync + 'static,
        Out: Future + Send + 'static,
    {
        self.items.insert(key, CacheVal::clone(&args.value));
        tokio::spawn(timer(args));
    }

    /// Gets the most recent value associated with the given key. Returns None is the key was never
    /// inserted or the value has been evicted by the background task. Otherwise, returns a clone of
    /// `Some(V)` following an asynchronous read lock.
    pub async fn get<Q>(&self, key: &Q) -> Option<V>
    where
        V: Clone,
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let item = self.items.get(key)?;
        item.read().await.clone()
    }

    /// Gets and maps the most recent value associated with the given key.
    /// For more information about the retrieval see [Cache::get].
    pub async fn get_map<Q, MapFn, U>(&self, key: &Q, map_fn: MapFn) -> Option<U>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
        MapFn: FnOnce(&V) -> U,
    {
        let item = self.items.get(key)?;
        item.read().await.as_ref().map(map_fn)
    }
}

/// Arguments of the background task that owns a value shared with the [Cache] and can update it via
/// interior mutability.
pub struct TaskArgs<V, UpdateFn> {
    /// Time To Live. When expired the cache value will be set to None.
    pub ttl: Duration,
    /// Interval of time between consecutive calls to the update function.
    pub update_interval: Duration,
    /// Value to update, associated with a specific cache unique key.
    pub value: CacheVal<V>,
    /// Function that is called when the task value needs to be updated.
    pub update_fn: UpdateFn,
}

/// An asynchronous background task that continuously updates the cache value by calling the update
/// function at every update interval of time until the TTL expires, at which point evicts the cache
/// value and terminates.
async fn timer<V, UpdateFn, Out>(args: TaskArgs<V, UpdateFn>)
where
    UpdateFn: Fn(CacheVal<V>) -> Out,
    Out: Future,
{
    let mut ttl_interval = time::interval(args.ttl);
    let mut update_interval = time::interval(args.update_interval);
    // the first tick completes immediately
    tokio::join!(ttl_interval.tick(), update_interval.tick());

    loop {
        tokio::select! {
            _ = ttl_interval.tick() => {
                // evict the cache value by setting it to None and terminate the task
                args.value.write().await.take();
                return;
            }
            _ = update_interval.tick() => {
                // call the update function with a shared reference to the cache value
                (args.update_fn)(CacheVal::clone(&args.value)).await;
            }
        };
    }
}

/// Wraps a value stored in the [Cache] behind a thread-safe reference-counting pointer (`Arc`) and
/// an asynchronous reader-writer lock (`RwLock`).
#[derive(Debug)]
pub struct CacheVal<V>(Arc<RwLock<Option<V>>>);

impl<V> CacheVal<V> {
    pub fn new(value: V) -> Self {
        Self(Arc::new(RwLock::new(Some(value))))
    }
}

impl<V> Default for CacheVal<V> {
    fn default() -> Self {
        Self(Arc::new(RwLock::new(None)))
    }
}

impl<V> Clone for CacheVal<V> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<V> Deref for CacheVal<V> {
    type Target = Arc<RwLock<Option<V>>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct Component {
        price: f64,
    }

    #[tokio::test]
    async fn test_cache_val() {
        // construct a new instance
        let component = Component { price: 10.0 };
        let val = CacheVal::new(component.clone());

        // asynchronous mutex lock to read inner value
        assert_eq!(&component, val.read().await.as_ref().unwrap());

        // default value is set to None
        assert!(CacheVal::<Component>::default().read().await.is_none());

        // clone shared reference
        let val2 = CacheVal::clone(&val);
        assert_eq!(&component, val2.read().await.as_ref().unwrap());

        // asynchronous mutex lock to evict the value by setting it to None
        val.write().await.take();
        assert!(val.read().await.is_none());
        assert!(val2.read().await.is_none());
    }

    #[tokio::test]
    async fn test_timer() {
        use tokio::sync::mpsc::{self, UnboundedSender};

        type Value = (UnboundedSender<String>, Component);

        let component = Component { price: 10.0 };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let value = CacheVal::new((tx, component));

        let args = TaskArgs {
            ttl: Duration::from_millis(600),
            update_interval: Duration::from_millis(50),
            value: CacheVal::clone(&value),
            update_fn: update_price,
        };

        async fn update_price(value: CacheVal<(UnboundedSender<f64>, Component)>) {
            let mut value = value.write().await;
            let (tx, component) = value.as_mut().unwrap();
            component.price += 1.0;
            tx.send(component.price).unwrap();
        }

        timer(args).await;

        for price in 11..=15 {
            assert_eq!(rx.recv().await.unwrap(), price as f64);
        }
    }

    #[tokio::test]
    async fn test_cache_insert_get() {
        let mut cache = Cache::default();

        async fn update_transistor_price(_: CacheVal<Component>) {}
        async fn update_diode_price(_: CacheVal<Component>) {}

        cache.insert(
            ":transistor".to_string(),
            TaskArgs {
                ttl: Duration::from_secs(5),
                update_interval: Duration::from_secs(1),
                value: CacheVal::new(Component { price: 10.0 }),
                update_fn: update_transistor_price,
            },
        );

        cache.insert(
            ":diode".to_string(),
            TaskArgs {
                ttl: Duration::from_secs(10),
                update_interval: Duration::from_secs(3),
                value: CacheVal::default(),
                update_fn: update_diode_price,
            },
        );

        assert!(cache.get(":not-found").await.is_none());

        let price = cache
            .get_map(":transistor", |component| component.price)
            .await
            .unwrap();
        assert_eq!(price, 10.0);

        assert!(cache.get(":diode").await.is_none());
    }

    #[tokio::test]
    async fn test_cache_err_handling() {
        use tokio::time::sleep;

        type Value = Result<Component, String>;

        let mut cache = Cache::default();

        async fn update_transistor_price(value: CacheVal<Value>) {
            async fn query_manufacturer() -> Value {
                Err("404: Not Found".into())
            }

            let new_value = query_manufacturer().await;
            *value.write().await = Some(new_value);
        }

        cache.insert(
            ":transistor",
            TaskArgs {
                ttl: Duration::from_secs(5),
                update_interval: Duration::from_millis(50),
                value: CacheVal::default(),
                update_fn: update_transistor_price,
            },
        );

        assert!(cache.get(":transistor").await.is_none());

        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            cache.get(":transistor").await.unwrap(),
            Err("404: Not Found".into())
        );
    }

    #[tokio::test]
    async fn test_cache_channels_err_handling() {
        use tokio::sync::mpsc::{self, UnboundedSender};

        type Value = (UnboundedSender<String>, Component);

        let mut cache = Cache::default();
        let (tx, mut rx) = mpsc::unbounded_channel();

        async fn update_transistor_price(value: CacheVal<Value>) {
            async fn query_manufacturer() -> Result<f64, String> {
                Err("404: Not Found".into())
            }

            match query_manufacturer().await {
                Ok(price) => {
                    let mut value = value.write().await;
                    let (_, component) = value.as_mut().unwrap();
                    component.price = price;
                }
                Err(e) => {
                    let value = value.read().await;
                    let (tx, _) = value.as_ref().unwrap();
                    tx.send(e).unwrap();
                }
            };
        }

        cache.insert(
            ":transistor",
            TaskArgs {
                ttl: Duration::from_secs(5),
                update_interval: Duration::from_millis(100),
                value: CacheVal::new((tx, Component { price: 10.0 })),
                update_fn: update_transistor_price,
            },
        );

        assert_eq!(rx.recv().await.unwrap(), "404: Not Found");

        let price = cache
            .get_map(":transistor", |(_, component)| component.price)
            .await
            .unwrap();
        assert_eq!(price, 10.0);
    }
}
