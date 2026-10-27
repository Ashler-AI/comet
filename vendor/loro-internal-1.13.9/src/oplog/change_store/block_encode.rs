//! # Encode
//!
//! ```log
//! ≈4KB after compression
//!
//!  N = Number of Changes
//!
//!  Peer_1 = This Peer
//!
//!
//! ┌────────────────────────┬─────────────────────────────────────┐
//! │ LEB Counter Start&Len  │  LEB Lamport Start&Len              │◁───┐
//! └────────────────────────┴─────────────────────────────────────┘    │
//! ┌──────────────┬──────────────┬────────────────────────────────┐    │
//! │    LEB N     │ LEB Peer Num │      8B Peer_1,...,Peer_x      │◁───┤
//! └──────────────┴──────────────┴────────────────────────────────┘    │
//! ┌──────────────────────────────────────────────────────────────┐    │
//! │                   N LEB128 Change AtomLen                    │◁───┼─────  Important metadata
//! └──────────────────────────────────────────────────────────────┘    │
//! ┌───────────────────┬────────────────────────┬─────────────────┐    │
//! │N DepOnSelf BoolRle│ N Delta Rle Deps Lens  │     Dep IDs     │◁───┤
//! └───────────────────┴────────────────────────┴─────────────────┘    │
//! ┌──────────────────────────────────────────────────────────────┐    │
//! │                   N LEB128 Delta Lamports                    │◁───┘
//! └──────────────────────────────────────────────────────────────┘
//! ┌──────────────────────────────────────────────────────────────┐
//! │               N LEB128 DeltaOfDelta Timestamps               │
//! └──────────────────────────────────────────────────────────────┘
//! ┌────────────────────────────────┬─────────────────────────────┐
//! │    N Rle Commit Msg Lengths    │       Commit Messages       │
//! └────────────────────────────────┴─────────────────────────────┘
//!
//!  ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ Encoded Operations ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─
//!
//! ┌────────────────────┬─────────────────────────────────────────┐
//! │ ContainerIDs Size  │             ContainerIDs                │
//! └────────────────────┴─────────────────────────────────────────┘
//! ┌────────────────────┬─────────────────────────────────────────┐
//! │  Key Strings Size  │               Key Strings               │
//! └────────────────────┴─────────────────────────────────────────┘
//! ┌────────────────────┬─────────────────────────────────────────┐
//! │  Position Size     │               Position                  │
//! └────────────────────┴─────────────────────────────────────────┘
//! ┌────────┬──────────┬──────────┬───────┬───────────────────────┐
//! │        │          │          │       │                       │
//! │        │          │          │       │                       │
//! │        │          │          │       │                       │
//! │  Ops   │  LEB128  │   RLE    │ Delta │                       │
//! │  Size  │ Lengths  │Containers│  RLE  │       ValueType       │
//! │        │          │          │ Props │                       │
//! │        │          │          │       │                       │
//! │        │          │          │       │                       │
//! │        │          │          │       │                       │
//! └────────┴──────────┴──────────┴───────┴───────────────────────┘
//! ┌──────────────────────────────────────────────────────────────┐
//! │             (Encoded with Ops By serde_columnar)             │
//! │                       Delete Start IDs                       │
//! │                                                              │
//! └──────────────────────────────────────────────────────────────┘
//! ┌────────────────┬─────────────────────────────────────────────┐
//! │                │                                             │
//! │Value Bytes Size│                Value Bytes                  │
//! │                │                                             │
//! └────────────────┴─────────────────────────────────────────────┘
//! ```

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::io::Write;
use std::sync::Arc;

use fractional_index::FractionalIndex;
use loro_common::{
    ContainerID, Counter, HasCounterSpan, HasLamportSpan, InternalString, Lamport, LoroError,
    IdSpan, LoroResult, PeerID, TreeID, ID,
};
use once_cell::sync::OnceCell;
use rle::{HasLength, Sliceable};
use serde::{Deserialize, Serialize};
use serde_columnar::{columnar, AnyRleDecoder, DeltaOfDeltaDecoder, Itertools};
use tracing::info;

use super::block_meta_encode::decode_changes_header;
use crate::arena::SharedArena;
use crate::change::{Change, Timestamp};
use crate::change_meta::ChangeMeta;
use crate::encoding::json_schema::{encode_op as encode_json_op, json::JsonOp};
use crate::container::tree::tree_op;
use crate::encoding::arena::{ContainerArena, PositionArena};
use crate::encoding::value_register::ValueRegister;
use crate::encoding::{
    self, decode_op, encode_op, get_op_prop, EncodedDeleteStartId, IterableEncodedDeleteStartId,
};
use crate::op::Op;

#[derive(Debug, Serialize, Deserialize)]
struct EncodedBlock<'a> {
    counter_start: u32,
    counter_len: u32,
    lamport_start: u32,
    lamport_len: u32,
    n_changes: u32,
    #[serde(borrow)]
    header: Cow<'a, [u8]>,
    // timestamp and commit messages
    #[serde(borrow)]
    change_meta: Cow<'a, [u8]>,
    // ---------------------- Ops ----------------------
    #[serde(borrow)]
    cids: Cow<'a, [u8]>,
    #[serde(borrow)]
    keys: Cow<'a, [u8]>,
    #[serde(borrow)]
    positions: Cow<'a, [u8]>,
    #[serde(borrow)]
    ops: Cow<'a, [u8]>,
    #[serde(borrow)]
    delete_start_ids: Cow<'a, [u8]>,
    #[serde(borrow)]
    values: Cow<'a, [u8]>,
}

