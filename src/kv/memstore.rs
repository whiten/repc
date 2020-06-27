use async_trait::async_trait;
use crate::kv::{Store, StoreError};
use std::collections::HashMap;
use std::fmt;

type Result<T> = std::result::Result<T, StoreError>;

pub struct MemStore {
    map: HashMap<String, Vec<u8>>,
}

impl MemStore {
    pub async fn new(name: &str) -> Result<Option<MemStore>> {
        Ok(Some(MemStore{map: HashMap::new()}))
    }
}

#[async_trait(?Send)]
impl Store for MemStore {
    async fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.map.insert(key.to_string(), value.to_vec());
        Ok(())
    }

    async fn has(&self, key: &str) -> Result<bool> {
        Ok(self.map.contains_key(key))
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.map.get(key) {
            None => Ok(None),
            Some(v) => Ok(Some(v.to_vec())),
        }
    }
}