use libp2p::kad::store::{Error, RecordStore, Result};
use libp2p::kad::{ProviderRecord, Record, RecordKey as Key};
use libp2p::PeerId;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone)]
pub struct BoundedConfig {
    pub max_records: usize,
    pub max_value_bytes: usize,
    pub max_providers_per_key: usize,
    pub max_provided_keys: usize,
}

impl Default for BoundedConfig {
    fn default() -> Self {
        BoundedConfig {
            max_records: 1024,
            max_value_bytes: 64 * 1024,
            max_providers_per_key: 20,
            max_provided_keys: 1024,
        }
    }
}

/// A bounded Kademlia record store.
///
/// Value records evict oldest-first when full, so record spam churns instead
/// of permanently locking out legitimate records (libp2p's `MemoryStore`
/// fails closed when full, which lets the first spammer win). Provider
/// records deliberately keep `MemoryStore`'s ignore-when-full behavior:
/// evicting providers would let Sybils flush legitimate providers, which
/// is worse than a full list that expires by TTL.
pub struct BoundedStore {
    local_id: PeerId,
    config: BoundedConfig,
    records: HashMap<Key, Record>,
    insertion_order: VecDeque<Key>,
    provided: HashSet<ProviderRecord>,
    providers: HashMap<Key, Vec<ProviderRecord>>,
}

impl BoundedStore {
    pub fn new(local_id: PeerId) -> Self {
        Self::with_config(local_id, BoundedConfig::default())
    }

    pub fn with_config(local_id: PeerId, config: BoundedConfig) -> Self {
        BoundedStore {
            local_id,
            config,
            records: HashMap::new(),
            insertion_order: VecDeque::new(),
            provided: HashSet::new(),
            providers: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn evict_oldest(&mut self) {
        while let Some(key) = self.insertion_order.pop_front() {
            if self.records.remove(&key).is_some() {
                return;
            }
        }
    }

    fn refresh_recency(&mut self, key: &Key) {
        if let Some(pos) = self.insertion_order.iter().position(|k| k == key) {
            self.insertion_order.remove(pos);
        }
        self.insertion_order.push_back(key.clone());
    }
}

impl RecordStore for BoundedStore {
    type RecordsIter<'a> = std::iter::Map<
        std::collections::hash_map::Values<'a, Key, Record>,
        fn(&'a Record) -> Cow<'a, Record>,
    >;
    type ProvidedIter<'a> = std::iter::Map<
        std::collections::hash_set::Iter<'a, ProviderRecord>,
        fn(&'a ProviderRecord) -> Cow<'a, ProviderRecord>,
    >;

    fn get(&self, k: &Key) -> Option<Cow<'_, Record>> {
        self.records.get(k).map(Cow::Borrowed)
    }

    fn put(&mut self, r: Record) -> Result<()> {
        if r.value.len() >= self.config.max_value_bytes {
            return Err(Error::ValueTooLarge);
        }
        if self.records.contains_key(&r.key) {
            self.records.insert(r.key.clone(), r.clone());
            self.refresh_recency(&r.key);
            return Ok(());
        }
        if self.records.len() >= self.config.max_records {
            self.evict_oldest();
        }
        self.insertion_order.push_back(r.key.clone());
        self.records.insert(r.key.clone(), r);
        Ok(())
    }

    fn remove(&mut self, k: &Key) {
        self.records.remove(k);
    }

    fn records(&self) -> Self::RecordsIter<'_> {
        self.records.values().map(Cow::Borrowed)
    }

    fn add_provider(&mut self, record: ProviderRecord) -> Result<()> {
        let num_keys = self.providers.len();
        let providers = match self.providers.entry(record.key.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                if num_keys >= self.config.max_provided_keys {
                    return Err(Error::MaxProvidedKeys);
                }
                e.insert(Vec::new())
            }
        };
        for p in providers.iter_mut() {
            if p.provider == record.provider {
                if self.local_id == record.provider {
                    self.provided.remove(p);
                    self.provided.insert(record.clone());
                }
                *p = record;
                return Ok(());
            }
        }
        if providers.len() >= self.config.max_providers_per_key {
            return Ok(());
        }
        if self.local_id == record.provider {
            self.provided.insert(record.clone());
        }
        providers.push(record);
        Ok(())
    }

    fn providers(&self, key: &Key) -> Vec<ProviderRecord> {
        self.providers.get(key).cloned().unwrap_or_default()
    }

    fn provided(&self) -> Self::ProvidedIter<'_> {
        self.provided.iter().map(Cow::Borrowed)
    }