fn diagnose_block(block: &EncodedBlock) {
    info!("Diagnosing EncodedBlock:");
    info!("  header {} bytes", block.header.len());
    info!("  change_meta {} bytes", block.change_meta.len());
    info!("  cids: {} bytes", block.cids.len());
    info!("  keys: {} bytes", block.keys.len());
    info!("  positions: {} bytes", block.positions.len());
    info!("  ops: {} bytes", block.ops.len());
    info!("  delete_id_starts: {} bytes", block.delete_start_ids.len());
    info!("  values: {} bytes", block.values.len());
}

const VERSION: u16 = 0;

// MARK: encode_block
/// It's assume that every change in the block share the same peer.
pub fn encode_block(block: &[Change], arena: &SharedArena) -> Vec<u8> {
    if block.is_empty() {
        panic!("Empty block")
    }

    let mut peer_register: ValueRegister<PeerID> = ValueRegister::new();
    let peer = block[0].peer();
    peer_register.register(&peer);

    let cid_register: ValueRegister<ContainerID> = ValueRegister::new();

    let mut encoded_ops = Vec::new();
    let mut registers = Registers {
        peer_register,
        key_register: ValueRegister::new(),
        cid_register,
        position_register: ValueRegister::new(),
    };

    {
        // Init position register, making it ordered by fractional index
        let mut position_set = BTreeSet::default();
        for c in block {
            for op in c.ops().iter() {
                if let crate::op::InnerContent::Tree(tree_op) = &op.content {
                    match &**tree_op {
                        tree_op::TreeOp::Create { position, .. } => {
                            position_set.insert(position.clone());
                        }
                        tree_op::TreeOp::Move { position, .. } => {
                            position_set.insert(position.clone());
                        }
                        tree_op::TreeOp::Delete { .. } => {}
                    }
                }
            }
        }

        for position in position_set {
            registers.position_register.register(&position);
        }
    }

    let mut del_starts: Vec<_> = Vec::new();
    let mut value_writer = ValueWriter::new();
    for c in block {
        for op in c.ops().iter() {
            let cid = arena.get_container_id(op.container).unwrap();
            let cidx = registers.cid_register.register(&cid);
            let prop = get_op_prop(op, &mut registers);
            let value_kind = encode_op(
                op,
                arena,
                &mut del_starts,
                &mut value_writer,
                &mut registers,
            );
            encoded_ops.push(EncodedOp {
                container_index: cidx as u32,
                prop,
                value_type: value_kind.to_u8(),
                len: op.atom_len() as u32,
            })
        }
    }

    let cids = registers.cid_register.unwrap_vec();
    let container_arena = ContainerArena::from_containers(
        cids,
        &mut registers.peer_register,
        &mut registers.key_register,
    );

    // Write to output

    //      ┌────────────────────┬─────────────────────────────────────────┐
    //      │  Key Strings Size  │               Key Strings               │
    //      └────────────────────┴─────────────────────────────────────────┘
    let keys = registers.key_register.unwrap_vec();
    let keys_bytes = encode_keys(keys);

    //      ┌────────────────────┬─────────────────────────────────────────┐
    //      │  Position Size     │               Position                  │
    //      └────────────────────┴─────────────────────────────────────────┘
    let position_vec = registers.position_register.unwrap_vec();
    let positions = PositionArena::from_positions(position_vec.iter().map(|p| p.as_bytes()));
    let position_bytes = positions.encode_v2();

    //      ┌──────────┬──────────┬───────┬────────────────────────────────┐
    //      │          │          │       │                                │
    //      │          │          │       │                                │
    //      │          │          │       │                                │
    //      │  LEB128  │   RLE    │ Delta │                                │
    //      │ Lengths  │Containers│  RLE  │           ValueType            │
    //      │          │          │ Props │                                │
    //      │          │          │       │                                │
    //      │          │          │       │                                │
    //      │          │          │       │                                │
    //      └──────────┴──────────┴───────┴────────────────────────────────┘

    let ops_bytes = serde_columnar::to_vec(&EncodedOps { ops: encoded_ops }).unwrap();

    let delete_id_starts_bytes = if del_starts.is_empty() {
        Vec::new()
    } else {
        serde_columnar::to_vec(&EncodedDeleteStartIds {
            delete_start_ids: del_starts,
        })
        .unwrap()
    };
    //      ┌────────────────┬─────────────────────────────────────────────┐
    //      │Value Bytes Size│                Value Bytes                  │
    //      └────────────────┴─────────────────────────────────────────────┘

    // PeerIDs
    let mut peer_register = registers.peer_register;
    // .unwrap_vec();
    // let peer_bytes: Vec<u8> = peers.iter().flat_map(|p| p.to_le_bytes()).collect();

    // Change meta
    let (header, change_meta) = encode_changes(block, &mut peer_register);

    let value_bytes = value_writer.finish();
    let out = EncodedBlock {
        counter_start: block[0].id.counter as u32,
        counter_len: (block.last().unwrap().ctr_end() - block[0].id.counter) as u32,
        lamport_start: block[0].lamport(),
        lamport_len: block.last().unwrap().lamport_end() - block[0].lamport(),
        n_changes: block.len() as u32,
        header: header.into(),
        change_meta: change_meta.into(),
        cids: container_arena.encode().into(),
        keys: keys_bytes.into(),
        positions: position_bytes.into(),
        ops: ops_bytes.into(),
        delete_start_ids: delete_id_starts_bytes.into(),
        values: value_bytes.into(),
    };

    diagnose_block(&out);
    postcard::to_allocvec(&out).unwrap()
}

