//! Streamed zip archives: STORE only (photos and PDFs don't shrink), ZIP64 where sizes or
//! offsets need it, and data descriptors, so the CRC-32 is computed while the bytes go out and
//! nothing is read twice. Not resumable: a dropped transfer starts again.
//!
//! The layout follows Go's `archive/zip` writer, which `unzip`, Python and the usual desktop
//! tools read: a local header with zeros for CRC and sizes (bit 3 set), the data, a data
//! descriptor, and a central directory with the real values at the end.

use bytes::{BufMut, Bytes, BytesMut};
use futures_util::StreamExt;

use crate::policy::CheckedPath;
use crate::storage::{ByteStream, Storage, StorageError};
use crate::time;

const LOCAL: u32 = 0x0403_4b50;
const DESCRIPTOR: u32 = 0x0807_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const END: u32 = 0x0605_4b50;
const END64: u32 = 0x0606_4b50;
const LOCATOR64: u32 = 0x0706_4b50;
/// Bit 3 (sizes and CRC follow the data) and bit 11 (names are UTF-8).
const FLAGS_FILE: u16 = 0x0808;
const FLAGS_DIR: u16 = 0x0800;
const MAX32: u64 = 0xFFFF_FFFF;
/// Made by Unix (3), spec 4.5, so external attributes carry Unix modes.
const MADE_BY: u16 = (3 << 8) | 45;

/// One entry of the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipItem {
    pub path: CheckedPath,
    /// The name inside the archive, `/`-separated, no leading slash; folders end with `/`.
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<i64>,
}

struct Central {
    name: String,
    is_dir: bool,
    crc: u32,
    size: u64,
    offset: u64,
    time: u16,
    date: u16,
}

fn dos_time(mtime: Option<i64>) -> (u16, u16) {
    // DOS dates start in 1980; anything earlier (or unknown) shows as 1980-01-01.
    let ts = mtime.unwrap_or(0).max(315_532_800);
    let (y, mo, d, h, mi, s) = time::civil(ts);
    let year = (y - 1980).clamp(0, 127) as u16;
    let date = (year << 9) | ((mo as u16) << 5) | d as u16;
    let time = ((h as u16) << 11) | ((mi as u16) << 5) | (s as u16 / 2);
    (time, date)
}

fn local_header(item: &ZipItem, time: u16, date: u16) -> Bytes {
    let zip64 = item.size >= MAX32;
    let mut b = BytesMut::with_capacity(30 + item.name.len() + 20);
    b.put_u32_le(LOCAL);
    b.put_u16_le(if zip64 { 45 } else { 20 });
    b.put_u16_le(if item.is_dir { FLAGS_DIR } else { FLAGS_FILE });
    b.put_u16_le(0); // stored
    b.put_u16_le(time);
    b.put_u16_le(date);
    b.put_u32_le(0); // crc, in the descriptor
    // A ZIP64 entry says so with 0xFFFFFFFF and a ZIP64 extra field (zeros until the
    // descriptor), which is what tells readers its descriptor holds 8-byte sizes.
    let unknown = if zip64 { MAX32 as u32 } else { 0 };
    b.put_u32_le(unknown);
    b.put_u32_le(unknown);
    b.put_u16_le(item.name.len() as u16);
    b.put_u16_le(if zip64 { 20 } else { 0 });
    b.put_slice(item.name.as_bytes());
    if zip64 {
        b.put_u16_le(0x0001);
        b.put_u16_le(16);
        b.put_u64_le(0);
        b.put_u64_le(0);
    }
    b.freeze()
}

fn descriptor(crc: u32, size: u64) -> Bytes {
    let mut b = BytesMut::with_capacity(24);
    b.put_u32_le(DESCRIPTOR);
    b.put_u32_le(crc);
    if size >= MAX32 {
        b.put_u64_le(size);
        b.put_u64_le(size);
    } else {
        b.put_u32_le(size as u32);
        b.put_u32_le(size as u32);
    }
    b.freeze()
}

fn central_directory(entries: &[Central], start: u64) -> Bytes {
    let mut b = BytesMut::new();
    for e in entries {
        let big_size = e.size >= MAX32;
        let big_offset = e.offset >= MAX32;
        let mut extra = BytesMut::new();
        if big_size || big_offset {
            let mut fields = BytesMut::new();
            if big_size {
                fields.put_u64_le(e.size);
                fields.put_u64_le(e.size);
            }
            if big_offset {
                fields.put_u64_le(e.offset);
            }
            extra.put_u16_le(0x0001);
            extra.put_u16_le(fields.len() as u16);
            extra.put_slice(&fields);
        }
        b.put_u32_le(CENTRAL);
        b.put_u16_le(MADE_BY);
        b.put_u16_le(if big_size || big_offset { 45 } else { 20 });
        b.put_u16_le(if e.is_dir { FLAGS_DIR } else { FLAGS_FILE });
        b.put_u16_le(0);
        b.put_u16_le(e.time);
        b.put_u16_le(e.date);
        b.put_u32_le(e.crc);
        let size32 = if big_size {
            MAX32 as u32
        } else {
            e.size as u32
        };
        b.put_u32_le(size32);
        b.put_u32_le(size32);
        b.put_u16_le(e.name.len() as u16);
        b.put_u16_le(extra.len() as u16);
        b.put_u16_le(0); // comment
        b.put_u16_le(0); // disk
        b.put_u16_le(0); // internal attributes
        let mode: u32 = if e.is_dir { 0o040_755 } else { 0o100_644 };
        b.put_u32_le((mode << 16) | (u32::from(e.is_dir) * 0x10));
        b.put_u32_le(if big_offset {
            MAX32 as u32
        } else {
            e.offset as u32
        });
        b.put_slice(e.name.as_bytes());
        b.put_slice(&extra);
    }
    let size = b.len() as u64;
    let count = entries.len() as u64;
    let end64_at = start + size;
    if count >= 0xFFFF || size >= MAX32 || start >= MAX32 {
        b.put_u32_le(END64);
        b.put_u64_le(44);
        b.put_u16_le(MADE_BY);
        b.put_u16_le(45);
        b.put_u32_le(0);
        b.put_u32_le(0);
        b.put_u64_le(count);
        b.put_u64_le(count);
        b.put_u64_le(size);
        b.put_u64_le(start);
        b.put_u32_le(LOCATOR64);
        b.put_u32_le(0);
        b.put_u64_le(end64_at);
        b.put_u32_le(1);
    }
    b.put_u32_le(END);
    b.put_u16_le(0);
    b.put_u16_le(0);
    let count16 = count.min(0xFFFF) as u16;
    b.put_u16_le(count16);
    b.put_u16_le(count16);
    b.put_u32_le(size.min(MAX32) as u32);
    b.put_u32_le(start.min(MAX32) as u32);
    b.put_u16_le(0);
    b.freeze()
}

