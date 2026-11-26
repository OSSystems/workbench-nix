//! A stopped workspace's disk as a stream, from one node to another
//! through the front: its data only, holes skipped (`SEEK_DATA` and
//! `SEEK_HOLE`), then the stream's sha256.
//!
//! ```text
//! stream = "WBDISK1\n" size frame* end hash
//! size   = u64, big-endian: the file's length
//! frame  = offset length data: u64 and u64, big-endian, then `length`
//!          bytes, 1 to 1 MiB, at offsets that only grow
//! end    = offset u64::MAX, length 0
//! hash   = the sha256 of every byte before it, 32 bytes
//! ```
//!
//! What isn't in a frame is a hole in the file that receives it.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt as _;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use axum::body::Bytes;
use futures_util::{Stream, StreamExt};
use nix::errno::Errno;
use nix::unistd::{Whence, lseek};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// What a stream starts with.
pub const MAGIC: &[u8; 8] = b"WBDISK1\n";
/// The most data one frame carries.
pub const FRAME: u64 = 1 << 20;
/// The end frame's offset.
const END: u64 = u64::MAX;
/// How long a stream may stay silent before it counts as broken: a node
/// that lost power closes nothing.
pub const IDLE: std::time::Duration = std::time::Duration::from_secs(120);

/// A sha256, in hex.
#[must_use]
pub fn hex(hash: &[u8]) -> String {
    hash.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = std::fmt::Write::write_fmt(&mut s, format_args!("{b:02x}"));
        s
    })
}

/// The next data of a file at or after `from`, up to its next hole.
fn next_data(f: &File, from: u64, size: u64) -> io::Result<Option<(u64, u64)>> {
    if from >= size {
        return Ok(None);
    }
    let at = i64::try_from(from).map_err(|_| io::Error::other("offset out of range"))?;
    let data = match lseek(f, at, Whence::SeekData) {
        Ok(d) => u64::try_from(d).unwrap_or(size),
        Err(Errno::ENXIO) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if data >= size {
        return Ok(None);
    }
    let hole = u64::try_from(lseek(f, i64::try_from(data).unwrap_or(i64::MAX), Whence::SeekHole)?).unwrap_or(size);
    Ok(Some((data, hole.min(size))))
}

/// Sends a file as a stream through `tx`, a frame at a time, and returns
/// the stream's sha256. It blocks: run it on a thread of its own.
pub fn send(path: &Path, tx: &mpsc::Sender<Bytes>) -> anyhow::Result<[u8; 32]> {
    let f = File::open(path).with_context(|| path.display().to_string())?;
    let size = f.metadata()?.len();
    let mut hasher = Sha256::new();
    let mut emit = |chunk: Vec<u8>| -> anyhow::Result<()> {
        hasher.update(&chunk);
        tx.blocking_send(Bytes::from(chunk)).map_err(|_| anyhow::anyhow!("the receiver went away"))
    };
    let mut head = MAGIC.to_vec();
    head.extend(size.to_be_bytes());
    emit(head)?;
    let mut pos = 0;
    while let Some((data, hole)) = next_data(&f, pos, size)? {
        let mut at = data;
        while at < hole {
            let len = (hole - at).min(FRAME);
            let mut frame = Vec::with_capacity(16 + usize::try_from(len)?);
            frame.extend(at.to_be_bytes());
            frame.extend(len.to_be_bytes());
            let start = frame.len();
            frame.resize(start + usize::try_from(len)?, 0);
            f.read_exact_at(&mut frame[start..], at)?;
            emit(frame)?;
            at += len;
        }
        pos = hole;
    }
    let mut end = END.to_be_bytes().to_vec();
    end.extend(0u64.to_be_bytes());
    emit(end)?;
    let hash: [u8; 32] = hasher.finalize().into();
    tx.blocking_send(Bytes::copy_from_slice(&hash)).map_err(|_| anyhow::anyhow!("the receiver went away"))?;
    Ok(hash)
}

/// Reads exactly what is asked of a stream of chunks.
struct Reader<S> {
    input: S,
    buf: Vec<u8>,
    start: usize,
    idle: std::time::Duration,
}

impl<S, E> Reader<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    async fn next(&mut self) -> anyhow::Result<Option<Result<Bytes, E>>> {
        tokio::time::timeout(self.idle, self.input.next())
            .await
            .map_err(|_| anyhow::anyhow!("the stream said nothing for {}s", self.idle.as_secs()))
    }

    async fn take(&mut self, n: usize) -> anyhow::Result<&[u8]> {
        while self.buf.len() - self.start < n {
            match self.next().await? {
                Some(Ok(chunk)) => {
                    if self.start > 0 {
                        self.buf.drain(..self.start);
                        self.start = 0;
                    }
                    self.buf.extend_from_slice(&chunk);
                }
                Some(Err(e)) => bail!("the stream broke: {e}"),
                None => bail!("the stream ended early"),
            }
        }
        let at = self.start;
        self.start += n;
        Ok(&self.buf[at..at + n])
    }

    async fn u64(&mut self, hasher: &mut Sha256) -> anyhow::Result<u64> {
        let b = self.take(8).await?;
        hasher.update(b);
        Ok(u64::from_be_bytes(b.try_into()?))
    }

    async fn at_end(&mut self) -> anyhow::Result<bool> {
        if self.buf.len() > self.start {
            return Ok(false);
        }
        loop {
            match self.next().await? {
                Some(Ok(chunk)) if chunk.is_empty() => {}
                Some(Ok(chunk)) => {
                    self.buf.extend_from_slice(&chunk);
                    return Ok(false);
                }
                Some(Err(e)) => bail!("the stream broke: {e}"),
                None => return Ok(true),
            }
        }
    }
}