fn encode_keys(keys: Vec<loro_common::InternalString>) -> Vec<u8> {
    let mut keys_bytes = Vec::new();
    for key in keys {
        let bytes = key.as_bytes();
        leb128::write::unsigned(&mut keys_bytes, bytes.len() as u64).unwrap();
        keys_bytes.write_all(bytes).unwrap();
    }
    keys_bytes
}

fn decode_keys(mut bytes: &[u8]) -> LoroResult<Vec<InternalString>> {
    let mut keys = Vec::new();
    while !bytes.is_empty() {
        let len = leb128::read::unsigned(&mut bytes)
            .map_err(|_| LoroError::DecodeDataCorruptionError)? as usize;
        if bytes.len() < len {
            return Err(LoroError::DecodeDataCorruptionError);
        }
        let key =
            std::str::from_utf8(&bytes[..len]).map_err(|_| LoroError::DecodeDataCorruptionError)?;
        keys.push(key.into());
        bytes = &bytes[len..];
    }

    Ok(keys)
}

struct Registers {
    peer_register: ValueRegister<PeerID>,
    key_register: ValueRegister<loro_common::InternalString>,
    cid_register: ValueRegister<ContainerID>,
    position_register: ValueRegister<FractionalIndex>,
}

use crate::encoding::value::{
    RawTreeMove, Value, ValueDecodedArenasTrait, ValueEncodeRegister, ValueKind, ValueReader,
    ValueWriter,
};
use crate::oplog::change_store::block_meta_encode::encode_changes;
use crate::version::Frontiers;
impl ValueEncodeRegister for Registers {
    fn key_mut(&mut self) -> &mut ValueRegister<loro_common::InternalString> {
        &mut self.key_register
    }

    fn peer_mut(&mut self) -> &mut ValueRegister<PeerID> {
        &mut self.peer_register
    }

    fn encode_tree_op(&mut self, op: &tree_op::TreeOp) -> encoding::value::Value<'static> {
        match op {
            tree_op::TreeOp::Create {
                target,
                parent,
                position,
            } => encoding::value::Value::RawTreeMove(RawTreeMove {
                subject_peer_idx: self.peer_register.register(&target.peer),
                subject_cnt: target.counter,
                is_parent_null: parent.is_none(),
                parent_peer_idx: parent.map_or(0, |p| self.peer_register.register(&p.peer)),
                parent_cnt: parent.map_or(0, |p| p.counter),
                position_idx: self.position_register.register(position),
            }),
            tree_op::TreeOp::Move {
                target,
                parent,
                position,
            } => encoding::value::Value::RawTreeMove(RawTreeMove {
                subject_peer_idx: self.peer_register.register(&target.peer),
                subject_cnt: target.counter,
                is_parent_null: parent.is_none(),
                parent_peer_idx: parent.map_or(0, |p| self.peer_register.register(&p.peer)),
                parent_cnt: parent.map_or(0, |p| p.counter),
                position_idx: self.position_register.register(position),
            }),
            tree_op::TreeOp::Delete { target } => {
                let parent = TreeID::delete_root();
                encoding::value::Value::RawTreeMove(RawTreeMove {
                    subject_peer_idx: self.peer_register.register(&target.peer),
                    subject_cnt: target.counter,
                    is_parent_null: false,
                    parent_peer_idx: self.peer_register.register(&parent.peer),
                    parent_cnt: parent.counter,
                    position_idx: 0,
                })
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChangesBlockHeader {
    pub peer: PeerID,
    pub counter: Counter,
    pub n_changes: usize,
    pub peers: Vec<PeerID>,
    /// This has n + 1 elements, where counters[n] is the end counter of the
    /// last change in the block.
    ///
    /// You can infer the size of kth change by taking counters[k + 1] - counters[k]
    pub counters: Vec<Counter>,
    /// This has n elements
    pub lamports: Vec<Lamport>,
    pub deps_groups: Vec<Frontiers>,
    pub keys: OnceCell<Vec<InternalString>>,
    pub cids: OnceCell<Vec<ContainerID>>,
}

pub fn decode_header(m_bytes: &[u8]) -> LoroResult<ChangesBlockHeader> {
    let doc = postcard::from_bytes(m_bytes)
        .map_err(|e| LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str()))?;

    decode_header_from_doc(&doc)
}

// MARK: decode_block_from_doc
fn decode_header_from_doc(doc: &EncodedBlock) -> Result<ChangesBlockHeader, LoroError> {
    let EncodedBlock {
        n_changes,
        header,
        counter_len,
        counter_start,
        lamport_len,
        lamport_start,
        ..
    } = doc;
    let ans: ChangesBlockHeader = decode_changes_header(
        header,
        *n_changes as usize,
        Counter::try_from(*counter_start).map_err(|_| LoroError::DecodeDataCorruptionError)?,
        Counter::try_from(*counter_len).map_err(|_| LoroError::DecodeDataCorruptionError)?,
        *lamport_start,
        *lamport_len,
    )?;
    Ok(ans)
}

#[columnar(vec, ser, de, iterable)]
#[derive(Debug, Clone)]
struct EncodedOp {
    #[columnar(strategy = "DeltaRle")]
    container_index: u32,
    #[columnar(strategy = "DeltaRle")]
    prop: i32,
    #[columnar(strategy = "Rle")]
    value_type: u8,
    #[columnar(strategy = "Rle")]
    len: u32,
}

#[columnar(ser, de)]
struct EncodedOps {
    #[columnar(class = "vec", iter = "EncodedOp")]
    ops: Vec<EncodedOp>,
}

#[columnar(ser, de)]
struct EncodedDeleteStartIds {
    #[columnar(class = "vec", iter = "EncodedDeleteStartId")]
    delete_start_ids: Vec<EncodedDeleteStartId>,
}

struct ValueDecodeArena<'a> {
    peers: &'a [PeerID],
    keys: &'a [InternalString],
}

impl ValueDecodedArenasTrait for ValueDecodeArena<'_> {
    fn keys(&self) -> &[InternalString] {
        self.keys
    }

