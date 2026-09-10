use std::panic::{AssertUnwindSafe, catch_unwind};

use loro::{ExportMode, LoroDoc};

const LORO_ENVELOPE_BODY_START: usize = 20;
const LORO_SNAPSHOT_BODY_START: usize = 22;
const SSTABLE_BLOCK_START: usize = 5;

#[test]
fn corrupt_snapshot_is_rejected_without_poisoning_the_doc() {
    let source = LoroDoc::new();
    source.set_peer_id(9).unwrap();
    source.get_map("map").insert("key", "value").unwrap();
    let mut snapshot = source.export(ExportMode::Snapshot).unwrap();

    corrupt_oplog_sstable_block(&mut snapshot);
    let target = LoroDoc::new();
    let import = catch_unwind(AssertUnwindSafe(|| target.import(&snapshot)));
    assert!(
        import.is_ok(),
        "invalid snapshot must return an error instead of poisoning a Loro lock"
    );
    let error = import
        .unwrap()
        .expect_err("invalid inner SSTable must be rejected");
    assert!(
        error.to_string().to_ascii_lowercase().contains("checksum"),
        "unexpected import error: {error:?}"
    );
    assert!(target.oplog_vv().is_empty());
    assert!(
        target
            .get_deep_value()
            .as_map()
            .is_some_and(|value| value.is_empty())
    );

    target
        .get_map("after")
        .insert("usable", true)
        .expect("failed import must leave the document usable");
    target.export(ExportMode::Snapshot).unwrap();
}

fn corrupt_oplog_sstable_block(snapshot: &mut [u8]) {
    let oplog_len = u32::from_le_bytes(
        snapshot[LORO_SNAPSHOT_BODY_START..LORO_SNAPSHOT_BODY_START + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let oplog_start = LORO_SNAPSHOT_BODY_START + 4;
    assert!(oplog_len > SSTABLE_BLOCK_START);

    // Preserve the outer Loro envelope checksum while invalidating the checksum
    // embedded in the first lazy SSTable block. Loro 1.13.7 skipped that inner
    // validation and later panicked in Block::decode while holding a document lock.
    snapshot[oplog_start + SSTABLE_BLOCK_START] ^= 0xff;
    let checksum = xxhash_rust::xxh32::xxh32(
        &snapshot[LORO_ENVELOPE_BODY_START..],
        u32::from_le_bytes(*b"LORO"),
    );
    snapshot[16..20].copy_from_slice(&checksum.to_le_bytes());
}
