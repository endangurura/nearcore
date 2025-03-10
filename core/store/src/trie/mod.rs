use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::io::{Cursor, Read, Write};
use std::sync::Arc;

use borsh::{BorshDeserialize, BorshSerialize};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

use near_primitives::challenge::PartialState;
use near_primitives::contract::ContractCode;
use near_primitives::hash::{hash, CryptoHash};
pub use near_primitives::shard_layout::ShardUId;
use near_primitives::types::{StateRoot, StateRootNode};

use crate::trie::insert_delete::NodesStorage;
use crate::trie::iterator::TrieIterator;
use crate::trie::nibble_slice::NibbleSlice;
pub use crate::trie::shard_tries::{KeyForStateChanges, ShardTries, WrappedTrieChanges};
use crate::trie::trie_storage::{
    TouchedNodesCounter, TrieMemoryPartialStorage, TrieRecordingStorage, TrieStorage,
};
pub(crate) use crate::trie::trie_storage::{TrieCache, TrieCachingStorage};
use crate::StorageError;

mod insert_delete;
pub mod iterator;
mod nibble_slice;
mod shard_tries;
pub mod split_state;
mod state_parts;
mod trie_storage;
pub mod update;

#[cfg(test)]
mod trie_tests;

const POISONED_LOCK_ERR: &str = "The lock was poisoned.";

/// For fraud proofs
#[derive(Debug, Clone)]
pub struct PartialStorage {
    pub nodes: PartialState,
}

#[derive(Clone, Hash, Debug, Copy)]
pub(crate) struct StorageHandle(usize);

#[derive(Clone, Hash, Debug, Copy)]
pub(crate) struct StorageValueHandle(usize);

pub struct TrieCosts {
    pub byte_of_key: u64,
    pub byte_of_value: u64,
    pub node_cost: u64,
}

const TRIE_COSTS: TrieCosts = TrieCosts { byte_of_key: 2, byte_of_value: 1, node_cost: 50 };

#[derive(Clone, Hash, Debug)]
enum NodeHandle {
    InMemory(StorageHandle),
    Hash(CryptoHash),
}

impl NodeHandle {
    fn unwrap_hash(&self) -> &CryptoHash {
        match self {
            Self::Hash(hash) => hash,
            Self::InMemory(_) => unreachable!(),
        }
    }
}

#[derive(Clone, Hash, Debug)]
enum ValueHandle {
    InMemory(StorageValueHandle),
    HashAndSize(u32, CryptoHash),
}

#[derive(Clone, Hash, Debug)]
enum TrieNode {
    /// Null trie node. Could be an empty root or an empty branch entry.
    Empty,
    /// Key and value of the leaf node.
    Leaf(Vec<u8>, ValueHandle),
    /// Branch of 16 possible children and value if key ends here.
    Branch(Box<[Option<NodeHandle>; 16]>, Option<ValueHandle>),
    /// Key and child of extension.
    Extension(Vec<u8>, NodeHandle),
}

#[derive(Clone, Debug)]
pub struct TrieNodeWithSize {
    node: TrieNode,
    pub memory_usage: u64,
}

impl TrieNodeWithSize {
    fn from_raw(rc_node: RawTrieNodeWithSize) -> TrieNodeWithSize {
        TrieNodeWithSize { node: TrieNode::new(rc_node.node), memory_usage: rc_node.memory_usage }
    }

    fn new(node: TrieNode, memory_usage: u64) -> TrieNodeWithSize {
        TrieNodeWithSize { node, memory_usage }
    }

    fn memory_usage(&self) -> u64 {
        self.memory_usage
    }

    fn empty() -> TrieNodeWithSize {
        TrieNodeWithSize { node: TrieNode::Empty, memory_usage: 0 }
    }
}

impl TrieNode {
    fn new(rc_node: RawTrieNode) -> TrieNode {
        match rc_node {
            RawTrieNode::Leaf(key, value_length, value_hash) => {
                TrieNode::Leaf(key, ValueHandle::HashAndSize(value_length, value_hash))
            }
            RawTrieNode::Branch(children, value) => {
                let mut new_children: Box<[Option<NodeHandle>; 16]> = Default::default();
                for i in 0..children.len() {
                    new_children[i] = children[i].map(NodeHandle::Hash);
                }
                TrieNode::Branch(
                    new_children,
                    value.map(|(value_length, value_hash)| {
                        ValueHandle::HashAndSize(value_length, value_hash)
                    }),
                )
            }
            RawTrieNode::Extension(key, child) => TrieNode::Extension(key, NodeHandle::Hash(child)),
        }
    }