    fn peers(&self) -> &[PeerID] {
        self.peers
    }

    fn decode_tree_op(
        &self,
        _positions: &[Vec<u8>],
        _op: encoding::value::EncodedTreeMove,
        _id: ID,
    ) -> LoroResult<tree_op::TreeOp> {
        Err(LoroError::DecodeDataCorruptionError)
    }
}

pub fn decode_block_range(
    mut bytes: &[u8],
) -> LoroResult<((Counter, Counter), (Lamport, Lamport))> {
    let version = leb128::read::unsigned(&mut bytes).map_err(|e| {
        LoroError::DecodeError(format!("Failed to read version: {e}").into_boxed_str())
    })?;

    if version as u16 != VERSION {
        return Err(LoroError::DecodeError(
            "Version mismatch".to_string().into_boxed_str(),
        ));
    }

    let counter_start = leb128::read::unsigned(&mut bytes).map_err(|e| {
        LoroError::DecodeError(format!("Failed to read counter start: {e}").into_boxed_str())
    })? as Counter;
    let counter_len = leb128::read::unsigned(&mut bytes).map_err(|e| {
        LoroError::DecodeError(format!("Failed to read counter length: {e}").into_boxed_str())
    })? as Counter;
    let lamport_start = leb128::read::unsigned(&mut bytes).map_err(|e| {
        LoroError::DecodeError(format!("Failed to read lamport start: {e}").into_boxed_str())
    })? as Lamport;
    let lamport_len = leb128::read::unsigned(&mut bytes).map_err(|e| {
        LoroError::DecodeError(format!("Failed to read lamport length: {e}").into_boxed_str())
    })? as Lamport;
    Ok((
        (counter_start, counter_start + counter_len),
        (lamport_start, lamport_start + lamport_len),
    ))
}

/// Ensure the cids in header are decoded
#[allow(unused)]
pub fn decode_cids(
    bytes: &[u8],
    header: Option<ChangesBlockHeader>,
) -> LoroResult<ChangesBlockHeader> {
    let doc = postcard::from_bytes(bytes)
        .map_err(|e| LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str()))?;
    let header = if let Some(h) = header {
        h
    } else {
        let doc = postcard::from_bytes(bytes).map_err(|e| {
            LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str())
        })?;
        decode_header_from_doc(&doc)?
    };

    if header.cids.get().is_some() {
        return Ok(header);
    }

    let EncodedBlock { cids, keys, .. } = doc;
    let keys = header.keys.get_or_try_init(|| decode_keys(&keys))?;
    let decode_arena = ValueDecodeArena {
        peers: &header.peers,
        keys,
    };

    header.cids.get_or_try_init(|| {
        ContainerArena::decode(&cids)?
            .iter()
            .map(|x| x.as_container_id(&decode_arena))
            .try_collect()
    })?;
    Ok(header)
}

// Metadata decoding never touches operation payloads or a shared arena.
fn decode_change_metas(
    doc: &EncodedBlock<'_>,
    header: &ChangesBlockHeader,
) -> LoroResult<Vec<ChangeMeta>> {
    let n_changes = doc.n_changes as usize;
    let timestamp_decoder = DeltaOfDeltaDecoder::<i64>::new(&doc.change_meta)
        .map_err(|_| LoroError::DecodeDataCorruptionError)?;
    let (timestamps, bytes) = timestamp_decoder
        .take_n_finalize(n_changes)
        .map_err(|_| LoroError::DecodeDataCorruptionError)?;
    let (commit_msg_lens, commit_msgs) = AnyRleDecoder::<u32>::new(bytes)
        .take_n_finalize(n_changes)
        .map_err(|_| LoroError::DecodeDataCorruptionError)?;
    let mut commit_msg_index = 0usize;
    let mut metas = Vec::with_capacity(n_changes);
    for i in 0..n_changes {
        let len = *commit_msg_lens.get(i).ok_or(LoroError::DecodeDataCorruptionError)?;
        let end = commit_msg_index.checked_add(len as usize)
            .ok_or(LoroError::DecodeDataCorruptionError)?;
        let message = commit_msgs.get(commit_msg_index..end)
            .ok_or(LoroError::DecodeDataCorruptionError)?;
        let message = std::str::from_utf8(message)
            .map_err(|_| LoroError::DecodeDataCorruptionError)?;
        commit_msg_index = end;
        let start = *header.counters.get(i).ok_or(LoroError::DecodeDataCorruptionError)?;
        let end = *header.counters.get(i + 1).ok_or(LoroError::DecodeDataCorruptionError)?;
        if start < 0 || end <= start {
            return Err(LoroError::DecodeDataCorruptionError);
        }
        let lamport = *header.lamports.get(i).ok_or(LoroError::DecodeDataCorruptionError)?;
        lamport.checked_add((end - start) as Lamport)
            .ok_or(LoroError::DecodeDataCorruptionError)?;
        metas.push(ChangeMeta {
            id: ID::new(header.peer, start),
            lamport,
            timestamp: *timestamps.get(i).ok_or(LoroError::DecodeDataCorruptionError)? as Timestamp,
            message: if message.is_empty() { None } else { Some(Arc::from(message)) },
            deps: header.deps_groups.get(i).ok_or(LoroError::DecodeDataCorruptionError)?.clone(),
            len: (end - start) as usize,
        });
    }
    if commit_msg_index != commit_msgs.len() {
        return Err(LoroError::DecodeDataCorruptionError);
    }
    Ok(metas)
}

