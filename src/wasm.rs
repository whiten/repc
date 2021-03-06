use log::warn;
use std::sync::Once;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsValue;

use crate::dag;
use crate::embed;
use crate::kv::idbstore::IdbStore;
use crate::kv::Store;
use crate::prolly::Map;

// Use `wee_alloc` as the global allocator.
#[global_allocator]
static ALLOC: wee_alloc::WeeAlloc = wee_alloc::WeeAlloc::INIT;

#[wasm_bindgen]
pub async fn exercise_prolly() {
    init_panic_hook();
    let kv = IdbStore::new("foo").await.unwrap().unwrap();
    let store = dag::Store::new(Box::new(kv));
    let mut write = store.write().await.unwrap();
    let mut map = Map::new();
    map.put(b"foo".to_vec(), b"bar".to_vec());
    let h = map.flush(&mut write).await.unwrap();
    warn!("{}", h);
}

#[cfg(not(default))]
pub async fn new_idbstore(name: String) -> Option<Box<dyn Store>> {
    init_panic_hook();
    match IdbStore::new(&name).await {
        Ok(Some(v)) => Some(Box::new(v)),
        _ => None,
    }
}

#[wasm_bindgen]
pub async fn dispatch(db_name: String, rpc: String, args: String) -> Result<String, JsValue> {
    init_panic_hook();
    match embed::dispatch(db_name, rpc, args).await {
        Err(v) => Err(JsValue::from_str(&v[..])),
        Ok(v) => Ok(v),
    }
}

static INIT: Once = Once::new();

pub fn init_console_log() {
    INIT.call_once(|| {
        if let Err(e) = console_log::init_with_level(log::Level::Info) {
            web_sys::console::error_1(&format!("Error registering console_log: {}", e).into());
        }
    });
}

fn init_panic_hook() {
    #[cfg(feature = "console_error_panic_hook")]
    console_error_panic_hook::set_once();
    init_console_log();
}
