# sr-cache

A periodic asynchronous self-rehydrating cache.


## Overview

The goal of this little project is to show step by step how to write a small and basic Rust implementation of a periodic self-rehydrating cache, where the user has access to two main APIs that resemble a [HashMap][1] and allow to:
- Insert a key-value pair along with:
    - Time To Live (TTL): interval of time after which the value is evicted from the cache. It effectively represents our simple cache replacement policy.
    - Update interval: interval of time after which, if the TTL has not yet expired, to call an asynchronous function that will allow to update the value stored in the cache.
    - The asynchronous update function.
- Get the most recent value associated with a given key.

This kind of caching mechanism can be useful when we need to work with data that can be retrieved concurrently and doesn't change very often. As an example, throughout this article we will work with a cache where we are going to store the price of electronic components, which can be retrieved by querying the web servers of 3rd party manufacturers via HTTP calls.


## The asynchronous runtime

The first thing to consider is possibly how are we going to support updating the cache values asynchronously in a way that is opaque to the user, that is, being able to hide the complexity of a background task that takes care of the updates from the public APIs of the cache.

For this use case, I chose [Tokio][2], arguably one of the most complete and best maintained asynchronous runtimes in Rust. The main features we will exploit are the ability to [spawn][3] asynchronous tasks (where to run the update function provided by the user of the cache) as well as the ability to keep track of [time][4] (TTL and update interval).


## The Cache data structure

As already mentioned, our cache is going to behave very similarly to a hash table; we can start by defining it as follows:

```rust
use std::collections::HashMap;

pub struct Cache<K, V> {
    items: HashMap<K, CacheVal<V>>,
}
```

From the above definition, we can see how our cache will be generic over its type parameters `K` (for the key type) and `V` (for the value type). Note how we are not storing `V` directly as value in our hash table, but instead `V` is wrapped in a new type `CacheVal<V>` that we are yet to define. This is because we are going to exploit the type properties of `CacheVal<V>` to communicate with the cache when a value `V` is updated by a background task.

To understand how to make this possible, we need to think about what we want to achieve:

- Shared ownership: The value stored in the cache should also be accessible by a background task that can change its value, and which ideally should not have access to the whole cache.

    Following this requirement, the default wrapper for our values `V` is [Rc][5], a single-threaded reference-counting pointer, which provides shared ownership of a value of type `V`. We can use reference-counting pointers to share references to the same heap allocation of the same value by, for example, passing its [Clone][6] to the background task.

    ```rust
    use std::rc::Rc;

    pub type CacheVal<V> = Rc<V>; 
    ```

- Thread-safety: each value can be associated with its own lightweight and non-blocking background task, and each task can run concurrently to the other tasks. This can happen on the same thread that spawned the task, but it may also be sent to a different thread depending on the runtime configuration, which in our case will be multi-threaded.

    Following this requirement, we need to change `Rc` to [Arc][7]. Unlike `Rc<V>`, `Arc<V>` uses atomic operations for its reference counting, making it thread-safe (at the expense of the more costly atomic operations required to update the reference count).

    ```rust
    use std::sync::Arc;

    pub type CacheVal<V> = Arc<V>; 
    ```

- (Interior) Mutability: each background task needs to be able to change the value it is in charge of updating, so that the most up to date value can be reflected by what is returned to the user when querying the cache.

    In Rust, shared references such as `Arc` in Rust disallow mutation by default: we cannot obtain a mutable reference to our values `V` when stored inside an `Arc<V>`. However, we need to be able to mutate `V` while having multiple aliases. This can be achieved using a pattern called interior mutability. A type has interior mutability if its internal state can be changed through a shared reference to it.
    To achieve this, we can make use of mutexes, and since the data `V` we are going to protect can be accessed by both the background task and the user (via the cache APIs) in a separate thread, we can avoid blocking the user thread that is trying to acquire the lock when the background task has locked it already (and vice-versa) by using an asynchronous mutex provided by Tokio, which in this scenario will yield execution back to the runtime. Moreover, since we can differentiate between read (eg: getting the value from the cache) and write (eg: setting the updated value in the background task) operations, we are going to use a [RwLock][25] asynchronous reader-writer lock.

    ```rust
    use std::sync::Arc;
    use tokio::sync::RwLock;

    pub type CacheVal<V> = Arc<RwLock<V>>;
    ```