/// Look up retained change metadata without decoding operations or populating caches.
pub(super) fn get_change_meta_from_bytes(bytes: &[u8], id: ID) -> LoroResult<Option<ChangeMeta>> {
    let doc: EncodedBlock<'_> = postcard::from_bytes(bytes)
        .map_err(|e| LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str()))?;
    let header = decode_header_from_doc(&doc)?;
    let metas = decode_change_metas(&doc, &header)?;
    Ok(metas.into_iter().find(|meta| {
        meta.id.peer == id.peer && meta.id.counter <= id.counter
            && id.counter < meta.id.counter + meta.len as Counter
    }))
}

/// Visit precisely sliced JSON operations. Only the current operation owns a scratch arena.
pub(super) fn try_visit_json_ops_from_bytes<E: From<LoroError>>(
    bytes: &[u8],
    mut span: IdSpan,
    mut f: impl FnMut(&ChangeMeta, JsonOp) -> Result<(), E>,
) -> Result<(), E> {
    if span.counter.start == span.counter.end {
        return Ok(());
    }
    span.normalize_();
    let doc: EncodedBlock<'_> = postcard::from_bytes(bytes)
        .map_err(|e| LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str()))?;
    // This header and its dictionaries are private to the scan, never the store's caches.
    let header = decode_header_from_doc(&doc)?;
    let mut metas = decode_change_metas(&doc, &header)?;
    if span.peer != header.peer || span.counter.start < header.counter
        || span.counter.end > *header.counters.last().ok_or(LoroError::DecodeDataCorruptionError)?
    {
        return Err(LoroError::DecodeDataCorruptionError.into());
    }
    for meta in &mut metas {
        let start = span.counter.start.max(meta.id.counter);
        let end = span.counter.end.min(meta.id.counter + meta.len as Counter);
        if start >= end {
            continue;
        }
        let offset = start - meta.id.counter;
        if offset > 0 {
            meta.deps = Frontiers::from_id(ID::new(span.peer, start - 1));
            meta.id.counter = start;
            meta.lamport += offset as Lamport;
        }
        meta.len = (end - start) as usize;
    }
    try_decode_block_ops(&doc, &header, None, |change_index, op, arena| {
        let start = span.counter.start.max(op.counter);
        let end = span.counter.end.min(op.counter + op.atom_len() as Counter);
        if start < end {
            let meta = &metas[change_index];
            if start == op.counter && end == op.counter + op.atom_len() as Counter {
                f(meta, encode_json_op(&op, arena, None))?;
            } else {
                let sliced = op.slice((start - op.counter) as usize, (end - op.counter) as usize);
                f(meta, encode_json_op(&sliced, arena, None))?;
            }
        }
        Ok(())
    })
}

// Both bulk decoding and read-only inspection use the native value/operation codecs.
// A missing arena requests an operation-scoped scratch arena, not block-wide storage.
fn try_decode_block_ops<E: From<LoroError>>(
    doc: &EncodedBlock<'_>,
    header: &ChangesBlockHeader,
    arena: Option<&SharedArena>,
    mut f: impl FnMut(usize, Op, &SharedArena) -> Result<(), E>,
) -> Result<(), E> {
    let keys = header.keys.get_or_try_init(|| decode_keys(&doc.keys))?;
    let decode_arena = ValueDecodeArena { peers: &header.peers, keys };
    let positions = PositionArena::decode_v2(&doc.positions)?.try_parse_to_positions()?;
    let cids = header.cids.get_or_try_init(|| {
        ContainerArena::decode(&doc.cids)?.iter()
            .map(|x| x.as_container_id(&decode_arena)).try_collect()
    })?;
    let mut value_reader = ValueReader::new(&doc.values);
    let op_iter = serde_columnar::iter_from_bytes::<EncodedOps>(&doc.ops)
        .map_err(LoroError::from)?.ops;
    let delete_ids = if doc.delete_start_ids.is_empty() {
        None
    } else {
        Some(serde_columnar::iter_from_bytes::<EncodedDeleteStartIds>(&doc.delete_start_ids)
            .map_err(LoroError::from)?.delete_start_ids)
    };
    let mut del_iter = delete_ids.into_iter().flatten();
    let mut counter = header.counter;
    let mut change_index = 0;
    for encoded in op_iter {
        let EncodedOp { container_index, prop, value_type, len } = encoded.map_err(LoroError::from)?;
        let len = Counter::try_from(len).map_err(|_| LoroError::DecodeDataCorruptionError)?;
        let end = counter.checked_add(len).ok_or(LoroError::DecodeDataCorruptionError)?;
        let change_end = *header.counters.get(change_index + 1)
            .ok_or(LoroError::DecodeDataCorruptionError)?;
        if len <= 0 || end > change_end {
            return Err(LoroError::DecodeDataCorruptionError.into());
        }
        let value = Value::decode(ValueKind::from_u8(value_type), &mut value_reader,
            &decode_arena, ID::new(header.peer, counter))?;
        let cid = cids.get(container_index as usize).ok_or(LoroError::DecodeDataCorruptionError)?;
        let scratch;
        let shared_arena = if let Some(arena) = arena {
            arena
        } else {
            scratch = SharedArena::new();
            &scratch
        };
        let content = decode_op(cid, value, &mut del_iter, shared_arena, &decode_arena,
            &positions, prop, ID::new(header.peer, counter))?;
        let op = Op { counter, container: shared_arena.register_container(cid), content };
        if op.atom_len() != len as usize {
            return Err(LoroError::DecodeDataCorruptionError.into());
        }
        f(change_index, op, shared_arena)?;
        counter = end;
        if counter == change_end {
            change_index += 1;
        }
    }
    if change_index != header.n_changes || del_iter.next().is_some() {
        return Err(LoroError::DecodeDataCorruptionError.into());
    }
    Ok(())
}

