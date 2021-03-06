use crate::kv::{Read, Result, Store, StoreError, Write};
use async_std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use async_std::task;
use async_trait::async_trait;
use futures::channel::oneshot;
use futures::future::join_all;
use log::warn;
use std::collections::HashMap;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{IdbDatabase, IdbTransaction};

impl From<String> for StoreError {
    fn from(err: String) -> StoreError {
        StoreError::Str(err)
    }
}

impl From<JsValue> for StoreError {
    fn from(err: JsValue) -> StoreError {
        // TODO(nate): Pick out a useful subset of this value.
        StoreError::Str(format!("{:?}", err))
    }
}

impl From<futures::channel::oneshot::Canceled> for StoreError {
    fn from(_e: futures::channel::oneshot::Canceled) -> StoreError {
        StoreError::Str("oneshot cancelled".into())
    }
}

pub struct IdbStore {
    // We would like:
    // - tests that verify essential behavior such as tx isolation.
    // - Store to be strictly serializabile.
    // - implementations of Store to work in as close to the same way as possible
    //      in order to ensure our tests are realistic and to make replicache easy
    //      to reason about.
    //
    // Idb v2 is (strictly I think) serializable but its API makes it a bit awkward to
    // test: a tx can be created and begin accepting requests before the transaction
    // actually starts, which happens asynchronously and opaquely. This
    // means you can open 20 write txs in parallel and start
    // sending them requests and while only one of them will actually start executing,
    // you can't tell which one.
    //
    // Note:
    // - per https://www.w3.org/TR/IndexedDB-2/#transaction-lifetime-concept
    //      read transacitons can be executed concurrently with readwrite txs
    //      if the read tx is snapshot isolated and started before the readwrite tx.
    //      Browsers however do not do this.
    // - idb v1 api allowed read txs to be re-ordered before write txs, meaning
    //      that indexdb was potentially not read-after-write. Chrome apparently had this
    //      behavior: https://lists.w3.org/Archives/Public/public-webapps/2014JanMar/0586.html).
    //
    // The Memstore implementation we wrote had a simpler to implement interface: at most
    // one write tx can be instantiated at any time and it must be exclusive of all other txs;
    // callers wait asynchronosly to start txs until this constraint can be met.
    //
    // Here we use a RwLock around the underlying idb in order to bring the memstore
    // behavior (caller asynchronously waits to open a tx until it can proceed safely) to idb
    // (caller creates a tx and sends it requests and it starts asynchronously and opaquely).
    // This RwLock makes the Idbstore work like the Memstore, makes it easy to test,
    // and (in my mind) makes it easier to reason about. In principle adding this lock
    // mirrors the constraints in play under the hood, so in principle nbd, but there
    // are probably practical considerations that make this approach less efficient
    // (e.g. if implementations increase concurrency with the snapshot isolation
    // loophole above). It's also the case that we lose a measure of fairness implemented by
    // idb, per the spec: "User agents must ensure a reasonable level of fairness across
    // transactions to prevent starvation. For example, if multiple read-only transactions
    // are started one after another the implementation must not indefinitely prevent a
    // pending read/write transaction from starting." Using the RwLock means the IdbStore is
    // serializable, but not strictly so because the RwLock is not fair and so we don't
    // guarantee temporal ordering (anyone waiting might acquire the lock).
    //
    // It's possible we should have gone the other way and made memstore have the idb
    // interface. However the thing we should not do is have memstore and idbstore work differently.
    db: RwLock<IdbDatabase>,
}

const OBJECT_STORE: &str = "chunks";

impl IdbStore {
    pub async fn new(name: &str) -> Result<Option<IdbStore>> {
        let window = match web_sys::window() {
            Some(w) => w,
            None => return Ok(None),
        };
        let factory = match window.indexed_db()? {
            Some(f) => f,
            None => return Ok(None),
        };
        let request = factory.open(name)?;
        let (callback, receiver) = IdbStore::oneshot_callback();
        let request_copy = request.clone();
        let onupgradeneeded = Closure::once(move |_event: web_sys::IdbVersionChangeEvent| {
            let result = match request_copy.result() {
                Ok(r) => r,
                Err(e) => {
                    warn!("Error before ugradeneeded: {:?}", e);
                    return;
                }
            };
            let db = web_sys::IdbDatabase::unchecked_from_js(result);

            if let Err(e) = db.create_object_store(OBJECT_STORE) {
                warn!("Create object store failed: {:?}", e);
            }
        });
        request.set_onsuccess(Some(callback.as_ref().unchecked_ref()));
        request.set_onerror(Some(callback.as_ref().unchecked_ref()));
        request.set_onupgradeneeded(Some(onupgradeneeded.as_ref().unchecked_ref()));
        receiver.await?;
        Ok(Some(IdbStore {
            db: RwLock::new(request.result()?.into()),
        }))
    }

