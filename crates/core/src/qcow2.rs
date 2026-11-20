//! Creates an empty qcow2 image: air/OS's BFS has no sparse files, so the
//! data disk must be a format that grows as Android writes.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

const CLUSTER_BITS: u32 = 16;
const CLUSTER: u64 = 1 << CLUSTER_BITS;

/// Writes a new qcow2 (v3, 64 KiB clusters, 16-bit refcounts) of `size`
/// bytes. Fails if `path` exists.
pub fn create(path: &Path, size: u64) -> io::Result<()> {
    let l2_entries = CLUSTER / 8;
    let l1_size = size.div_ceil(CLUSTER * l2_entries);
    let l1_clusters = (l1_size * 8).div_ceil(CLUSTER).max(1);
    // cluster 0 header, 1 refcount table, 2 refcount block, 3.. L1
    let reftable_off = CLUSTER;
    let refblock_off = 2 * CLUSTER;
    let l1_off = 3 * CLUSTER;
    let used = 3 + l1_clusters;
    let total = used * CLUSTER;

    let mut img = vec![0u8; total as usize];
    let mut h = Vec::with_capacity(104);
    h.extend_from_slice(b"QFI\xfb");
    h.extend_from_slice(&3u32.to_be_bytes()); // version
    h.extend_from_slice(&0u64.to_be_bytes()); // backing file offset
    h.extend_from_slice(&0u32.to_be_bytes()); // backing file size
    h.extend_from_slice(&CLUSTER_BITS.to_be_bytes());
    h.extend_from_slice(&size.to_be_bytes());
    h.extend_from_slice(&0u32.to_be_bytes()); // crypt method
    h.extend_from_slice(&(l1_size as u32).to_be_bytes());
    h.extend_from_slice(&l1_off.to_be_bytes());
    h.extend_from_slice(&reftable_off.to_be_bytes());
    h.extend_from_slice(&1u32.to_be_bytes()); // refcount table clusters
    h.extend_from_slice(&0u32.to_be_bytes()); // snapshots
    h.extend_from_slice(&0u64.to_be_bytes()); // snapshots offset
    h.extend_from_slice(&0u64.to_be_bytes()); // incompatible features
    h.extend_from_slice(&0u64.to_be_bytes()); // compatible features
    h.extend_from_slice(&0u64.to_be_bytes()); // autoclear features
    h.extend_from_slice(&4u32.to_be_bytes()); // refcount order: 16 bits
    h.extend_from_slice(&104u32.to_be_bytes()); // header length
    img[..h.len()].copy_from_slice(&h);
    // end of header extensions
    // (an all-zero extension header right after the header terminates them)
    img[reftable_off as usize..reftable_off as usize + 8].copy_from_slice(&refblock_off.to_be_bytes());
    for c in 0..used {
        let at = (refblock_off + c * 2) as usize;
        img[at..at + 2].copy_from_slice(&1u16.to_be_bytes());
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    f.write_all(&img)?;
    f.sync_all()?;
    Ok(())
}

/// Virtual size of a qcow2 image (None if it is not one).
pub fn virtual_size(path: &Path) -> Option<u64> {
    use std::io::Read;
    let mut h = [0u8; 32];
    File::open(path).ok()?.read_exact(&mut h).ok()?;
    if &h[..4] != b"QFI\xfb" {
        return None;
    }
    Some(u64::from_be_bytes(h[24..32].try_into().ok()?))
}