- Eviction: finally, we need to be able to encode in our type the information that will tell us if the value associated with a given cache key has been evicted, due to the expiration of the TTL, or if its value is still considered valid.

    We can express this requirement in the type system by allowing our values to be always either valid or evicted via one of the most common Rust tagged union types: [Option][8]. When the value is set to `Some` it will represent a valid value, otherwise it will be set and returned to the user as `None` when the TTL has expired.

     ```rust
    use std::sync::Arc;
    use tokio::sync::RwLock;

    pub type CacheVal<V> = Arc<RwLock<Option<V>>>;
    ```


Going back to our cache, what we are missing is a way to construct an instance of it with initially no elements. To do so, we are going to manually implement the [Default][22] trait. Note how we need to be able to call `Default` for our `Cache<K, V>` even if `V` is a type that does not implement `Default`, therefore simply [deriving Default][23] would not be sufficient.

```rust
impl<K, V> Default for Cache<K, V> {
    fn default() -> Self {
        Self {
            items: HashMap::default(),
        }
    }
}
```


## CacheVal behavior

So far, we have been able to define the `Cache` data structure and, in particular, define how values are going to be stored, so that they can be accessed by both the user and the background task in charge of updating their value, by wrapping them in a new type alias `CacheVal<V>`. Type aliases in Rust allow us to simply define a new name for an existing type.

Although `CacheVal<V>` is a type with basic semantics, we would still like to define its basic behavior by implementing a few traits that will become useful later on and make the API more convenient for the user of this type. For example, we could implement a constructor that takes a `V` and simply returns a `CacheVal<V>`, or we could implement the trait [Debug][9] for `CacheVal<V>` (when `V` implements `Debug`). To do this, we can start by creating a new tuple struct type with a single field.

```rust
#[derive(Debug)]
pub struct CacheVal<V>(Arc<RwLock<Option<V>>>);
```

The first useful trait we are going to implement is [Clone][6]; this follows the shared ownership requirement that we discussed before, which implies we now need to be able to clone an instance of `CacheVal<V>`. Invoking clone on `CacheVal<V>` will produce a new `CacheVal<V>` instance, which points to the same allocation on the heap as the source `CacheVal<V>`, while increasing its reference count. Note how we need to be able to `Clone` our `CacheVal<V>` even if `V` is a type that cannot be cloned, therefore, similarly to what we have seen for the cache `Default` trait implementation, simply [deriving Clone][12] would not be sufficient unless we can always guarantee that `V` is also clonable (which is not a requirement for us).

```rust
impl<V> Clone for CacheVal<V> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}
```

Likewise, for the principle of [eagerly implementing common traits][24], we are going to implement [Default][22] for `CacheVal<V>`, so that by default its inner value is initialized to `None`:

```rust
impl<V> Default for CacheVal<V> {
    fn default() -> Self {
        Self(Arc::new(RwLock::new(None)))
    }
}
```

In order to give immutable access to the `struct` field (and avoid declaring the field public as well as having to access it via tuple indexing), we simply need to implement the [Deref][10] trait. Treating our smart pointer `CacheVal<V>` like a regular reference to its field is called [deref coercion][11] and can conveniently work by being implicitly applied by the compiler so that writing function and method calls doesn't require the addition of as many explicit references and dereferences with `&` and `*`.

```rust
use std::ops::Deref;

impl<V> Deref for CacheVal<V> {
    type Target = Arc<RwLock<Option<V>>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
```

Finally, we are going to implement a simple constructor for `CacheVal<V>`, which constructs a new instance of `CacheVal<V>` when given ownership of an instance of type `V`:

```rust
impl<V> CacheVal<V> {
    pub fn new(value: V) -> Self {
        Self(Arc::new(RwLock::new(Some(value))))
    }
}
```

All the above allows us to work with `CacheVal<V>` by possibly exposing simple APIs:

```rust
#[derive(Debug, Clone)]
struct Component {
    price: f64,
}

// construct a new instance
let val = CacheVal::new(Component { price: 10.0 });

// asynchronous mutex lock to read inner value
println!("{:?}", val.read().await);     // Some(Component { price: 10.0 })

// default value is set to None
println!("{:?}", CacheVal::<Component>::default().read().await); // None

// clone shared reference
let val2 = CacheVal::clone(&val);
println!("{:?}", val2.read().await);    // Some(Component { price: 10.0 })

// asynchronous mutex lock to evict the value by setting it to None
val.write().await.take();
println!("{:?}", val.read().await);     // None
println!("{:?}", val2.read().await);    // None
```


## The background task

There are two main aspects to discuss about the background task: the arguments that need to be provided (such as TTL and update interval) and the task's actual implementation.