// MARK: decode_block
pub fn decode_block(
    m_bytes: &[u8],
    shared_arena: &SharedArena,
    header: Option<&ChangesBlockHeader>,
) -> LoroResult<Vec<Change>> {
    let doc = postcard::from_bytes(m_bytes)
        .map_err(|e| LoroError::DecodeError(format!("Decode block error {e}").into_boxed_str()))?;
    let header_on_stack;
    let header = if let Some(header) = header {
        header
    } else {
        header_on_stack = decode_header_from_doc(&doc)?;
        &header_on_stack
    };
    let mut changes: Vec<Change> = decode_change_metas(&doc, header)?.into_iter().map(|meta| Change {
        ops: Default::default(),
        id: meta.id,
        lamport: meta.lamport,
        timestamp: meta.timestamp,
        commit_msg: meta.message,
        deps: meta.deps,
    }).collect();
    try_decode_block_ops::<LoroError>(&doc, header, Some(shared_arena), |index, op, _| {
        changes[index].ops.push(op);
        Ok(())
    })?;
    Ok(changes)
}

#[cfg(test)]
mod test {
    use std::{borrow::Cow, panic::AssertUnwindSafe};

    use loro_common::{ContainerType, HasId};

    use crate::encoding::value::{EncodedTreeMove, Value};
    use crate::{cursor::PosType, delta::DeltaValue, loro::ExportMode, LoroDoc};

    use super::*;