    fn print(
        &self,
        f: &mut dyn fmt::Write,
        memory: &NodesStorage,
        spaces: &mut String,
    ) -> fmt::Result {
        match self {
            TrieNode::Empty => {
                write!(f, "{}Empty", spaces)?;
            }
            TrieNode::Leaf(key, _value) => {
                let slice = NibbleSlice::from_encoded(key);
                write!(f, "{}Leaf({:?}, val)", spaces, slice.0)?;
            }
            TrieNode::Branch(children, value) => {
                writeln!(
                    f,
                    "{}Branch({}){{",
                    spaces,
                    if value.is_some() { "Some" } else { "None" }
                )?;
                spaces.push(' ');
                for (idx, child) in
                    children.iter().enumerate().filter(|(_idx, child)| child.is_some())
                {
                    let child = child.as_ref().unwrap();
                    write!(f, "{}{:01x}->", spaces, idx)?;
                    match child {
                        NodeHandle::Hash(hash) => {
                            write!(f, "{}", hash)?;
                        }
                        NodeHandle::InMemory(handle) => {
                            let child = &memory.node_ref(*handle).node;
                            child.print(f, memory, spaces)?;
                        }
                    }
                    writeln!(f)?;
                }
                spaces.remove(spaces.len() - 1);
                write!(f, "{}}}", spaces)?;
            }
            TrieNode::Extension(key, child) => {
                let slice = NibbleSlice::from_encoded(key);
                writeln!(f, "{}Extension({:?})", spaces, slice)?;
                spaces.push(' ');
                match child {
                    NodeHandle::Hash(hash) => {
                        write!(f, "{}{}", spaces, hash)?;
                    }
                    NodeHandle::InMemory(handle) => {
                        let child = &memory.node_ref(*handle).node;
                        child.print(f, memory, spaces)?;
                    }
                }
                writeln!(f)?;
                spaces.remove(spaces.len() - 1);
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn deep_to_string(&self, memory: &NodesStorage) -> String {
        let mut buf = String::new();
        self.print(&mut buf, memory, &mut "".to_string()).expect("printing failed");
        buf
    }

    fn memory_usage_for_value_length(value_length: u64) -> u64 {
        value_length * TRIE_COSTS.byte_of_value + TRIE_COSTS.node_cost
    }

    fn memory_usage_value(value: &ValueHandle, memory: Option<&NodesStorage>) -> u64 {
        let value_length = match value {
            ValueHandle::InMemory(handle) => memory
                .expect("InMemory nodes exist, but storage is not provided")
                .value_ref(*handle)
                .len() as u64,
            ValueHandle::HashAndSize(value_length, _value_hash) => *value_length as u64,
        };
        Self::memory_usage_for_value_length(value_length)
    }

    fn memory_usage_direct_no_memory(&self) -> u64 {
        self.memory_usage_direct_internal(None)
    }

    fn memory_usage_direct(&self, memory: &NodesStorage) -> u64 {
        self.memory_usage_direct_internal(Some(memory))
    }

    fn memory_usage_direct_internal(&self, memory: Option<&NodesStorage>) -> u64 {
        match self {
            TrieNode::Empty => {
                // DEVNOTE: empty nodes don't exist in storage.
                // In the in-memory implementation Some(TrieNode::Empty) and None are interchangeable as
                // children of branch nodes which means cost has to be 0
                0
            }
            TrieNode::Leaf(key, value) => {
                TRIE_COSTS.node_cost
                    + (key.len() as u64) * TRIE_COSTS.byte_of_key
                    + Self::memory_usage_value(value, memory)
            }
            TrieNode::Branch(_children, value) => {
                TRIE_COSTS.node_cost
                    + value.as_ref().map_or(0, |value| Self::memory_usage_value(value, memory))
            }
            TrieNode::Extension(key, _child) => {
                TRIE_COSTS.node_cost + (key.len() as u64) * TRIE_COSTS.byte_of_key
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
enum RawTrieNode {
    Leaf(Vec<u8>, u32, CryptoHash),
    Branch([Option<CryptoHash>; 16], Option<(u32, CryptoHash)>),
    Extension(Vec<u8>, CryptoHash),
}

/// Trie node + memory cost of its subtree
/// memory_usage is serialized, stored, and contributes to hash
#[derive(Debug, Eq, PartialEq)]
struct RawTrieNodeWithSize {
    node: RawTrieNode,
    memory_usage: u64,
}

const LEAF_NODE: u8 = 0;
const BRANCH_NODE_NO_VALUE: u8 = 1;
const BRANCH_NODE_WITH_VALUE: u8 = 2;
const EXTENSION_NODE: u8 = 3;

fn decode_children(cursor: &mut Cursor<&[u8]>) -> Result<[Option<CryptoHash>; 16], std::io::Error> {
    let mut children: [Option<CryptoHash>; 16] = Default::default();
    let bitmap = cursor.read_u16::<LittleEndian>()?;
    let mut pos = 1;
    for child in &mut children {
        if bitmap & pos != 0 {
            let mut arr = [0; 32];
            cursor.read_exact(&mut arr)?;
            *child = Some(CryptoHash::try_from(&arr[..]).unwrap());
        }
        pos <<= 1;
    }
    Ok(children)
}

impl RawTrieNode {
    fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), std::io::Error> {
        let mut cursor = Cursor::new(out);
        // size in state_parts = size + 8 for RawTrieNodeWithSize + 8 for borsh vector length
        match &self {
            // size <= 1 + 4 + 4 + 32 + key_length + value_length
            RawTrieNode::Leaf(key, value_length, value_hash) => {
                cursor.write_u8(LEAF_NODE)?;
                cursor.write_u32::<LittleEndian>(key.len() as u32)?;
                cursor.write_all(key)?;
                cursor.write_u32::<LittleEndian>(*value_length)?;
                cursor.write_all(value_hash.as_ref())?;
            }
            // size <= 1 + 4 + 32 + value_length + 2 + 32 * num_children
            RawTrieNode::Branch(children, value) => {
                if let Some((value_length, value_hash)) = value {
                    cursor.write_u8(BRANCH_NODE_WITH_VALUE)?;
                    cursor.write_u32::<LittleEndian>(*value_length)?;
                    cursor.write_all(value_hash.as_ref())?;
                } else {
                    cursor.write_u8(BRANCH_NODE_NO_VALUE)?;
                }
                let mut bitmap: u16 = 0;
                let mut pos: u16 = 1;
                for child in children.iter() {
                    if child.is_some() {
                        bitmap |= pos
                    }
                    pos <<= 1;
                }
                cursor.write_u16::<LittleEndian>(bitmap)?;
                for child in children.iter() {
                    if let Some(hash) = child {
                        cursor.write_all(hash.as_ref())?;
                    }
                }
            }
            // size <= 1 + 4 + key_length + 32
            RawTrieNode::Extension(key, child) => {
                cursor.write_u8(EXTENSION_NODE)?;
                cursor.write_u32::<LittleEndian>(key.len() as u32)?;
                cursor.write_all(key)?;
                cursor.write_all(child.as_ref())?;
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn encode(&self) -> Result<Vec<u8>, std::io::Error> {
        let mut out = Vec::new();
        self.encode_into(&mut out)?;
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self, std::io::Error> {
        let mut cursor = Cursor::new(bytes);
        match cursor.read_u8()? {
            LEAF_NODE => {
                let key_length = cursor.read_u32::<LittleEndian>()?;
                let mut key = vec![0; key_length as usize];
                cursor.read_exact(&mut key)?;
                let value_length = cursor.read_u32::<LittleEndian>()?;
                let mut arr = [0; 32];
                cursor.read_exact(&mut arr)?;
                let value_hash = CryptoHash(arr);
                Ok(RawTrieNode::Leaf(key, value_length, value_hash))
            }
            BRANCH_NODE_NO_VALUE => {
                let children = decode_children(&mut cursor)?;
                Ok(RawTrieNode::Branch(children, None))
            }
            BRANCH_NODE_WITH_VALUE => {
                let value_length = cursor.read_u32::<LittleEndian>()?;
                let mut arr = [0; 32];
                cursor.read_exact(&mut arr)?;
                let value_hash = CryptoHash(arr);
                let children = decode_children(&mut cursor)?;
                Ok(RawTrieNode::Branch(children, Some((value_length, value_hash))))
            }
            EXTENSION_NODE => {
                let key_length = cursor.read_u32::<LittleEndian>()?;
                let mut key = vec![0; key_length as usize];
                cursor.read_exact(&mut key)?;
                let mut child = [0; 32];
                cursor.read_exact(&mut child)?;
                Ok(RawTrieNode::Extension(key, CryptoHash(child)))
            }
            _ => Err(std::io::Error::new(std::io::ErrorKind::Other, "Wrong type")),
        }
    }
}

impl RawTrieNodeWithSize {
    fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), std::io::Error> {
        self.node.encode_into(out)?;
        out.write_u64::<LittleEndian>(self.memory_usage)
    }

    #[allow(dead_code)]
    fn encode(&self) -> Result<Vec<u8>, std::io::Error> {
        let mut out = Vec::new();
        self.encode_into(&mut out)?;
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self, std::io::Error> {
        if bytes.len() < 8 {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Wrong type"));
        }
        let node = RawTrieNode::decode(&bytes[0..bytes.len() - 8])?;
        let mut arr: [u8; 8] = Default::default();
        arr.copy_from_slice(&bytes[bytes.len() - 8..]);
        let memory_usage = u64::from_le_bytes(arr);
        Ok(RawTrieNodeWithSize { node, memory_usage })
    }
}

pub struct Trie {
    pub(crate) storage: Box<dyn TrieStorage>,
    pub counter: TouchedNodesCounter,
}

/// Stores reference count change for some key-value pair in DB.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct TrieRefcountChange {
    /// Hash of trie_node_or_value and part of the DB key.
    /// Used for uniting with shard id to get actual DB key.
    trie_node_or_value_hash: CryptoHash,
    /// DB value. Can be either serialized RawTrieNodeWithSize or value corresponding to
    /// some TrieKey.
    trie_node_or_value: Vec<u8>,
    /// Reference count difference which will be added to the total refcount if it corresponds to
    /// insertion and subtracted from it in the case of deletion.
    rc: u32,
}

///
/// TrieChanges stores delta for refcount.
/// Multiple versions of the state work the following way:
///         __changes1___state1
/// state0 /
///        \__changes2___state2
///
/// To store state0, state1 and state2, apply insertions from changes1 and changes2
///
/// Then, to discard state2, apply insertions from changes2 as deletions
///
/// Then, to discard state0, apply deletions from changes1.
/// deleting state0 while both state1 and state2 exist is not possible.
/// Applying deletions from changes1 while state2 exists makes accessing state2 invalid.
///
///
/// create a fork -> apply insertions
/// resolve a fork -> apply opposite of insertions
/// discard old parent which has no forks from it -> apply deletions
///
/// Having old_root and values in deletions allows to apply TrieChanges in reverse
///
/// StoreUpdate are the changes from current state refcount to refcount + delta.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct TrieChanges {
    pub old_root: StateRoot,
    pub new_root: StateRoot,
    insertions: Vec<TrieRefcountChange>,
    deletions: Vec<TrieRefcountChange>,
}

impl TrieChanges {
    pub fn empty(old_root: StateRoot) -> Self {
        TrieChanges { old_root, new_root: old_root, insertions: vec![], deletions: vec![] }
    }
}

/// Result of applying state part to Trie.
pub struct ApplyStatePartResult {
    /// Trie changes after applying state part.
    pub trie_changes: TrieChanges,
    /// Contract codes belonging to the state part.
    pub contract_codes: Vec<ContractCode>,
}

impl Trie {
    pub fn new(store: Box<dyn TrieStorage>, _shard_uid: ShardUId) -> Self {
        Trie { storage: store, counter: TouchedNodesCounter::default() }
    }

    pub fn recording_reads(&self) -> Self {
        let storage =
            self.storage.as_caching_storage().expect("Storage should be TrieCachingStorage");
        let storage = TrieRecordingStorage {
            store: storage.store.clone(),
            shard_uid: storage.shard_uid,
            recorded: RefCell::new(Default::default()),
        };
        Trie { storage: Box::new(storage), counter: TouchedNodesCounter::default() }
    }

    pub fn empty_root() -> StateRoot {
        StateRoot::default()
    }

    pub fn recorded_storage(&self) -> Option<PartialStorage> {
        let storage = self.storage.as_recording_storage()?;
        let mut nodes: Vec<_> =
            storage.recorded.borrow_mut().drain().map(|(_key, value)| value).collect();
        nodes.sort();
        Some(PartialStorage { nodes: PartialState(nodes) })
    }

    pub fn from_recorded_storage(partial_storage: PartialStorage) -> Self {
        let recorded_storage =
            partial_storage.nodes.0.into_iter().map(|value| (hash(&value), value)).collect();
        Trie {
            storage: Box::new(TrieMemoryPartialStorage {
                recorded_storage,
                visited_nodes: Default::default(),
            }),
            counter: TouchedNodesCounter::default(),
        }
    }

    #[cfg(test)]
    fn memory_usage_verify(&self, memory: &NodesStorage, handle: NodeHandle) -> u64 {
        if self.storage.as_recording_storage().is_some() {
            return 0;
        }
        let TrieNodeWithSize { node, memory_usage } = match handle {
            NodeHandle::InMemory(h) => memory.node_ref(h).clone(),
            NodeHandle::Hash(h) => self.retrieve_node(&h).expect("storage failure"),
        };

        let mut memory_usage_naive = node.memory_usage_direct(memory);
        match &node {
            TrieNode::Empty => {}
            TrieNode::Leaf(_key, _value) => {}
            TrieNode::Branch(children, _value) => {
                memory_usage_naive += children
                    .iter()
                    .filter_map(Option::as_ref)
                    .map(|handle| self.memory_usage_verify(memory, handle.clone()))
                    .sum::<u64>();
            }
            TrieNode::Extension(_key, child) => {
                memory_usage_naive += self.memory_usage_verify(memory, child.clone());
            }
        };
        if memory_usage_naive != memory_usage {
            eprintln!("Incorrectly calculated memory usage");
            eprintln!("Correct is {}", memory_usage_naive);
            eprintln!("Computed is {}", memory_usage);
            match handle {
                NodeHandle::InMemory(h) => {
                    eprintln!("TRIE!!!!");
                    eprintln!("{}", memory.node_ref(h).node.deep_to_string(memory));
                }
                NodeHandle::Hash(_h) => {
                    eprintln!("Bad node in storage!");
                }
            };
            assert_eq!(memory_usage_naive, memory_usage);
        }
        memory_usage
    }

    fn delete_value(
        &self,
        memory: &mut NodesStorage,
        value: &ValueHandle,
    ) -> Result<(), StorageError> {
        match value {
            ValueHandle::HashAndSize(_, hash) => {
                let bytes = self.storage.retrieve_raw_bytes(hash)?;
                memory.refcount_changes.entry(*hash).or_insert_with(|| (bytes.to_vec(), 0)).1 -= 1;
            }
            ValueHandle::InMemory(_) => {
                // do nothing
            }
        }
        Ok(())
    }

    fn move_node_to_mutable(
        &self,
        memory: &mut NodesStorage,
        hash: &CryptoHash,
    ) -> Result<StorageHandle, StorageError> {
        if *hash == Trie::empty_root() {
            Ok(memory.store(TrieNodeWithSize::empty()))
        } else {
            self.counter.increment();
            let bytes = self.storage.retrieve_raw_bytes(hash)?;
            match RawTrieNodeWithSize::decode(&bytes) {
                Ok(value) => {
                    let result = memory.store(TrieNodeWithSize::from_raw(value));
                    memory
                        .refcount_changes
                        .entry(*hash)
                        .or_insert_with(|| (bytes.to_vec(), 0))
                        .1 -= 1;
                    Ok(result)
                }
                Err(_) => Err(StorageError::StorageInconsistentState(format!(
                    "Failed to decode node {}",
                    hash
                ))),
            }
        }
    }

    fn retrieve_node(&self, hash: &CryptoHash) -> Result<TrieNodeWithSize, StorageError> {
        if *hash == Trie::empty_root() {
            return Ok(TrieNodeWithSize::empty());
        }
        let bytes = self.retrieve_raw_bytes(hash)?;
        match RawTrieNodeWithSize::decode(&bytes) {
            Ok(value) => Ok(TrieNodeWithSize::from_raw(value)),
            Err(_) => Err(StorageError::StorageInconsistentState(format!(
                "Failed to decode node {}",
                hash
            ))),
        }
    }

    pub(crate) fn retrieve_raw_bytes(&self, hash: &CryptoHash) -> Result<Arc<[u8]>, StorageError> {
        self.counter.increment();
        self.storage.retrieve_raw_bytes(hash)
    }

    pub fn retrieve_root_node(&self, root: &StateRoot) -> Result<StateRootNode, StorageError> {
        if *root == Trie::empty_root() {
            return Ok(StateRootNode::empty());
        }
        let data = self.retrieve_raw_bytes(root)?;
        match RawTrieNodeWithSize::decode(&data) {
            Ok(value) => {
                let memory_usage = TrieNodeWithSize::from_raw(value).memory_usage;
                Ok(StateRootNode { data: data.to_vec(), memory_usage })
            }
            Err(_) => Err(StorageError::StorageInconsistentState(format!(
                "Failed to decode node {}",
                root
            ))),
        }
    }

    fn lookup(
        &self,
        root: &CryptoHash,
        mut key: NibbleSlice<'_>,
    ) -> Result<Option<(u32, CryptoHash)>, StorageError> {
        let mut hash = *root;

        loop {
            if hash == Trie::empty_root() {
                return Ok(None);
            }
            let bytes = self.retrieve_raw_bytes(&hash)?;
            let node = RawTrieNodeWithSize::decode(&bytes).map_err(|_| {
                StorageError::StorageInconsistentState("RawTrieNode decode failed".to_string())
            })?;

            match node.node {
                RawTrieNode::Leaf(existing_key, value_length, value_hash) => {
                    if NibbleSlice::from_encoded(&existing_key).0 == key {
                        return Ok(Some((value_length, value_hash)));
                    } else {
                        return Ok(None);
                    }
                }
                RawTrieNode::Extension(existing_key, child) => {
                    let existing_key = NibbleSlice::from_encoded(&existing_key).0;
                    if key.starts_with(&existing_key) {
                        hash = child;
                        key = key.mid(existing_key.len());
                    } else {
                        return Ok(None);
                    }
                }
                RawTrieNode::Branch(mut children, value) => {
                    if key.is_empty() {
                        match value {
                            Some((value_length, value_hash)) => {
                                return Ok(Some((value_length, value_hash)));
                            }
                            None => return Ok(None),
                        }
                    } else {
                        match children[key.at(0) as usize].take() {
                            Some(x) => {
                                hash = x;
                                key = key.mid(1);
                            }
                            None => return Ok(None),
                        }
                    }
                }
            };
        }
    }

    pub fn get_ref(
        &self,
        root: &CryptoHash,
        key: &[u8],
    ) -> Result<Option<(u32, CryptoHash)>, StorageError> {
        let key = NibbleSlice::new(key);
        self.lookup(root, key)
    }

    pub fn get(&self, root: &CryptoHash, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        match self.get_ref(root, key)? {
            Some((_length, hash)) => {
                self.retrieve_raw_bytes(&hash).map(|bytes| Some(bytes.to_vec()))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn convert_to_insertions_and_deletions(
        changes: HashMap<CryptoHash, (Vec<u8>, i32)>,
    ) -> (Vec<TrieRefcountChange>, Vec<TrieRefcountChange>) {
        let mut deletions = Vec::new();
        let mut insertions = Vec::new();
        for (trie_node_or_value_hash, (trie_node_or_value, rc)) in changes.into_iter() {
            match rc.cmp(&0) {
                Ordering::Greater => insertions.push(TrieRefcountChange {
                    trie_node_or_value_hash,
                    trie_node_or_value,
                    rc: rc as u32,
                }),
                Ordering::Less => deletions.push(TrieRefcountChange {
                    trie_node_or_value_hash,
                    trie_node_or_value,
                    rc: (-rc) as u32,
                }),
                Ordering::Equal => {}
            }
        }
        // Sort so that trie changes have unique representation
        insertions.sort();
        deletions.sort();
        (insertions, deletions)
    }

    pub fn update<I>(&self, root: &CryptoHash, changes: I) -> Result<TrieChanges, StorageError>
    where
        I: Iterator<Item = (Vec<u8>, Option<Vec<u8>>)>,
    {
        let mut memory = NodesStorage::new();
        let mut root_node = self.move_node_to_mutable(&mut memory, root)?;
        for (key, value) in changes {
            let key = NibbleSlice::new(&key);
            match value {
                Some(arr) => {
                    root_node = self.insert(&mut memory, root_node, key, arr)?;
                }
                None => {
                    root_node = self.delete(&mut memory, root_node, key)?;
                }
            }
        }

        #[cfg(test)]
        {
            self.memory_usage_verify(&memory, NodeHandle::InMemory(root_node));
        }
        Trie::flatten_nodes(root, memory, root_node)
    }

    pub fn iter<'a>(&'a self, root: &CryptoHash) -> Result<TrieIterator<'a>, StorageError> {
        TrieIterator::new(self, root)
    }
}

#[cfg(test)]
mod tests {
    use rand::Rng;

    use crate::db::DBCol::ColState;
    use crate::test_utils::{
        create_test_store, create_tries, create_tries_complex, gen_changes, simplify_changes,
        test_populate_trie,
    };

    use super::*;

    type TrieChanges = Vec<(Vec<u8>, Option<Vec<u8>>)>;
    const SHARD_VERSION: u32 = 1;

    fn test_clear_trie(
        tries: &ShardTries,
        root: &CryptoHash,
        shard_uid: ShardUId,
        changes: TrieChanges,
    ) -> CryptoHash {
        let trie = tries.get_trie_for_shard(shard_uid);
        let delete_changes: TrieChanges =
            changes.iter().map(|(key, _)| (key.clone(), None)).collect();
        let mut other_delete_changes = delete_changes.clone();
        let trie_changes = trie.update(root, other_delete_changes.drain(..)).unwrap();
        let (store_update, root) = tries.apply_all(&trie_changes, shard_uid).unwrap();
        store_update.commit().unwrap();
        for (key, _) in delete_changes {
            assert_eq!(trie.get(&root, &key), Ok(None));
        }
        root
    }

    #[test]
    fn test_encode_decode() {
        let value = vec![123, 245, 255];
        let value_length = 3;
        let value_hash = hash(&value);
        let node = RawTrieNode::Leaf(vec![1, 2, 3], value_length, value_hash);
        let buf = node.encode().expect("Failed to serialize");
        let new_node = RawTrieNode::decode(&buf).expect("Failed to deserialize");
        assert_eq!(node, new_node);

        let mut children: [Option<CryptoHash>; 16] = Default::default();
        children[3] = Some(CryptoHash::default());
        let node = RawTrieNode::Branch(children, Some((value_length, value_hash)));
        let buf = node.encode().expect("Failed to serialize");
        let new_node = RawTrieNode::decode(&buf).expect("Failed to deserialize");
        assert_eq!(node, new_node);

        let node = RawTrieNode::Extension(vec![123, 245, 255], CryptoHash::default());
        let buf = node.encode().expect("Failed to serialize");
        let new_node = RawTrieNode::decode(&buf).expect("Failed to deserialize");
        assert_eq!(node, new_node);
    }

    #[test]
    fn test_basic_trie() {
        // test trie version > 0
        let tries = create_tries_complex(SHARD_VERSION, 2);
        let shard_uid = ShardUId { version: SHARD_VERSION, shard_id: 0 };
        let trie = tries.get_trie_for_shard(shard_uid);
        let empty_root = Trie::empty_root();
        assert_eq!(trie.get(&empty_root, &[122]), Ok(None));
        let changes = vec![
            (b"doge".to_vec(), Some(b"coin".to_vec())),
            (b"docu".to_vec(), Some(b"value".to_vec())),
            (b"do".to_vec(), Some(b"verb".to_vec())),
            (b"horse".to_vec(), Some(b"stallion".to_vec())),
            (b"dog".to_vec(), Some(b"puppy".to_vec())),
            (b"h".to_vec(), Some(b"value".to_vec())),
        ];
        let root = test_populate_trie(&tries, &empty_root, shard_uid, changes.clone());
        let new_root = test_clear_trie(&tries, &root, shard_uid, changes);
        assert_eq!(new_root, empty_root);
        assert_eq!(trie.iter(&new_root).unwrap().fold(0, |acc, _| acc + 1), 0);
    }

    #[test]
    fn test_trie_iter() {
        let tries = create_tries_complex(SHARD_VERSION, 2);
        let shard_uid = ShardUId { version: SHARD_VERSION, shard_id: 0 };
        let trie = tries.get_trie_for_shard(shard_uid);
        let pairs = vec![
            (b"a".to_vec(), Some(b"111".to_vec())),
            (b"b".to_vec(), Some(b"222".to_vec())),
            (b"x".to_vec(), Some(b"333".to_vec())),
            (b"y".to_vec(), Some(b"444".to_vec())),
        ];
        let root = test_populate_trie(&tries, &Trie::empty_root(), shard_uid, pairs.clone());
        let mut iter_pairs = vec![];
        for pair in trie.iter(&root).unwrap() {
            let (key, value) = pair.unwrap();
            iter_pairs.push((key, Some(value.to_vec())));
        }
        assert_eq!(pairs, iter_pairs);

        let mut other_iter = trie.iter(&root).unwrap();
        other_iter.seek(b"r").unwrap();
        assert_eq!(other_iter.next().unwrap().unwrap().0, b"x".to_vec());
    }

    #[test]
    fn test_trie_leaf_into_branch() {
        let tries = create_tries_complex(SHARD_VERSION, 2);
        let shard_uid = ShardUId { version: SHARD_VERSION, shard_id: 0 };
        let changes = vec![
            (b"dog".to_vec(), Some(b"puppy".to_vec())),
            (b"dog2".to_vec(), Some(b"puppy".to_vec())),
            (b"xxx".to_vec(), Some(b"puppy".to_vec())),
        ];
        test_populate_trie(&tries, &Trie::empty_root(), shard_uid, changes);
    }

    #[test]
    fn test_trie_same_node() {
        let tries = create_tries();
        let changes = vec![
            (b"dogaa".to_vec(), Some(b"puppy".to_vec())),
            (b"dogbb".to_vec(), Some(b"puppy".to_vec())),
            (b"cataa".to_vec(), Some(b"puppy".to_vec())),
            (b"catbb".to_vec(), Some(b"puppy".to_vec())),
            (b"dogax".to_vec(), Some(b"puppy".to_vec())),
        ];
        test_populate_trie(&tries, &Trie::empty_root(), ShardUId::single_shard(), changes);
    }

    #[test]
    fn test_trie_iter_seek_stop_at_extension() {
        let tries = create_tries();
        let trie = tries.get_trie_for_shard(ShardUId::single_shard());
        let changes = vec![
            (vec![0, 116, 101, 115, 116], Some(vec![0])),
            (vec![2, 116, 101, 115, 116], Some(vec![0])),
            (
                vec![
                    0, 116, 101, 115, 116, 44, 98, 97, 108, 97, 110, 99, 101, 115, 58, 98, 111, 98,
                    46, 110, 101, 97, 114,
                ],
                Some(vec![0]),
            ),
            (
                vec![
                    0, 116, 101, 115, 116, 44, 98, 97, 108, 97, 110, 99, 101, 115, 58, 110, 117,
                    108, 108,
                ],
                Some(vec![0]),
            ),
        ];
        let root =
            test_populate_trie(&tries, &Trie::empty_root(), ShardUId::single_shard(), changes);
        let mut iter = trie.iter(&root).unwrap();
        iter.seek(&vec![0, 116, 101, 115, 116, 44]).unwrap();
        let mut pairs = vec![];
        for pair in iter {
            pairs.push(pair.unwrap().0);
        }
        assert_eq!(
            pairs[..2],
            [
                vec![
                    0, 116, 101, 115, 116, 44, 98, 97, 108, 97, 110, 99, 101, 115, 58, 98, 111, 98,
                    46, 110, 101, 97, 114
                ],
                vec![
                    0, 116, 101, 115, 116, 44, 98, 97, 108, 97, 110, 99, 101, 115, 58, 110, 117,
                    108, 108
                ],
            ]
        );
    }

    #[test]
    fn test_trie_remove_non_existent_key() {
        let tries = create_tries();
        let trie = tries.get_trie_for_shard(ShardUId::single_shard());
        let initial = vec![
            (vec![99, 44, 100, 58, 58, 49], Some(vec![1])),
            (vec![99, 44, 100, 58, 58, 50], Some(vec![1])),
            (vec![99, 44, 100, 58, 58, 50, 51], Some(vec![1])),
        ];
        let root =
            test_populate_trie(&tries, &Trie::empty_root(), ShardUId::single_shard(), initial);

        let changes = vec![
            (vec![99, 44, 100, 58, 58, 45, 49], None),
            (vec![99, 44, 100, 58, 58, 50, 52], None),
        ];
        let root = test_populate_trie(&tries, &root, ShardUId::single_shard(), changes);
        for r in trie.iter(&root).unwrap() {
            r.unwrap();
        }
    }

    #[test]
    fn test_equal_leafs() {
        let tries = create_tries();
        let trie = tries.get_trie_for_shard(ShardUId::single_shard());
        let initial = vec![
            (vec![1, 2, 3], Some(vec![1])),
            (vec![2, 2, 3], Some(vec![1])),
            (vec![3, 2, 3], Some(vec![1])),
        ];
        let root =
            test_populate_trie(&tries, &Trie::empty_root(), ShardUId::single_shard(), initial);
        for r in trie.iter(&root).unwrap() {
            r.unwrap();
        }

        let changes = vec![(vec![1, 2, 3], None)];
        let root = test_populate_trie(&tries, &root, ShardUId::single_shard(), changes);
        for r in trie.iter(&root).unwrap() {
            r.unwrap();
        }
    }

    #[test]
    fn test_trie_unique() {
        let mut rng = rand::thread_rng();
        for _ in 0..100 {
            let tries = create_tries();
            let trie = tries.get_trie_for_shard(ShardUId::single_shard());
            let trie_changes = gen_changes(&mut rng, 20);
            let simplified_changes = simplify_changes(&trie_changes);

            let trie_changes1 =
                trie.update(&Trie::empty_root(), trie_changes.iter().cloned()).unwrap();
            let trie_changes2 =
                trie.update(&Trie::empty_root(), simplified_changes.iter().cloned()).unwrap();
            if trie_changes1.new_root != trie_changes2.new_root {
                eprintln!("{:?}", trie_changes);
                eprintln!("{:?}", simplified_changes);
                eprintln!("root1: {:?}", trie_changes1.new_root);
                eprintln!("root2: {:?}", trie_changes2.new_root);
                panic!("MISMATCH!");
            }
            // TODO: compare state updates?
        }
    }

    #[test]
    fn test_iterator_seek() {
        let mut rng = rand::thread_rng();
        for _test_run in 0..10 {
            let tries = create_tries();
            let trie = tries.get_trie_for_shard(ShardUId::single_shard());
            let trie_changes = gen_changes(&mut rng, 500);

            let state_root = test_populate_trie(
                &tries,
                &Trie::empty_root(),
                ShardUId::single_shard(),
                trie_changes.clone(),
            );
            let queries = gen_changes(&mut rng, 500).into_iter().map(|(key, _)| key);
            for query in queries {
                let mut iterator = trie.iter(&state_root).unwrap();
                iterator.seek(&query).unwrap();
                if let Some(Ok((key, _))) = iterator.next() {
                    assert!(key >= query);
                }
            }
        }
    }

    #[test]
    fn test_refcounts() {
        let mut rng = rand::thread_rng();
        for _test_run in 0..10 {
            let num_iterations = rng.gen_range(1, 20);
            let tries = create_tries();
            let trie = tries.get_trie_for_shard(ShardUId::single_shard());
            let mut state_root = Trie::empty_root();
            for _ in 0..num_iterations {
                let trie_changes = gen_changes(&mut rng, 20);
                state_root =
                    test_populate_trie(&tries, &state_root, ShardUId::single_shard(), trie_changes);
                println!(
                    "New memory_usage: {}",
                    trie.retrieve_root_node(&state_root).unwrap().memory_usage
                );
            }
            {
                let trie_changes = trie
                    .iter(&state_root)
                    .unwrap()
                    .map(|item| {
                        let (key, _) = item.unwrap();
                        (key, None)
                    })
                    .collect::<Vec<_>>();
                state_root =
                    test_populate_trie(&tries, &state_root, ShardUId::single_shard(), trie_changes);
                assert_eq!(state_root, Trie::empty_root(), "Trie must be empty");
                assert!(
                    trie.storage
                        .as_caching_storage()
                        .unwrap()
                        .store
                        .iter(ColState)
                        .peekable()
                        .peek()
                        .is_none(),
                    "Storage must be empty"
                );
            }
        }
    }

    #[test]
    fn test_trie_restart() {
        let store = create_test_store();
        let tries = ShardTries::new(store.clone(), 0, 1);
        let empty_root = Trie::empty_root();
        let changes = vec![
            (b"doge".to_vec(), Some(b"coin".to_vec())),
            (b"docu".to_vec(), Some(b"value".to_vec())),
            (b"do".to_vec(), Some(b"verb".to_vec())),
            (b"horse".to_vec(), Some(b"stallion".to_vec())),
            (b"dog".to_vec(), Some(b"puppy".to_vec())),
            (b"h".to_vec(), Some(b"value".to_vec())),
        ];
        let root = test_populate_trie(&tries, &empty_root, ShardUId::single_shard(), changes);

        let tries2 = ShardTries::new(store, 0, 1);
        let trie2 = tries2.get_trie_for_shard(ShardUId::single_shard());
        assert_eq!(trie2.get(&root, b"doge"), Ok(Some(b"coin".to_vec())));
    }

    // TODO: somehow also test that we don't record unnecessary nodes
    #[test]
    fn test_trie_recording_reads() {
        let store = create_test_store();
        let tries = ShardTries::new(store, 0, 1);
        let empty_root = Trie::empty_root();
        let changes = vec![
            (b"doge".to_vec(), Some(b"coin".to_vec())),
            (b"docu".to_vec(), Some(b"value".to_vec())),
            (b"do".to_vec(), Some(b"verb".to_vec())),
            (b"horse".to_vec(), Some(b"stallion".to_vec())),
            (b"dog".to_vec(), Some(b"puppy".to_vec())),
            (b"h".to_vec(), Some(b"value".to_vec())),
        ];
        let root = test_populate_trie(&tries, &empty_root, ShardUId::single_shard(), changes);

        let trie2 = tries.get_trie_for_shard(ShardUId::single_shard()).recording_reads();
        trie2.get(&root, b"dog").unwrap();
        trie2.get(&root, b"horse").unwrap();
        let partial_storage = trie2.recorded_storage();

        let trie3 = Trie::from_recorded_storage(partial_storage.unwrap());

        assert_eq!(trie3.get(&root, b"dog"), Ok(Some(b"puppy".to_vec())));
        assert_eq!(trie3.get(&root, b"horse"), Ok(Some(b"stallion".to_vec())));
        assert_eq!(trie3.get(&root, b"doge"), Err(StorageError::TrieNodeMissing));
    }

    #[test]
    fn test_trie_recording_reads_update() {
        let store = create_test_store();
        let tries = ShardTries::new(store, 0, 1);
        let empty_root = Trie::empty_root();
        let changes = vec![
            (b"doge".to_vec(), Some(b"coin".to_vec())),
            (b"docu".to_vec(), Some(b"value".to_vec())),
        ];
        let root = test_populate_trie(&tries, &empty_root, ShardUId::single_shard(), changes);
        // Trie: extension -> branch -> 2 leaves
        {
            let trie2 = tries.get_trie_for_shard(ShardUId::single_shard()).recording_reads();
            trie2.get(&root, b"doge").unwrap();
            // record extension, branch and one leaf with value, but not the other
            assert_eq!(trie2.recorded_storage().unwrap().nodes.0.len(), 4);
        }

        {
            let trie2 = tries.get_trie_for_shard(ShardUId::single_shard()).recording_reads();
            let updates = vec![(b"doge".to_vec(), None)];
            trie2.update(&root, updates.into_iter()).unwrap();
            // record extension, branch and both leaves (one with value)
            assert_eq!(trie2.recorded_storage().unwrap().nodes.0.len(), 5);
        }

        {
            let trie2 = tries.get_trie_for_shard(ShardUId::single_shard()).recording_reads();
            let updates = vec![(b"dodo".to_vec(), Some(b"asdf".to_vec()))];
            trie2.update(&root, updates.into_iter()).unwrap();
            // record extension and branch, but not leaves
            assert_eq!(trie2.recorded_storage().unwrap().nodes.0.len(), 2);
        }
    }

    #[test]
    fn test_dump_load_trie() {
        let store = create_test_store();
        let tries = ShardTries::new(store.clone(), 0, 1);
        let empty_root = Trie::empty_root();
        let changes = vec![
            (b"doge".to_vec(), Some(b"coin".to_vec())),
            (b"docu".to_vec(), Some(b"value".to_vec())),
        ];
        let root = test_populate_trie(&tries, &empty_root, ShardUId::single_shard(), changes);
        let dir = tempfile::Builder::new().prefix("test_dump_load_trie").tempdir().unwrap();
        store.save_to_file(ColState, &dir.path().join("test.bin")).unwrap();
        let store2 = create_test_store();
        store2.load_from_file(ColState, &dir.path().join("test.bin")).unwrap();
        let tries2 = ShardTries::new(store2, 0, 1);
        let trie2 = tries2.get_trie_for_shard(ShardUId::single_shard());
        assert_eq!(trie2.get(&root, b"doge").unwrap().unwrap(), b"coin");
    }
}