We can start by defining the type that will include all the arguments that we need to provide, and which can be used as the same abstraction for both the task implementation and the cache public API. Here's the full definition:

```rust
use tokio::time::Duration;

pub struct TaskArgs<V, UpdateFn> {
    pub ttl: Duration,
    pub update_interval: Duration,
    pub value: CacheVal<V>,
    pub update_fn: UpdateFn,
}
```

The `ttl` and `update_interval` fields are self-explanatory, while `value` is the cache value that is shared between the task and the cache itself, and will be updated by calling the `UpdateFn` stored in the fourth field of this `struct`.

What's missing next is the implementation of the background task; its logic is relatively simple: continue to update the cache value by calling the update function at every update interval of time until the TTL expires, at which point evict the cache value and terminate the task. Fortunately, Tokio provides us all the time primitives and features to detect when a specific (or multiple) interval of time has elapsed in an asynchronous fashion.

What follows is the implementation of an asynchronous function that internally behaves as a timer, by [selecting][16] which of the TTL vs update [Interval][15] futures completes first.


```rust
use std::future::Future;
use tokio::time;

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
```

The trait bounds of the generic type parameter `UpdateFn` state that it must implement a call operator [Fn][13] that operates over the input value of type `CacheVal<V>` and returns a generic output type `Out` that represents the result of an asynchronous computation, aka [Future][17].

Note how passing the whole `CacheVal<V>` to the update function gives finer granularity over when to mutably request access to the inner value `V` by letting the implementer of the update function decide when to lock the mutex, rather than locking prior to the call to the update function and passing a mutex guard to it.

Assuming we already have a Tokio runtime running, the `timer` function can be used as follow:

```rust
use tokio::time::Instant;

let start = Instant::now();
let component = Component { price: 10.0 };

let args = TaskArgs {
    ttl: Duration::from_secs(5),
    update_interval: Duration::from_secs(1),
    value: CacheVal::new((start, component)),
    update_fn: update_price,
};

async fn update_price(value: CacheVal<(Instant, Component)>) {
    let mut value = value.write().await;
    let (start, component) = value.as_mut().unwrap();
    component.price += 1.0;

    println!(
        "Price at {}s: {}",
        Instant::now().duration_since(*start).as_secs(),
        component.price
    );
}

timer(args).await;
// Price at 1s: €11
// Price at 2s: €12
// Price at 3s: €13
// Price at 4s: €14
// Price at 5s: €15
```


## The Cache APIs