    #[test]
    fn readonly_scan_matches_native_slices_and_propagates_errors() {
        let doc = LoroDoc::new_auto_commit();
        let text = doc.get_text("text");
        text.insert(0, "aé中z", PosType::Unicode).unwrap();
        doc.get_map("map").insert("x", 42).unwrap();
        doc.commit_then_renew();
        text.delete(1, 2, PosType::Unicode).unwrap();
        doc.commit_then_renew();
        let oplog = doc.oplog.lock();
        let mut changes = Vec::new();
        oplog.change_store().visit_all_changes(&mut |change| changes.push(change.clone()));
        let bytes = encode_block(&changes, &oplog.arena);
        for change in &changes {
            for from in 0..change.atom_len() {
                for to in from + 1..=change.atom_len() {
                    let sliced = change.slice(from, to);
                    let expected = crate::encoding::json_schema::encode_change(
                        crate::change::ChangeRef::from_change(&sliced), &oplog.arena, None);
                    let span = IdSpan::new(change.id.peer,
                        change.id.counter + from as Counter, change.id.counter + to as Counter);
                    let mut actual = Vec::new();
                    try_visit_json_ops_from_bytes::<LoroError>(&bytes, span, |meta, op| {
                        assert_eq!(meta.id, sliced.id);
                        assert_eq!(meta.lamport, sliced.lamport);
                        assert_eq!(meta.deps, sliced.deps);
                        assert_eq!(meta.len, sliced.atom_len());
                        assert_eq!(meta.timestamp, sliced.timestamp);
                        assert_eq!(meta.message, sliced.commit_msg);
                        actual.push(op);
                        Ok(())
                    }).unwrap();
                    assert_eq!(serde_json::to_value(actual).unwrap(),
                        serde_json::to_value(expected.ops).unwrap());
                    let meta = get_change_meta_from_bytes(&bytes, span.id_start()).unwrap().unwrap();
                    assert_eq!(meta.id, change.id);
                    assert_eq!(meta.len, change.atom_len());
                }
            }
        }
        let first = &changes[0];
        let span = IdSpan::new(first.id.peer, first.id.counter, first.id.counter + 1);
        let mut calls = 0;
        let result = try_visit_json_ops_from_bytes::<LoroError>(&bytes, span, |_, _| {
            calls += 1;
            Err(LoroError::DecodeDataCorruptionError)
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
        assert!(try_visit_json_ops_from_bytes::<LoroError>(&[],
            IdSpan::new(first.id.peer, 0, 0), |_, _| unreachable!()).is_ok());
        let last = changes.last().unwrap();
        let end = last.id.counter + last.atom_len() as Counter;
        assert!(try_visit_json_ops_from_bytes::<LoroError>(&bytes,
            IdSpan::new(first.id.peer, first.id.counter, end + 1), |_, _| Ok(())).is_err());
        let mut corrupt: EncodedBlock = postcard::from_bytes(&bytes).unwrap();
        corrupt.ops = Cow::Owned(vec![0x80]);
        let corrupt = postcard::to_allocvec(&corrupt).unwrap();
        assert!(get_change_meta_from_bytes(&corrupt, first.id).unwrap().is_some());
        assert!(try_visit_json_ops_from_bytes::<LoroError>(&corrupt, span, |_, _| Ok(())).is_err());
    }

    #[test]
    fn readonly_headers_reject_unbounded_counts_and_wrapped_lengths() {
        let doc = LoroDoc::new_auto_commit();
        doc.set_peer_id(7).unwrap();
        doc.set_next_commit_message("first change");
        doc.get_text("text").insert(0, "a", PosType::Unicode).unwrap();
        doc.commit_then_renew();
        doc.set_next_commit_message("second change");
        doc.get_text("text").insert(1, "b", PosType::Unicode).unwrap();
        doc.commit_then_renew();
        let oplog = doc.oplog.lock();
        let mut changes = Vec::new();
        oplog.change_store().visit_all_changes(&mut |change| changes.push(change.clone()));
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].atom_len(), 1);
        assert_eq!(changes[1].atom_len(), 1);
        let valid = encode_block(&changes, &oplog.arena);
        let single = encode_block(&changes[..1], &oplog.arena);
        let id = changes[0].id;
        for case in 0..8 {
            let mut block: EncodedBlock = postcard::from_bytes(if case == 3 { &single } else { &valid }).unwrap();
            match case {
                0 => block.n_changes = u32::MAX,
                1 => {
                    block.n_changes = Counter::MAX as u32;
                    block.counter_len = Counter::MAX as u32;
                    block.lamport_len = Counter::MAX as u32;
                    let mut header = vec![1];
                    header.extend_from_slice(&id.peer.to_le_bytes());
                    block.header = Cow::Owned(header);
                }
                2 | 5 | 6 => {
                    // Replace only the explicit length; all trailing native
                    // dependency/Lamport columns and operation payloads stay valid.
                    let mut rest = block.header.as_ref();
                    let peers = leb128::read::unsigned(&mut rest).unwrap() as usize;
                    rest = &rest[peers * 8..];
                    let length_start = block.header.len() - rest.len();
                    assert_eq!(leb128::read::unsigned(&mut rest).unwrap(), 1);
                    let mut header = block.header[..length_start].to_vec();
                    let length = match case { 2 => (1u64 << 32) + 1, 5 => 0, _ => 2 };
                    leb128::write::unsigned(&mut header, length).unwrap();
                    header.extend_from_slice(rest);
                    block.header = Cow::Owned(header);
                }
                3 => {
                    block.counter_len = u32::MAX;
                    block.lamport_len = u32::MAX;
                }
                4 => block.counter_start = u32::MAX,
                7 => block.counter_start = Counter::MAX as u32,
                _ => unreachable!(),
            }
            assert!(decode_header_from_doc(&block).is_err(), "header case {case}");
            let bytes = postcard::to_allocvec(&block).unwrap();
            assert!(get_change_meta_from_bytes(&bytes, id).is_err(), "case {case}");
            let store = super::super::ChangeStore::new_for_test();
            store.external_kv.lock().set(&id.to_bytes(), bytes.into());
            assert!(store.get_change_meta(id).is_err(), "cold lookup case {case}");
            assert!(store.try_visit_json_ops::<LoroError>(
                IdSpan::new(id.peer, id.counter, id.counter + 1),
                |_, _| panic!("malformed header delivered an operation"),
            ).is_err(), "visitor case {case}");
        }
        assert!(decode_changes_header(&[], 1, -1, 1, 0, 1).is_err());
        assert!(decode_changes_header(&[], 1, 0, 0, 0, 1).is_err());
    }

