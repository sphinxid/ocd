//! TCP relays of one desktop attachment (plugin-browser/src/tunnel.ts). The desktop's proxy adapts
//! Chromium's HTTP/CONNECT traffic to `tunnel.open/read/write/close`, so pages browse through the
//! server's network. Reads hand out at most one 64 KiB chunk at a time; the socket buffers provide the
//! backpressure.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio_util::sync::CancellationToken;

use super::TUNNEL_CHUNK_BYTES;

const LIMIT: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// `Browser.TunnelRead`.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub data: Vec<u8>,
    pub eof: bool,
}

struct Entry {
    reader: tokio::sync::Mutex<Option<OwnedReadHalf>>,
    writer: tokio::sync::Mutex<Option<OwnedWriteHalf>>,
    reading: AtomicBool,
    /// The socket's `error` (its message), reported by every later read.
    error: Mutex<Option<String>>,
    write_ended: AtomicBool,
    read_ended: AtomicBool,
    /// `socket.destroy()`: closed by `close`, disposal, or a failed connect.
    destroyed: CancellationToken,
}

impl Entry {
    fn destroy(&self) {
        self.destroyed.cancel();
    }
}

/// The sockets of one attachment.
#[derive(Default)]
pub struct Tunnels {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    disposed: bool,
    sockets: HashMap<String, Arc<Entry>>,
}