/// Receives a stream into a new file at `path`, its holes kept, and
/// returns its sha256 once it matches the one the stream ends with. A
/// stream that doesn't, or breaks, leaves no file.
pub async fn receive<S, E>(input: S, path: &Path) -> anyhow::Result<[u8; 32]>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    receive_within(input, path, IDLE).await
}

/// [`receive`], with a stream that may be silent for `idle` at most.
pub async fn receive_within<S, E>(input: S, path: &Path, idle: std::time::Duration) -> anyhow::Result<[u8; 32]>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let got = receive_into(input, path, idle).await;
    if got.is_err() {
        let _ = std::fs::remove_file(path);
    }
    got
}

async fn receive_into<S, E>(input: S, path: &Path, idle: std::time::Duration) -> anyhow::Result<[u8; 32]>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut r = Reader { input, buf: Vec::new(), start: 0, idle };
    let mut hasher = Sha256::new();
    let magic = r.take(MAGIC.len()).await?;
    ensure!(magic == MAGIC, "not a disk stream");
    hasher.update(MAGIC);
    let size = r.u64(&mut hasher).await?;
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .with_context(|| path.display().to_string())?;
    f.set_len(size).await?;
    let mut next = 0;
    loop {
        let offset = r.u64(&mut hasher).await?;
        let len = r.u64(&mut hasher).await?;
        if offset == END {
            ensure!(len == 0, "a malformed end frame");
            break;
        }
        ensure!((1..=FRAME).contains(&len), "a frame of {len} bytes");
        ensure!(offset >= next && offset.checked_add(len).is_some_and(|e| e <= size), "a frame out of place");
        let data = r.take(usize::try_from(len)?).await?;
        hasher.update(data);
        f.seek(io::SeekFrom::Start(offset)).await?;
        f.write_all(data).await?;
        next = offset + len;
    }
    let computed: [u8; 32] = hasher.finalize().into();
    let claimed = r.take(32).await?;
    ensure!(claimed == computed, "the stream's sha256 is not its data's");
    ensure!(r.at_end().await?, "the stream goes on after its sha256");
    f.sync_all().await?;
    Ok(computed)
}

/// What the front checks of a stream it passes on: the sha256 of all but
/// its last 32 bytes, which must be them.
#[derive(Default)]
pub struct Tail {
    hasher: Sha256,
    held: Vec<u8>,
}

impl Tail {
    pub fn push(&mut self, chunk: &[u8]) {
        self.held.extend_from_slice(chunk);
        if self.held.len() > 32 {
            let cut = self.held.len() - 32;
            self.hasher.update(&self.held[..cut]);
            self.held.drain(..cut);
        }
    }