/// The archive of `items`, read from `storage` one file at a time as the stream is polled. A
/// file whose size differs from what `items` says (it changed since) ends the stream with an
/// error, so the client sees a broken transfer rather than a quietly wrong archive.
pub fn zip_stream<S: Storage + 'static>(storage: S, items: Vec<ZipItem>) -> ByteStream {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Bytes, StorageError>>(4);
    tokio::spawn(async move {
        let mut offset: u64 = 0;
        let mut central = Vec::with_capacity(items.len());
        for item in &items {
            let (time, date) = dos_time(item.mtime);
            let header = local_header(item, time, date);
            let at = offset;
            offset += header.len() as u64;
            if tx.send(Ok(header)).await.is_err() {
                return;
            }
            let mut crc = crc32fast::Hasher::new();
            let mut size: u64 = 0;
            if !item.is_dir {
                let mut body = match storage.open(&item.path, None).await {
                    Ok(body) => body,
                    Err(err) => {
                        let _ = tx.send(Err(err)).await;
                        return;
                    }
                };
                while let Some(chunk) = body.next().await {
                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(err) => {
                            let _ = tx.send(Err(err)).await;
                            return;
                        }
                    };
                    crc.update(&chunk);
                    size += chunk.len() as u64;
                    if tx.send(Ok(chunk)).await.is_err() {
                        return;
                    }
                }
                if size != item.size {
                    let _ = tx
                        .send(Err(StorageError::Backend(format!(
                            "{} changed while it was being zipped ({} bytes, expected {})",
                            item.path, size, item.size
                        ))))
                        .await;
                    return;
                }
                offset += size;
                let desc = descriptor(crc.clone().finalize(), size);
                offset += desc.len() as u64;
                if tx.send(Ok(desc)).await.is_err() {
                    return;
                }
            }
            central.push(Central {
                name: item.name.clone(),
                is_dir: item.is_dir,
                crc: if item.is_dir { 0 } else { crc.finalize() },
                size,
                offset: at,
                time,
                date,
            });
        }
        let _ = tx.send(Ok(central_directory(&central, offset))).await;
    });
    Box::pin(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)))
}

/// The exact archive size for `items`, when every file keeps its size.
pub fn zip_size(items: &[ZipItem]) -> u64 {
    let mut offset = 0u64;
    let mut central = Vec::with_capacity(items.len());
    for item in items {
        let header = local_header(item, 0, 0).len() as u64;
        central.push(Central {
            name: item.name.clone(),
            is_dir: item.is_dir,
            crc: 0,
            size: if item.is_dir { 0 } else { item.size },
            offset,
            time: 0,
            date: 0,
        });
        offset += header;
        if !item.is_dir {
            offset += item.size + descriptor(0, item.size).len() as u64;
        }
    }
    offset + central_directory(&central, offset).len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dos_times() {
        // 2026-10-31T13:45:30Z
        let (time, date) = dos_time(Some(1_793_454_330));
        assert_eq!(date >> 9, 46);
        assert_eq!((date >> 5) & 0xF, 10);
        assert_eq!(date & 0x1F, 31);
        assert_eq!(time >> 11, 13);
        assert_eq!((time >> 5) & 0x3F, 45);
        assert_eq!((time & 0x1F) * 2, 30);
        assert_eq!(dos_time(None), dos_time(Some(0)));
    }

    #[test]
    fn zip64_only_where_needed() {
        let policy = crate::Policy::default();
        let small = ZipItem {
            path: policy.check("/a").unwrap(),
            name: "a".into(),
            is_dir: false,
            size: 10,
            mtime: None,
        };
        assert_eq!(local_header(&small, 0, 0).len(), 31);
        let big = ZipItem {
            size: 5 << 30,
            ..small.clone()
        };
        assert_eq!(local_header(&big, 0, 0).len(), 51);
        assert_eq!(descriptor(0, 10).len(), 16);
        assert_eq!(descriptor(0, 5 << 30).len(), 24);
        // 31 + 10 + 16 local, 47 central entry, 22 end.
        assert_eq!(zip_size(&[small]), 31 + 10 + 16 + 47 + 22);
    }
}