/// Clears a pending read's flag when the read ends or is dropped (`cleanup`).
struct Reading<'a>(&'a AtomicBool);

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Tunnels {
    pub fn new() -> Self {
        Self::default()
    }

    /// `open(target)`: connects to `host:port` (10 s), returning the tunnel ID.
    pub async fn open(&self, host: &str, port: u16) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let entry = Arc::new(Entry {
            reader: tokio::sync::Mutex::new(None),
            writer: tokio::sync::Mutex::new(None),
            reading: AtomicBool::new(false),
            error: Mutex::new(None),
            write_ended: AtomicBool::new(false),
            read_ended: AtomicBool::new(false),
            destroyed: CancellationToken::new(),
        });
        {
            let mut inner = self.inner.lock();
            if inner.disposed {
                return Err("Browser attachment is closed.".into());
            }
            if inner.sockets.len() >= LIMIT {
                return Err(format!("Browser attachment has reached its {LIMIT}-connection limit."));
            }
            inner.sockets.insert(id.clone(), entry.clone());
        }
        let connected = tokio::select! {
            result = tokio::time::timeout(CONNECT_TIMEOUT, connect(host, port)) => match result {
                Ok(result) => result,
                Err(_) => Err("Browser tunnel target connection timed out.".to_string()),
            },
            _ = entry.destroyed.cancelled() => Err("Browser tunnel closed while connecting.".to_string()),
        };
        match connected {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                let (read, write) = stream.into_split();
                *entry.reader.lock().await = Some(read);
                *entry.writer.lock().await = Some(write);
                if entry.destroyed.is_cancelled() {
                    return Err("Browser tunnel closed while connecting.".into());
                }
                Ok(id)
            }
            Err(error) => {
                self.close(&id);
                Err(error)
            }
        }
    }

    fn entry(&self, id: &str) -> Option<Arc<Entry>> {
        self.inner.lock().sockets.get(id).cloned()
    }

    /// `read(id)`: the next chunk of at most 64 KiB, or `eof` once the peer ended or the tunnel closed.
    pub async fn read(&self, id: &str) -> Result<Chunk, String> {
        let entry = self.entry(id).ok_or_else(|| "Browser tunnel is closed or unknown.".to_string())?;
        if entry.reading.swap(true, Ordering::SeqCst) {
            return Err("Only one read may be pending per browser tunnel.".into());
        }
        let _reading = Reading(&entry.reading);
        if let Some(error) = entry.error.lock().clone() {
            return Err(error);
        }
        if entry.destroyed.is_cancelled() || entry.read_ended.load(Ordering::SeqCst) {
            return Ok(Chunk { data: vec![], eof: true });
        }
        let mut reader = entry.reader.lock().await;
        let Some(stream) = reader.as_mut() else { return Ok(Chunk { data: vec![], eof: true }) };
        let mut buffer = vec![0u8; TUNNEL_CHUNK_BYTES];
        tokio::select! {
            result = stream.read(&mut buffer) => match result {
                Ok(0) => {
                    entry.read_ended.store(true, Ordering::SeqCst);
                    Ok(Chunk { data: vec![], eof: true })
                }
                Ok(n) => {
                    buffer.truncate(n);
                    Ok(Chunk { data: buffer, eof: false })
                }
                Err(error) => {
                    let message = format!("read {}", super::files::errno(&error).0);
                    *entry.error.lock() = Some(message.clone());
                    entry.destroy();
                    Err(message)
                }
            },
            _ = entry.destroyed.cancelled() => Ok(Chunk { data: vec![], eof: true }),
        }
    }

    /// `write(id, data, end)`: writes the bytes, ending the write side after them when `end`.
    pub async fn write(&self, id: &str, data: &[u8], end: bool) -> Result<(), String> {
        let entry = self.entry(id).filter(|e| !e.destroyed.is_cancelled() && !e.write_ended.load(Ordering::SeqCst));
        let Some(entry) = entry else { return Err("Browser tunnel is not writable.".into()) };
        if end {
            entry.write_ended.store(true, Ordering::SeqCst);
        }
        // An interrupted write (the caller went away) closes the tunnel.
        struct Interrupted<'a>(&'a Tunnels, &'a str, bool);
        impl Drop for Interrupted<'_> {
            fn drop(&mut self) {
                if self.2 {
                    self.0.close(self.1);
                }
            }
        }
        let mut guard = Interrupted(self, id, true);
        let result = async {
            let mut writer = entry.writer.lock().await;
            let Some(stream) = writer.as_mut() else { return Err("Browser tunnel is not writable.".to_string()) };
            let written = tokio::select! {
                result = async {
                    stream.write_all(data).await?;
                    if end {
                        stream.shutdown().await?;
                    }
                    Ok::<(), std::io::Error>(())
                } => result,
                _ = entry.destroyed.cancelled() => Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
            };
            written.map_err(|error| format!("write {}", super::files::errno(&error).0))
        }
        .await;
        guard.2 = false;
        if let Err(message) = &result {
            entry.error.lock().get_or_insert_with(|| message.clone());
            entry.destroy();
        }
        result
    }

    /// `close(id)`: destroys and forgets the tunnel (unknown IDs are fine).
    pub fn close(&self, id: &str) {
        if let Some(entry) = self.inner.lock().sockets.remove(id) {
            entry.destroy();
        }
    }

    /// `dispose()`: the attachment ended; every socket closes and no new tunnel opens.
    pub fn dispose(&self) {
        let mut inner = self.inner.lock();
        inner.disposed = true;
        for (_, entry) in inner.sockets.drain() {
            entry.destroy();
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().sockets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `net.createConnection({host, port})`: the first address that accepts; errors read like Node's.
async fn connect(host: &str, port: u16) -> Result<TcpStream, String> {
    let addresses: Vec<SocketAddr> = match tokio::net::lookup_host((host, port)).await {
        Ok(addresses) => addresses.collect(),
        Err(_) => return Err(format!("getaddrinfo ENOTFOUND {host}")),
    };
    let mut last = None;
    for address in &addresses {
        match TcpStream::connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some((error, *address)),
        }
    }
    match last {
        Some((error, address)) => Err(format!("connect {} {}:{}", super::files::errno(&error).0, address.ip(), address.port())),
        None => Err(format!("getaddrinfo ENOTFOUND {host}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn relays_bounded_binary_chunks_and_half_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (mut read, mut write) = socket.into_split();
            tokio::io::copy(&mut read, &mut write).await.unwrap();
            write.shutdown().await.unwrap();
        });
        let tunnels = Arc::new(Tunnels::new());
        let id = tunnels.open("127.0.0.1", port).await.unwrap();
        let reader = {
            let tunnels = tunnels.clone();
            let id = id.clone();
            tokio::spawn(async move {
                let mut received = Vec::new();
                loop {
                    let chunk = tunnels.read(&id).await.unwrap();
                    assert!(chunk.data.len() <= TUNNEL_CHUNK_BYTES);
                    if chunk.eof {
                        return received;
                    }
                    received.extend(chunk.data);
                }
            })
        };
        let bytes = vec![203u8; TUNNEL_CHUNK_BYTES * 3 + 17];
        for chunk in bytes.chunks(TUNNEL_CHUNK_BYTES) {
            tunnels.write(&id, chunk, false).await.unwrap();
        }
        tunnels.write(&id, &[], true).await.unwrap();
        assert_eq!(reader.await.unwrap(), bytes);
        tunnels.close(&id);
        tunnels.dispose();
    }

    #[tokio::test]
    async fn cancelled_reads_release_their_flag_and_disposal_closes_sockets() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = accepted_tx.send(socket);
        });
        let tunnels = Tunnels::new();
        let id = tunnels.open("127.0.0.1", port).await.unwrap();
        let mut peer = accepted_rx.await.unwrap();
        // A read the caller abandons releases its slot.
        assert!(tokio::time::timeout(Duration::from_millis(50), tunnels.read(&id)).await.is_err());
        peer.write_all(b"still readable").await.unwrap();
        peer.shutdown().await.unwrap();
        assert_eq!(tunnels.read(&id).await.unwrap().data, b"still readable");
        assert!(tunnels.read(&id).await.unwrap().eof);
        tunnels.dispose();
        assert_eq!(tunnels.open("127.0.0.1", port).await.unwrap_err(), "Browser attachment is closed.");
        assert_eq!(tunnels.write(&id, &[1], false).await.unwrap_err(), "Browser tunnel is not writable.");
        assert_eq!(tunnels.read(&id).await.unwrap_err(), "Browser tunnel is closed or unknown.");
    }

    #[tokio::test]
    async fn refused_connections_read_like_node() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let tunnels = Tunnels::new();
        assert_eq!(tunnels.open("127.0.0.1", port).await.unwrap_err(), format!("connect ECONNREFUSED 127.0.0.1:{port}"));
        assert!(tunnels.is_empty());
    }

    #[tokio::test]
    async fn one_pending_read_per_tunnel() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let tunnels = Arc::new(Tunnels::new());
        let id = tunnels.open("127.0.0.1", port).await.unwrap();
        let pending = {
            let (tunnels, id) = (tunnels.clone(), id.clone());
            tokio::spawn(async move { tunnels.read(&id).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(tunnels.read(&id).await.unwrap_err(), "Only one read may be pending per browser tunnel.");
        // Closing the tunnel ends the pending read with eof.
        tunnels.close(&id);
        assert!(pending.await.unwrap().unwrap().eof);
    }
}