    #[test]
    fn readonly_scan_preserves_future_marks_and_tree_operations() {
        use crate::container::{list::list_op::InnerListOp, richtext::TextStyleInfoFlag};
        use crate::encoding::value::{OwnedFutureValue, OwnedValue};
        use crate::op::{FutureInnerContent, InnerContent};

        let arena = SharedArena::new();
        let unknown = arena.register_container(&ContainerID::Root {
            name: "future".into(), container_type: ContainerType::Unknown(42),
        });
        let text = arena.register_container(&ContainerID::Root {
            name: "text".into(), container_type: ContainerType::Text,
        });
        let tree = arena.register_container(&ContainerID::Root {
            name: "tree".into(), container_type: ContainerType::Tree,
        });
        let target = TreeID::new(7, 3);
        let contents = [
            (unknown, InnerContent::Future(FutureInnerContent::Unknown {
                prop: -7,
                value: Box::new(OwnedValue::Future(OwnedFutureValue::Unknown {
                    kind: 42, data: Arc::new(vec![0, 127, 255]),
                })),
            })),
            (text, InnerContent::List(InnerListOp::StyleStart {
                start: 0, end: 3, key: "bold".into(), value: true.into(),
                info: TextStyleInfoFlag::BOLD,
            })),
            (text, InnerContent::List(InnerListOp::StyleEnd)),
            (tree, InnerContent::Tree(Arc::new(tree_op::TreeOp::Create {
                target, parent: None, position: FractionalIndex::default(),
            }))),
            (tree, InnerContent::Tree(Arc::new(tree_op::TreeOp::Move {
                target, parent: Some(TreeID::new(9, 1)), position: FractionalIndex::default(),
            }))),
            (tree, InnerContent::Tree(Arc::new(tree_op::TreeOp::Delete { target }))),
        ];
        let mut change = Change {
            id: ID::new(7, 0), lamport: 100, timestamp: 123,
            commit_msg: Some(Arc::from("codec families")),
            deps: Frontiers::from_id(ID::new(9, 3)), ops: Default::default(),
        };
        for (counter, (container, content)) in contents.into_iter().enumerate() {
            change.ops.push(Op { counter: counter as Counter, container, content });
        }
        let bytes = encode_block(std::slice::from_ref(&change), &arena);
        for from in 0..change.atom_len() {
            let sliced = change.slice(from, change.atom_len());
            let expected = crate::encoding::json_schema::encode_change(
                crate::change::ChangeRef::from_change(&sliced), &arena, None);
            let mut actual = Vec::new();
            try_visit_json_ops_from_bytes::<LoroError>(&bytes,
                IdSpan::new(7, from as Counter, change.atom_len() as Counter), |meta, op| {
                    assert_eq!(meta.id, sliced.id);
                    assert_eq!(meta.lamport, sliced.lamport);
                    assert_eq!(meta.len, sliced.atom_len());
                    assert_eq!(meta.deps, sliced.deps);
                    assert_eq!(meta.timestamp, sliced.timestamp);
                    assert_eq!(meta.message, sliced.commit_msg);
                    actual.push(op);
                    Ok(())
                }).unwrap();
            assert_eq!(serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected.ops).unwrap());
        }
    }

    #[test]
    fn decode_block_rejects_corrupt_payload_without_panic() {
        let doc = LoroDoc::new_auto_commit();
        doc.get_map("map").insert("x", 100).unwrap();
        doc.commit_then_renew();

        let oplog = doc.oplog.lock();
        let mut changes = Vec::new();
        oplog
            .change_store()
            .visit_all_changes(&mut |change| changes.push(change.clone()));
        let block_bytes = encode_block(&changes, &oplog.arena);
        let mut encoded: EncodedBlock = postcard::from_bytes(&block_bytes).unwrap();
        encoded.change_meta = Cow::Owned(vec![0x80]);
        let corrupt = postcard::to_allocvec(&encoded).unwrap();

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            decode_block(&corrupt, &oplog.arena, None)
        }));
        assert!(result.is_ok(), "corrupt block payload should not panic");
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn tree_move_payload_is_rejected_without_panic() {
        let arena = SharedArena::new();
        let value_arena = ValueDecodeArena {
            peers: &[1],
            keys: &[],
        };
        let cid = ContainerID::Root {
            name: "tree".into(),
            container_type: ContainerType::Tree,
        };
        let value = Value::TreeMove(EncodedTreeMove {
            target_idx: 0,
            is_parent_null: true,
            parent_idx: 0,
            position: 0,
        });
        let mut deletes = std::iter::empty();

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            decode_op(
                &cid,
                value,
                &mut deletes,
                &arena,
                &value_arena,
                &[],
                0,
                ID::new(1, 0),
            )
        }));

        assert!(result.is_ok(), "tree move payload should not panic");
        assert!(matches!(
            result.unwrap(),
            Err(LoroError::DecodeDataCorruptionError)
        ));
    }

    #[test]
    pub fn encode_single_text_edit() {
        let doc = LoroDoc::new();
        doc.start_auto_commit();
        doc.get_map("map").insert("x", 100).unwrap();
        // doc.get_text("text").insert(0, "Hi").unwrap();
        // let node = doc.get_tree("tree").create(None).unwrap();
        // doc.get_tree("tree").create(node).unwrap();
        diagnose(&doc);
        doc.get_map("map").insert("y", 20).unwrap();
        diagnose(&doc);
        doc.get_map("map").insert("z", 1000).unwrap();
        diagnose(&doc);
        doc.get_text("text")
            .insert(0, "Hello", PosType::Unicode)
            .unwrap();
        diagnose(&doc);
        doc.get_text("text")
            .insert(2, "He", PosType::Unicode)
            .unwrap();
        diagnose(&doc);
        doc.get_text("text").delete(1, 4, PosType::Unicode).unwrap();
        diagnose(&doc);
        doc.get_text("text").delete(0, 2, PosType::Unicode).unwrap();
        diagnose(&doc);
    }

    fn diagnose(doc: &LoroDoc) {
        let bytes = doc.export(ExportMode::all_updates()).unwrap();
        println!("Old Update bytes {:?}", bytes.length());

        let bytes = doc.export(ExportMode::all_updates()).unwrap();
        println!("Update bytes {:?}", bytes.length());
        // assert!(bytes.len() < 30);

        let bytes = doc.export(ExportMode::Snapshot).unwrap();
        println!("Snapshot bytes {:?}", bytes.length());
        // assert!(bytes.len() < 30);

        let json = doc.export_json_updates(&Default::default(), &doc.oplog_vv(), true);
        let json_string = serde_json::to_string(&json.changes).unwrap();
        println!(
            "JSON string bytes {:?}",
            json_string.len()
        );
        let bytes = postcard::to_allocvec(&json.changes).unwrap();
        println!("JSON update bytes {:?}", bytes.len());
        println!("\n\n")
    }
}