    /// The stream's sha256, if its end says the same.
    pub fn finish(self) -> anyhow::Result<[u8; 32]> {
        ensure!(self.held.len() == 32, "the stream ended early");
        let computed: [u8; 32] = self.hasher.finalize().into();
        ensure!(self.held == computed, "the stream's sha256 is not its data's");
        Ok(computed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt as _;

    const MIB: u64 = 1 << 20;
    const MIBS: usize = 1 << 20;

    /// Sends a file and returns every chunk of the stream.
    async fn stream_of(path: &Path) -> (Vec<Bytes>, [u8; 32]) {
        let (tx, mut rx) = mpsc::channel(4);
        let path = path.to_owned();
        let sender = tokio::task::spawn_blocking(move || send(&path, &tx));
        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }
        (chunks, sender.await.unwrap().unwrap())
    }

    fn as_stream(chunks: Vec<Bytes>) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Unpin {
        futures_util::stream::iter(chunks.into_iter().map(Ok))
    }

    /// A sparse 64 MiB disk travels as its few data blocks and its hash,
    /// and lands with its holes, byte for byte the same.
    #[tokio::test]
    async fn a_sparse_disk_and_its_hash() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("disk.raw");
        let f = File::create(&src).unwrap();
        f.set_len(64 * MIB).unwrap();
        f.write_all_at(&[b'a'; 4096], 0).unwrap();
        f.write_all_at(&vec![b'b'; 3 * MIBS], 10 * MIB).unwrap();
        f.write_all_at(b"z", 64 * MIB - 1).unwrap();
        f.sync_all().unwrap();
        let (chunks, sent) = stream_of(&src).await;
        let total: usize = chunks.iter().map(Bytes::len).sum();
        assert!(total < 4 * MIBS + 64 * 1024, "the holes travelled: {total} bytes");
        assert!(chunks.iter().all(|c| c.len() <= MIBS + 16), "a frame over a MiB");
        let mut tail = Tail::default();
        for c in &chunks {
            tail.push(c);
        }
        assert_eq!(tail.finish().unwrap(), sent, "the front's hash is the stream's");
        // Received in pieces that cut across the frames.
        let flat: Vec<u8> = chunks.concat();
        let pieces: Vec<Bytes> = flat.chunks(7777).map(Bytes::copy_from_slice).collect();
        let dst = dir.path().join("copy.raw");
        let got = receive(as_stream(pieces), &dst).await.unwrap();
        assert_eq!(got, sent);
        assert_eq!(hex(&got).len(), 64);
        assert_eq!(std::fs::read(&src).unwrap(), std::fs::read(&dst).unwrap(), "not the same disk");
        let used = std::fs::metadata(&dst).unwrap().blocks() * 512;
        assert!(used < 8 * MIB, "the holes were filled: {used} bytes used");
        assert!(receive(as_stream(chunks.clone()), &dst).await.is_err(), "a disk that is there is not replaced");
    }

    #[tokio::test]
    async fn a_bad_stream_leaves_no_disk() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("disk.raw");
        std::fs::write(&src, vec![7u8; 100_000]).unwrap();
        let (chunks, _) = stream_of(&src).await;
        let flat: Vec<u8> = chunks.concat();
        let dst = dir.path().join("copy.raw");
        let mut tampered = flat.clone();
        tampered[40] ^= 1;
        let truncated = flat[..flat.len() - 5].to_vec();
        let mut longer = flat.clone();
        longer.push(0);
        for (what, bytes) in [("tampered", tampered.clone()), ("truncated", truncated), ("longer", longer)] {
            let e = receive(as_stream(vec![Bytes::from(bytes)]), &dst).await.unwrap_err();
            assert!(!dst.exists(), "{what}: a disk was left: {e}");
        }
        let mut tail = Tail::default();
        tail.push(&tampered);
        assert!(tail.finish().is_err(), "the front passed a tampered stream");
        // A source gone silent half way, as one that lost power is.
        let silent = futures_util::stream::iter([Ok::<_, std::convert::Infallible>(chunks[0].clone())])
            .chain(futures_util::stream::pending());
        let idle = std::time::Duration::from_millis(50);
        let waited = tokio::time::timeout(std::time::Duration::from_secs(5), receive_within(silent, &dst, idle)).await;
        assert!(waited.expect("it waited for good").is_err() && !dst.exists());
        // An empty disk is a stream too.
        let empty = dir.path().join("empty.raw");
        File::create(&empty).unwrap();
        let (chunks, sent) = stream_of(&empty).await;
        assert_eq!(receive(as_stream(chunks), &dst).await.unwrap(), sent);
        assert_eq!(std::fs::metadata(&dst).unwrap().len(), 0);
    }
}