    /// Returns a oneshot callback and a Receiver to await it being called.
    ///
    /// Intended for use with Idb request callbacks, and may be registered for
    /// multiple callbacks. Await on the Receiver returns when any is invoked.
    fn oneshot_callback() -> (Closure<dyn FnMut()>, oneshot::Receiver<()>) {
        let (sender, receiver) = oneshot::channel::<()>();
        let callback = Closure::once(move || {
            if sender.send(()).is_err() {
                warn!("oneshot send failed");
            }
        });
        (callback, receiver)
    }
}

#[async_trait(?Send)]
impl Store for IdbStore {
    async fn read<'a>(&'a self) -> Result<Box<dyn Read + 'a>> {
        let db_guard = self.db.read().await;
        let tx = db_guard.transaction_with_str(OBJECT_STORE)?;
        Ok(Box::new(ReadTransaction::new(db_guard, tx)?))
    }

    async fn write<'a>(&'a self) -> Result<Box<dyn Write + 'a>> {
        let db_guard = self.db.write().await;
        let tx = db_guard
            .transaction_with_str_and_mode(OBJECT_STORE, web_sys::IdbTransactionMode::Readwrite)?;
        Ok(Box::new(WriteTransaction::new(db_guard, tx)?))
    }
}

struct ReadTransaction<'a> {
    #[allow(dead_code)]
    db: RwLockReadGuard<'a, IdbDatabase>,
    tx: IdbTransaction,
}

impl ReadTransaction<'_> {
    fn new(db: RwLockReadGuard<'_, IdbDatabase>, tx: IdbTransaction) -> Result<ReadTransaction> {
        Ok(ReadTransaction { db, tx })
    }
}

#[async_trait(?Send)]
impl Read for ReadTransaction<'_> {
    async fn has(&self, key: &str) -> Result<bool> {
        has_impl(&self.tx, key).await
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        get_impl(&self.tx, key).await
    }
}

async fn has_impl(tx: &IdbTransaction, key: &str) -> Result<bool> {
    let request = tx.object_store(OBJECT_STORE)?.count_with_key(&key.into())?;
    let (callback, receiver) = IdbStore::oneshot_callback();
    request.set_onsuccess(Some(callback.as_ref().unchecked_ref()));
    request.set_onerror(Some(callback.as_ref().unchecked_ref()));
    receiver.await?;
    let result = request.result()?;
    Ok(match result.as_f64() {
        Some(v) if v >= 1.0 => true,
        Some(_) => false,
        _ => {
            warn!("IdbStore.count returned non-float {:?}", result);
            false
        }
    })
}

async fn get_impl(tx: &IdbTransaction, key: &str) -> Result<Option<Vec<u8>>> {
    let request = tx.object_store(OBJECT_STORE)?.get(&key.into())?;
    let (callback, receiver) = IdbStore::oneshot_callback();
    request.set_onsuccess(Some(callback.as_ref().unchecked_ref()));
    request.set_onerror(Some(callback.as_ref().unchecked_ref()));
    receiver.await?;
    Ok(match request.result()? {
        v if v.is_undefined() => None,
        v => Some(js_sys::Uint8Array::new(&v).to_vec()),
    })
}

#[derive(PartialEq, Eq, Debug)]
enum WriteState {
    Open,
    Committed,
    Aborted,
    Errored,
}

struct WriteTransaction<'a> {
    #[allow(dead_code)]
    db: RwLockWriteGuard<'a, IdbDatabase>,
    tx: IdbTransaction,
    pending: Mutex<HashMap<String, Option<Vec<u8>>>>,
    pair: Arc<(Mutex<WriteState>, Condvar)>,
    callbacks: Vec<Closure<dyn FnMut()>>,
}

