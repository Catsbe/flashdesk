//! Pure-Rust binary compression and bsdiff-style delta patching (`FLDP` v1).

use crate::{ProtocolError, Result};
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

pub const MAX_UNCOMPRESSED_SIZE: usize = 128 * 1024 * 1024;
const PATCH_MAGIC: &[u8; 4] = b"FLDP";
const PATCH_VERSION: u8 = 1;
const HEADER_LEN: usize = 4 + 1 + 8 + 8 + 32; // 53 bytes
const OP_INSERT: u8 = 0;
const OP_COPY_DIFF: u8 = 1;

const HASH_BITS: usize = 19;
const HASH_SIZE: usize = 1 << HASH_BITS;
const EMPTY_SLOT: u32 = u32::MAX;

/// Compresses raw bytes using pure-Rust deflate (no C/C++ runtime dependency).
pub fn compress_full(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() > MAX_UNCOMPRESSED_SIZE {
        return Err(ProtocolError::Framing(format!(
            "input exceeds {} bytes limit",
            MAX_UNCOMPRESSED_SIZE
        )));
    }
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// Decompresses a deflate-encoded payload while enforcing `max_out`.
pub fn decompress_full(compressed: &[u8], max_out: usize) -> Result<Vec<u8>> {
    let cap = max_out.min(MAX_UNCOMPRESSED_SIZE);
    let mut decoder = DeflateDecoder::new(compressed);
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if out.len().saturating_add(n) > cap {
            return Err(ProtocolError::Framing(format!(
                "decompressed payload exceeds limit of {cap} bytes"
            )));
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

#[inline]
fn hash8(chunk: &[u8]) -> usize {
    let v = u64::from_le_bytes(chunk[..8].try_into().unwrap());
    ((v.wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> (64 - HASH_BITS)) as usize
}

#[inline]
fn is_uniform8(chunk: &[u8]) -> bool {
    let first = chunk[0];
    chunk[..8].iter().all(|&b| b == first)
}

fn build_old_index(old: &[u8]) -> Vec<u32> {
    let mut table = vec![EMPTY_SLOT; HASH_SIZE];
    if old.len() < 8 {
        return table;
    }
    let stride = if old.len() > 16 * 1024 * 1024 { 8 } else { 4 };
    let limit = old.len() - 8;
    let mut i = 0usize;
    while i <= limit {
        let chunk = &old[i..i + 8];
        let h = hash8(chunk);
        if table[h] == EMPTY_SLOT || !is_uniform8(chunk) {
            table[h] = i as u32;
        }
        i += stride;
    }
    table
}

fn emit_insert(ops: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let len = u32::try_from(bytes.len())
        .map_err(|_| ProtocolError::Framing("insert chunk exceeds u32".into()))?;
    ops.push(OP_INSERT);
    ops.extend_from_slice(&len.to_be_bytes());
    ops.extend_from_slice(bytes);
    Ok(())
}

fn emit_copy_diff(ops: &mut Vec<u8>, old: &[u8], new: &[u8], old_off: usize, new_off: usize, len: usize) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let len_u32 = u32::try_from(len)
        .map_err(|_| ProtocolError::Framing("copy_diff chunk exceeds u32".into()))?;
    ops.push(OP_COPY_DIFF);
    ops.extend_from_slice(&(old_off as u64).to_be_bytes());
    ops.extend_from_slice(&len_u32.to_be_bytes());
    let old_slice = &old[old_off..old_off + len];
    let new_slice = &new[new_off..new_off + len];
    ops.reserve(len);
    for i in 0..len {
        ops.push(new_slice[i].wrapping_sub(old_slice[i]));
    }
    Ok(())
}

/// Extends an exact 8-byte anchor forward across matches and minor differences (e.g. shifted
/// relocation/jump offsets) as long as at least half of each 16-byte sliding window matches.
fn extend_forward(old: &[u8], new: &[u8], old_start: usize, new_start: usize) -> usize {
    let max_len = (old.len() - old_start).min(new.len() - new_start);
    let mut ring = [false; 16];
    let mut win_matches = 0usize;
    let mut last_good_len = 0usize;
    let mut k = 0usize;

    while k < max_len {
        let eq = old[old_start + k] == new[new_start + k];
        if k < 16 {
            ring[k] = eq;
            if eq {
                win_matches += 1;
                last_good_len = k + 1;
            }
            if k == 15 && win_matches < 8 {
                break;
            }
        } else {
            let slot = k & 15;
            if ring[slot] {
                win_matches -= 1;
            }
            ring[slot] = eq;
            if eq {
                win_matches += 1;
                last_good_len = k + 1;
            }
            if win_matches < 8 {
                break;
            }
        }
        k += 1;
    }

    last_good_len
}

/// Creates a compact `FLDP` v1 binary delta patch transforming `old` into `new`.
pub fn create_delta_patch(old: &[u8], new: &[u8]) -> Result<Vec<u8>> {
    if old.len() > MAX_UNCOMPRESSED_SIZE || new.len() > MAX_UNCOMPRESSED_SIZE {
        return Err(ProtocolError::Framing(format!(
            "binary exceeds {} bytes limit",
            MAX_UNCOMPRESSED_SIZE
        )));
    }

    let new_sha256: [u8; 32] = Sha256::digest(new).into();
    let mut ops = Vec::new();

    if old.len() < 8 || new.len() < 8 {
        emit_insert(&mut ops, new)?;
    } else {
        let table = build_old_index(old);
        let mut pos = 0usize;
        let mut pending_start = 0usize;

        while pos + 8 <= new.len() {
            let cand = table[hash8(&new[pos..pos + 8])];
            if cand != EMPTY_SLOT {
                let old_pos = cand as usize;
                if old_pos + 8 <= old.len() && old[old_pos..old_pos + 8] == new[pos..pos + 8] {
                    // Extend backward across exact matches into pending literal bytes.
                    let mut m_new = pos;
                    let mut m_old = old_pos;
                    while m_new > pending_start && m_old > 0 && new[m_new - 1] == old[m_old - 1] {
                        m_new -= 1;
                        m_old -= 1;
                    }
                    let fwd = extend_forward(old, new, m_old, m_new);
                    let min_accept = if m_new + fwd == new.len() { 8 } else { 12 };
                    if fwd >= min_accept {
                        emit_insert(&mut ops, &new[pending_start..m_new])?;
                        emit_copy_diff(&mut ops, old, new, m_old, m_new, fwd)?;
                        pos = m_new + fwd;
                        pending_start = pos;
                        continue;
                    }
                }
            }
            pos += 1;
        }
        emit_insert(&mut ops, &new[pending_start..])?;
    }

    let compressed_ops = compress_full(&ops)?;
    let mut out = Vec::with_capacity(HEADER_LEN + compressed_ops.len());
    out.extend_from_slice(PATCH_MAGIC);
    out.push(PATCH_VERSION);
    out.extend_from_slice(&(old.len() as u64).to_be_bytes());
    out.extend_from_slice(&(new.len() as u64).to_be_bytes());
    out.extend_from_slice(&new_sha256);
    out.extend_from_slice(&compressed_ops);
    Ok(out)
}

/// Applies an `FLDP` v1 binary delta patch to `old` and verifies the resulting length and SHA-256.
pub fn apply_delta_patch(old: &[u8], patch: &[u8], max_out: usize) -> Result<Vec<u8>> {
    if patch.len() < HEADER_LEN {
        return Err(ProtocolError::Framing("delta patch truncated".into()));
    }
    if &patch[0..4] != PATCH_MAGIC {
        return Err(ProtocolError::Framing("invalid delta patch magic".into()));
    }
    if patch[4] != PATCH_VERSION {
        return Err(ProtocolError::Framing(format!(
            "unsupported delta patch version: {}",
            patch[4]
        )));
    }
    let expected_old_len = u64::from_be_bytes(patch[5..13].try_into().unwrap()) as usize;
    let expected_new_len = u64::from_be_bytes(patch[13..21].try_into().unwrap()) as usize;
    let expected_sha: [u8; 32] = patch[21..53].try_into().unwrap();

    if old.len() != expected_old_len {
        return Err(ProtocolError::Framing(format!(
            "base file size mismatch: got {}, expected {}",
            old.len(),
            expected_old_len
        )));
    }
    let cap = max_out.min(MAX_UNCOMPRESSED_SIZE);
    if expected_new_len > cap {
        return Err(ProtocolError::Framing(format!(
            "patched output size {} exceeds cap {}",
            expected_new_len, cap
        )));
    }

    let ops = decompress_full(&patch[HEADER_LEN..], expected_new_len.saturating_mul(2).saturating_add(4096))?;
    let mut out = Vec::with_capacity(expected_new_len);
    let mut cur = ops.as_slice();

    while !cur.is_empty() {
        let op = cur[0];
        cur = &cur[1..];
        match op {
            OP_INSERT => {
                if cur.len() < 4 {
                    return Err(ProtocolError::Framing("truncated INSERT length".into()));
                }
                let len = u32::from_be_bytes(cur[..4].try_into().unwrap()) as usize;
                cur = &cur[4..];
                if cur.len() < len || out.len().saturating_add(len) > expected_new_len {
                    return Err(ProtocolError::Framing("INSERT exceeds bounds".into()));
                }
                out.extend_from_slice(&cur[..len]);
                cur = &cur[len..];
            }
            OP_COPY_DIFF => {
                if cur.len() < 12 {
                    return Err(ProtocolError::Framing("truncated COPY_DIFF header".into()));
                }
                let old_off = u64::from_be_bytes(cur[..8].try_into().unwrap()) as usize;
                let len = u32::from_be_bytes(cur[8..12].try_into().unwrap()) as usize;
                cur = &cur[12..];
                if cur.len() < len
                    || old_off.saturating_add(len) > old.len()
                    || out.len().saturating_add(len) > expected_new_len
                {
                    return Err(ProtocolError::Framing("COPY_DIFF exceeds bounds".into()));
                }
                let old_slice = &old[old_off..old_off + len];
                let res_slice = &cur[..len];
                out.reserve(len);
                for i in 0..len {
                    out.push(old_slice[i].wrapping_add(res_slice[i]));
                }
                cur = &cur[len..];
            }
            other => {
                return Err(ProtocolError::Framing(format!(
                    "unknown delta opcode: {other}"
                )));
            }
        }
    }

    if out.len() != expected_new_len {
        return Err(ProtocolError::Framing(format!(
            "patched size mismatch: got {}, expected {}",
            out.len(),
            expected_new_len
        )));
    }
    let got_sha: [u8; 32] = Sha256::digest(&out).into();
    if got_sha != expected_sha {
        return Err(ProtocolError::Framing(
            "patched output SHA-256 verification failed".into(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_full_and_delta_edge_cases() {
        for (old, new) in [
            (Vec::<u8>::new(), Vec::<u8>::new()),
            (vec![1, 2, 3], vec![1, 2, 3]),
            (b"hello world".to_vec(), b"hello flashdesk world!".to_vec()),
            (vec![0xAA; 64], Vec::new()),
        ] {
            let full_z = compress_full(&new).unwrap();
            assert_eq!(decompress_full(&full_z, MAX_UNCOMPRESSED_SIZE).unwrap(), new);

            let patch = create_delta_patch(&old, &new).unwrap();
            let patched = apply_delta_patch(&old, &patch, MAX_UNCOMPRESSED_SIZE).unwrap();
            assert_eq!(patched, new);
        }
    }

    #[test]
    fn delta_patch_on_2mib_executable_like_binary_is_tiny() {
        let size = 2 * 1024 * 1024;
        let mut old = Vec::with_capacity(size);
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        for i in 0..size {
            if i % 64 < 8 {
                old.push(0); // simulated alignment padding
            } else {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                old.push((state >> 56) as u8);
            }
        }

        // Insert a 4 KiB block near the middle + mutate 200 4-byte jump offsets across the binary.
        let split = size / 2;
        let mut new = Vec::with_capacity(size + 4096);
        new.extend_from_slice(&old[..split]);
        for j in 0..4096 {
            new.push(((j * 131 + 17) & 0xFF) as u8);
        }
        new.extend_from_slice(&old[split..]);
        for k in 0..200 {
            let off = 128 + k * 9000;
            if off + 4 <= new.len() {
                new[off] = new[off].wrapping_add(0x10);
                new[off + 1] = new[off + 1].wrapping_add(0x01);
            }
        }

        let full_compressed = compress_full(&new).unwrap();
        let patch = create_delta_patch(&old, &new).unwrap();
        let restored = apply_delta_patch(&old, &patch, MAX_UNCOMPRESSED_SIZE).unwrap();
        assert_eq!(restored, new);
        assert!(
            patch.len() * 4 < full_compressed.len(),
            "expected delta ({} bytes) < 25% of compressed full ({} bytes)",
            patch.len(),
            full_compressed.len()
        );
    }

    #[test]
    fn rejects_wrong_base_or_tampered_patch() {
        let old = vec![42u8; 4096];
        let mut new = old.clone();
        new[100..120].fill(99);
        let mut patch = create_delta_patch(&old, &new).unwrap();

        let mut wrong_old = old.clone();
        wrong_old[500] ^= 1;
        assert!(apply_delta_patch(&wrong_old, &patch, MAX_UNCOMPRESSED_SIZE).is_err());

        let last = patch.len() - 1;
        patch[last] ^= 0xFF;
        assert!(apply_delta_patch(&old, &patch, MAX_UNCOMPRESSED_SIZE).is_err());
    }
}