As previously described in the [Overview](#overview), we are going to implement two main APIs for our cache: the first one to insert new values associated to a unique key, and the second one to retrieve the most recent value for that key.

Starting from the `insert` method, the logic here is also relatively simple: we insert a new key value pair in the hash table, where the value is a cloned reference of the `CacheVal<V>` provided as part of the `TaskArgs<V, UpdateFn>`, and we then immediately [spawn][3] a new detached asynchronous task that will run the previously described `timer` function.

```rust
use std::hash::Hash;

impl<K: Eq + Hash, V> Cache<K, V> {
    pub fn insert<UpdateFn, Out>(&mut self, key: K, args: TaskArgs<V, UpdateFn>)
    where
        V: Send + Sync + 'static,
        UpdateFn: Fn(CacheVal<V>) -> Out + Send + Sync + 'static,
        Out: Future + Send + 'static,
    {
        self.items.insert(key, CacheVal::clone(&args.value));
        tokio::spawn(timer(args));
    }
}
```

Since internally we are using a `HashMap` to store keys and values, it is required that the keys `K` implement the [Eq][18] and [Hash][19] traits, and we are requiring this in the `impl` block as `K: Eq + Hash`; any other trait bounds we will see are left to each specific method implementation.

Let's try now to demystify the trait bounds that we specified as part of the method `where` clause. The signature of the `insert` method allows us to define at compile time the type of the `UpdateFn` and of its return type `Out`. In particular, since we're going to use the `args` as parameter of the `timer` function, we start by specifying the same trait bounds that are required by the `timer` function:

```rust
UpdateFn: Fn(CacheVal<V>) -> Out,
Out: Future
```

On top of these, this is what [tokio::spawn][3] requires:

```rust
pub fn spawn<T>(future: T) -> JoinHandle<T::Output>
where
    T: Future + Send + 'static,
    T::Output: Send + 'static,
```

Basically meaning that both the future `T` and its output `T::Output` must be safe to [Send][20] to another thread (this allows the Tokio runtime to move the tasks between threads while they are suspended at an `.await`), and also that the spawned task must not contain any references to data owned outside the task (set by specifying the `'static` lifetime).

Tasks are `Send` when all data that is held across `.await` calls is `Send`; since our `TaskArgs<V, UpdateFn>` is part of the state of the task and it is reused and persisted across `.await` calls it must also be `Send`.

```rust
Out: Future + Send + 'static,
UpdateFn: Fn(CacheVal<V>) -> Out + Send + Sync + 'static,
```

Note how we also had to restrict the `UpdateFn` trait bounds to implement `Sync`. This is required because when we call the update function, we are calling it by reference `&UpdateFn`, and due to what we described before about allowing Tokio to move tasks (and their state) between threads, it follows that `&UpdateFn` must be `Send`, that is, it needs to be possible to reference `UpdateFn` from multiple threads at the same type, which is the definition of `Sync`. 

Finally, `V` needs to be restricted to the following trait bounds:

```rust
V: Send + Sync + 'static
```

While the `Send + 'static` bounds follow what we have describe above, the reason why `V` needs to be `Sync` is more subtle, but it comes down to the fact that in order for the future spawned by Tokio to be `Send`, our `UpdateFn` argument `CacheVal<V>` also needs to be `Send`. If we revisit the types that are part of `CacheVal<V>`, we'll see that it basically corresponds to an `Arc<RwLock<Option<V>>>` and, from the Rust standard library, we can see that:

```rust
// for Arc<T> to be Send T must be Send + Sync
impl<T: Sync + Send> Send for Arc<T> {}

// for RwLock<T> to be Send T must be Send
impl<T: Send> Send for RwLock<T> {}
// for RwLock<T> to be Sync T must be Send + Sync
impl<T: Send + Sync> Sync for RwLock<T> {}

// Option<T> is Send only if T is Send (and the same applies for Sync)
impl<T: Send> Send for Option<T> {}
impl<T: Sync> Sync for Option<T> {}
```

Therefore, for `CacheVal<V>` to be `Send`, `V` needs to be `Send + Sync`. Note how this requirement could be lifted if we instead used `Mutex<T>` instead of `RwLock<T>`, which only requires `impl<T: Send> Sync for Mutex<T> {}`. This is allowed because there will never be multiple immutable references of `T` at the same time when using a `Mutex<T>`, as it always only allows a mutually exclusive access to the data it protects (for both read and write operations, while `RwLock<T>` allows to have a finer control over these).

Finally, the cache `get` method is defined as follows:

```rust
use std::hash::Hash;
use std::borrow::Borrow;

impl<K: Eq + Hash, V> Cache<K, V> {
    pub async fn get<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
        V: Clone,
    {
        let item = self.items.get(key)?;
        item.read().await.clone()
    }
}
```

There are probably a few interesting points to highlight about this method:
- It accepts as a key a reference to anything (`Q`) that can be [borrowed][21] from an actual key `K`, allowing us to provide as parameter to this method a different representation of the key. For example, consider the case where your keys (`K`) were `String`, but you can call the `get` method using a `str` (thus avoiding an extra heap allocation); this would now be possible thanks to the `get` API signature and the fact that the standard library provides a `impl Borrow<str> for String`.
- It returns a [Clone][6] of the value `V`, which is wrapped in an `Option` that will be `None` if the key does not exist in our hash table or if the key has been evicted. It wouldn't be possible to return a reference `&V` to the value as this would effectively represent a reference to the value owned by the lock guard returned by the `RwLock::read` method (local to the `get` method). This is intuitively correct as otherwise the user of the cache would be able to read a reference to a value that could be changed by the background task without any synchronization mechanism. Instead, we decide to return a copy of the inner value by cloning it. Alternatively, it would be possible to return `Option<CacheVal<V>>`, but this may have some disadvantages depending on the user requirements, such as a less convenient API (effectively this represents an `Option` within an `Option` that can differentiate whether the key was ever inserted in the hash table or it was inserted but then later evicted) and giving the user the possibility of changing the shared inner value itself by exploiting the internal mutability offered by `CacheVal<V>` by calling `RwLock::write`.

    If you are interested only in part of the value `V` (imagine the case where `V` contains additional data that can be used in the update function but is not important at the time of retrieval), avoiding the `Clone` is also possible by, for example, implementing a map function that returns a new type `U` (defined by the user) from `&V`:

    ```rust
    pub async fn get_map<Q, MapFn, U>(&self, key: &Q, map_fn: MapFn) -> Option<U>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
        MapFn: FnOnce(&V) -> U,
    {
        let item = self.items.get(key)?;
        item.read().await.as_ref().map(map_fn)
    }
    ```

With the above described APIs, our `Cache` can be used as follows for example:

```rust
use tokio::time::sleep;

async fn update_transistor_price(_: CacheVal<Component>) {}

let mut cache = Cache::default();

cache.insert(
    ":transistor".to_string(),
    TaskArgs {
        ttl: Duration::from_secs(600),
        update_interval: Duration::from_secs(5),
        value: CacheVal::new(Component { price: 10.0 }),
        update_fn: update_transistor_price,
    },
);

println!("{:?}", cache.get(":not-found").await);    // None

let transistor = cache.get(":transistor").await.unwrap();
println!("{transistor:?}"); // Component { price: 10.0 }

let price = cache
    .get_map(":transistor", |component| component.price)
    .await
    .unwrap();
println!("{price}");        // 10
```


## Error handling

As part of the last section, we are going to briefly talk about error handling and show how we can use the current APIs to signal that a background task raised an error (for example when querying the 3rd-party service to get the price of the electronic components). Because the background tasks run asynchronously, we can expect our error handling to follow the same pattern.

There are at least a couple of ways we could support error handling:

- On a query basis: where we are interested to know if an error occurred when we query the value associated with a given key via the `get` API. In this use case, we could simply define our user type `V` so that it encodes this information in the type itself.

    ```rust
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
            update_interval: Duration::from_millis(500),
            value: CacheVal::default(),
            update_fn: update_transistor_price,
        },
    );

    println!("{:?}", cache.get(":transistor").await);           // None
    sleep(Duration::from_secs(1)).await;
    println!("{:?}", cache.get(":transistor").await.unwrap());  // Err("404: Not Found")
    ```

- Fully asynchronously: where we are interested to know if an error occurred when it occurs and get notified when this happens. In this case, we can still exploit the possibility of defining our own type `V` to support this feature by sending errors using the [mpsc][26] implementation provided by Tokio.

    ```rust
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
            update_interval: Duration::from_millis(500),
            value: CacheVal::new((tx, Component { price: 10.0 })),
            update_fn: update_transistor_price,
        },
    );

    let err = rx.recv().await.unwrap();
    println!("{err}");          // 404: Not Found

    let transistor = cache
        .get_map(":transistor", |(_, component)| component.clone())
        .await
        .unwrap();
    println!("{transistor:?}"); // Component { price: 10.0 }
    ```


[1]: https://doc.rust-lang.org/stable/std/collections/struct.HashMap.html
[2]: https://tokio.rs/
[3]: https://docs.rs/tokio/latest/tokio/task/fn.spawn.html
[4]: https://docs.rs/tokio/latest/tokio/time/index.html
[5]: https://doc.rust-lang.org/std/rc/index.html
[6]: https://doc.rust-lang.org/std/clone/trait.Clone.html
[7]: https://doc.rust-lang.org/std/sync/struct.Arc.html
[8]: https://doc.rust-lang.org/std/option/enum.Option.html
[9]: https://doc.rust-lang.org/std/fmt/trait.Debug.html
[10]: https://doc.rust-lang.org/std/ops/trait.Deref.html
[11]: https://doc.rust-lang.org/book/ch15-02-deref.html
[12]: https://doc.rust-lang.org/std/clone/trait.Clone.html#derivable
[13]: https://doc.rust-lang.org/std/ops/trait.Fn.html
[14]: https://doc.rust-lang.org/std/result/enum.Result.html
[15]: https://docs.rs/tokio/latest/tokio/time/struct.Interval.html
[16]: https://docs.rs/tokio/latest/tokio/macro.select.html
[17]: https://doc.rust-lang.org/std/future/trait.Future.html
[18]: https://doc.rust-lang.org/std/cmp/trait.Eq.html
[19]: https://doc.rust-lang.org/std/hash/trait.Hash.html
[20]: https://doc.rust-lang.org/std/marker/trait.Send.html
[21]: https://doc.rust-lang.org/std/borrow/trait.Borrow.html
[22]: https://doc.rust-lang.org/std/default/trait.Default.html
[23]: https://doc.rust-lang.org/std/default/trait.Default.html#derivable
[24]: https://rust-lang.github.io/api-guidelines/interoperability.html#types-eagerly-implement-common-traits-c-common-traits
[25]: https://docs.rs/tokio/latest/tokio/sync/struct.RwLock.html
[26]: https://docs.rs/tokio/latest/tokio/sync/mpsc/index.html