impl WriteTransaction<'_> {
    fn new(db: RwLockWriteGuard<'_, IdbDatabase>, tx: IdbTransaction) -> Result<WriteTransaction> {
        let mut wt = WriteTransaction {
            db,
            tx,
            pair: Arc::new((Mutex::new(WriteState::Open), Condvar::new())),
            pending: Mutex::new(HashMap::new()),
            callbacks: Vec::with_capacity(3),
        };

        let tx = &wt.tx;
        let callback = wt.tx_callback(WriteState::Committed);
        tx.set_oncomplete(Some(callback.as_ref().unchecked_ref()));
        wt.callbacks.push(callback);

        let callback = wt.tx_callback(WriteState::Aborted);
        tx.set_onabort(Some(callback.as_ref().unchecked_ref()));
        wt.callbacks.push(callback);

        let callback = wt.tx_callback(WriteState::Errored);
        tx.set_onerror(Some(callback.as_ref().unchecked_ref()));
        wt.callbacks.push(callback);

        Ok(wt)
    }

    fn tx_callback(&self, new_state: WriteState) -> Closure<dyn FnMut()> {
        let pair = self.pair.clone();
        Closure::once(move || {
            task::block_on(async move {
                let (lock, cv) = &*pair;
                let mut state = lock.lock().await;
                *state = new_state;
                cv.notify_one();
            });
        })
    }
}

#[async_trait(?Send)]
impl Read for WriteTransaction<'_> {
    async fn has(&self, key: &str) -> Result<bool> {
        match self.pending.lock().await.get(key) {
            Some(Some(_)) => Ok(true),
            Some(None) => Ok(false),
            None => has_impl(&self.tx, key).await,
        }
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.pending.lock().await.get(key) {
            Some(Some(v)) => Ok(Some(v.to_vec())),
            Some(None) => Ok(None),
            None => get_impl(&self.tx, key).await,
        }
    }
}

#[async_trait(?Send)]
impl Write for WriteTransaction<'_> {
    fn as_read(&self) -> &dyn Read {
        self
    }

    // We hold writes in memory until the API user calls commit
    // to ensure that we don't let partial transactions auto-commit.
    async fn put(&self, key: &str, value: &[u8]) -> Result<()> {
        self.pending
            .lock()
            .await
            .insert(key.into(), Some(value.to_vec()));
        Ok(())
    }

    async fn del(&self, key: &str) -> Result<()> {
        self.pending.lock().await.insert(key.into(), None);
        Ok(())
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        // Define rollback() to succeed if no writes have occurred, even if
        // the underlying transaction has exited. Users who expose themselves
        // to this would notice if they performed any reads after exposing
        // themselves to a situation where the transaction would autocommit.
        let pending = self.pending.lock().await;
        if pending.is_empty() {
            return Ok(());
        }

        let store = self.tx.object_store(OBJECT_STORE)?;
        let mut callbacks = Vec::with_capacity(pending.len());
        let mut requests: Vec<oneshot::Receiver<()>> = Vec::with_capacity(pending.len());
        for (key, value) in pending.iter() {
            let request = match value {
                Some(v) => store.put_with_key(&js_sys::Uint8Array::from(&v[..]), &key.into())?,
                None => store.delete(&key.into())?,
            };
            let (callback, receiver) = IdbStore::oneshot_callback();
            request.set_onsuccess(Some(callback.as_ref().unchecked_ref()));
            callbacks.push(callback);
            requests.push(receiver);
        }
        join_all(requests).await;

        let (lock, cv) = &*self.pair;
        let state = cv
            .wait_until(lock.lock().await, |state| *state != WriteState::Open)
            .await;
        if let Some(e) = self.tx.error() {
            return Err(format!("{:?}", e).into());
        }
        if *state != WriteState::Committed {
            return Err(StoreError::Str("Transaction aborted".into()));
        }
        Ok(())
    }

    async fn rollback(self: Box<Self>) -> Result<()> {
        // Define rollback() to succeed if no writes have occurred, even if
        // the underlying transaction has exited.
        if self.pending.lock().await.is_empty() {
            return Ok(());
        }

        let (lock, cv) = &*self.pair;
        match *lock.lock().await {
            WriteState::Committed | WriteState::Aborted => return Ok(()),
            _ => (),
        }

        self.tx.abort()?;
        let state = cv
            .wait_until(lock.lock().await, |state| *state != WriteState::Open)
            .await;
        if let Some(e) = self.tx.error() {
            return Err(format!("{:?}", e).into());
        }
        if *state != WriteState::Aborted {
            return Err(StoreError::Str("Transaction abort failed".into()));
        }
        Ok(())
    }
}

mod tests {
    // Idbstore is integration tested because web_sys only lives in browsers.
    // See tests/ at top level.
}