    fn remove_provider(&mut self, k: &Key, provider: &PeerId) {
        if let std::collections::hash_map::Entry::Occupied(mut e) = self.providers.entry(k.clone())
        {
            let providers = e.get_mut();
            if let Some(i) = providers.iter().position(|p| &p.provider == provider) {
                let p = providers.remove(i);
                if p.provider == self.local_id {
                    self.provided.remove(&p);
                }
            }
            if providers.is_empty() {
                e.remove();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::kad::Record;

    fn test_key(n: u8) -> Key {
        Key::from(vec![n])
    }

    fn test_record(n: u8, value: Vec<u8>) -> Record {
        Record {
            key: test_key(n),
            value,
            publisher: None,
            expires: None,
        }
    }

    #[test]
    fn put_get_remove_roundtrip() {
        let mut store = BoundedStore::new(PeerId::random());
        let record = test_record(1, b"value".to_vec());
        store.put(record.clone()).unwrap();
        assert_eq!(store.get(&record.key).as_deref(), Some(&record));
        assert_eq!(store.len(), 1);
        store.remove(&record.key);
        assert!(store.get(&record.key).is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn full_store_evicts_oldest_not_newest() {
        let mut store = BoundedStore::with_config(
            PeerId::random(),
            BoundedConfig {
                max_records: 2,
                ..BoundedConfig::default()
            },
        );
        store.put(test_record(1, b"a".to_vec())).unwrap();
        store.put(test_record(2, b"b".to_vec())).unwrap();
        store.put(test_record(3, b"c".to_vec())).unwrap();
        assert_eq!(store.len(), 2);
        assert!(store.get(&test_key(1)).is_none());
        assert!(store.get(&test_key(2)).is_some());
        assert!(store.get(&test_key(3)).is_some());
    }

    #[test]
    fn replacing_a_key_refreshes_recency() {
        let mut store = BoundedStore::with_config(
            PeerId::random(),
            BoundedConfig {
                max_records: 2,
                ..BoundedConfig::default()
            },
        );
        store.put(test_record(1, b"a".to_vec())).unwrap();
        store.put(test_record(2, b"b".to_vec())).unwrap();
        store.put(test_record(1, b"a2".to_vec())).unwrap();
        store.put(test_record(3, b"c".to_vec())).unwrap();
        assert!(store.get(&test_key(1)).is_some());
        assert!(store.get(&test_key(2)).is_none());
        assert!(store.get(&test_key(3)).is_some());
    }

    #[test]
    fn oversized_values_are_rejected() {
        let mut store = BoundedStore::with_config(
            PeerId::random(),
            BoundedConfig {
                max_value_bytes: 4,
                ..BoundedConfig::default()
            },
        );
        assert!(store.put(test_record(1, b"abc".to_vec())).is_ok());
        assert!(matches!(
            store.put(test_record(2, b"abcd".to_vec())),
            Err(Error::ValueTooLarge)
        ));
    }

    #[test]
    fn provider_list_ignores_overflow() {
        let local = PeerId::random();
        let mut store = BoundedStore::with_config(
            local,
            BoundedConfig {
                max_providers_per_key: 2,
                ..BoundedConfig::default()
            },
        );
        let key = test_key(9);
        for _ in 0..4 {
            store
                .add_provider(ProviderRecord {
                    key: key.clone(),
                    provider: PeerId::random(),
                    expires: None,
                    addresses: vec![],
                })
                .unwrap();
        }
        assert_eq!(store.providers(&key).len(), 2);
    }
}
