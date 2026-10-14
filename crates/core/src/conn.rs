// SPDX-License-Identifier: GPL-3.0-only
//! Async RTMP connection plumbing: handshakes plus message reader/writer halves
//! over any byte stream (TCP or TLS).

use opendelay_rtmp::{ChunkReader, ChunkWriter, Message, handshake, msg_type};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};

pub trait Io: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Io for T {}
pub type BoxIo = Box<dyn Io>;

/// Chunk size for everything we send (both legs).
pub const OUT_CHUNK_SIZE: u32 = 4096;
/// Flush the writer once this many bytes are queued.
const FLUSH_AT: usize = 64 * 1024;

fn invalid(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

pub async fn server_handshake(io: &mut BoxIo, uptime_ms: u32) -> io::Result<()> {
    let mut c0c1 = vec![0u8; 1 + handshake::SIZE];
    io.read_exact(&mut c0c1).await?;
    io.write_all(&handshake::server_s0s1s2(&c0c1, uptime_ms).map_err(invalid)?).await?;
    io.flush().await?;
    let mut c2 = vec![0u8; handshake::SIZE];
    io.read_exact(&mut c2).await?;
    Ok(())
}

pub async fn client_handshake(io: &mut BoxIo) -> io::Result<()> {
    io.write_all(&handshake::client_c0c1()).await?;
    io.flush().await?;
    let mut s0s1 = vec![0u8; 1 + handshake::SIZE];
    io.read_exact(&mut s0s1).await?;
    io.write_all(&handshake::client_c2(&s0s1).map_err(invalid)?).await?;
    io.flush().await?;
    let mut s2 = vec![0u8; handshake::SIZE];
    io.read_exact(&mut s2).await?;
    Ok(())
}

pub struct MsgReader {
    io: ReadHalf<BoxIo>,
    chunks: ChunkReader,
    buf: Vec<u8>,
    /// Acknowledgement window requested by the peer (0 = none).
    window: u32,
    acked: u64,
}

impl MsgReader {
    /// Next message; `Ok(None)` on a clean end of stream. Cancel-safe.
    pub async fn read(&mut self) -> io::Result<Option<Message>> {
        loop {
            if let Some(m) = self.chunks.next_message().map_err(invalid)? {
                if m.type_id == msg_type::WINDOW_ACK_SIZE {
                    self.window = m.u32_arg().unwrap_or(0);
                }
                return Ok(Some(m));
            }
            let n = self.io.read(&mut self.buf).await?;
            if n == 0 {
                return Ok(None);
            }
            self.chunks.feed(&self.buf[..n]);
        }
    }

    /// Acknowledgement to send once the peer's window has been received.
    pub fn ack_due(&mut self) -> Option<Message> {
        let received = self.chunks.bytes_received();
        if self.window > 0 && received - self.acked >= self.window as u64 {
            self.acked = received;
            return Some(Message::ack(received as u32));
        }
        None
    }

    pub fn bytes_received(&self) -> u64 {
        self.chunks.bytes_received()
    }
}

pub struct MsgWriter {
    io: WriteHalf<BoxIo>,
    chunks: ChunkWriter,
    out: Vec<u8>,
    sent: u64,
}

/// Conventional chunk stream ids: control 2, commands 3, audio 4, data 5, video 6.
fn csid_for(type_id: u8) -> u32 {
    match type_id {
        1..=6 => 2,
        msg_type::AUDIO => 4,
        msg_type::DATA_AMF0 | msg_type::DATA_AMF3 => 5,
        msg_type::VIDEO => 6,
        _ => 3,
    }
}

impl MsgWriter {
    pub fn queue(&mut self, msg: &Message) {
        self.chunks.write(&mut self.out, csid_for(msg.type_id), msg);
    }

    pub fn queue_chunk_size(&mut self, size: u32) {
        self.chunks.write_set_chunk_size(&mut self.out, size);
    }

    /// Next message on every chunk stream gets a full header.
    pub fn reset_headers(&mut self) {
        self.chunks.reset();
    }

    pub fn should_flush(&self) -> bool {
        self.out.len() >= FLUSH_AT
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        if !self.out.is_empty() {
            self.io.write_all(&self.out).await?;
            self.sent += self.out.len() as u64;
            self.out.clear();
        }
        self.io.flush().await
    }

    pub async fn send(&mut self, msg: &Message) -> io::Result<()> {
        self.queue(msg);
        self.flush().await
    }

    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.flush().await?;
        self.io.shutdown().await
    }

    pub fn bytes_sent(&self) -> u64 {
        self.sent
    }
}

pub fn split(io: BoxIo) -> (MsgReader, MsgWriter) {
    let (r, w) = tokio::io::split(io);
    (
        MsgReader { io: r, chunks: ChunkReader::new(), buf: vec![0; 64 * 1024], window: 0, acked: 0 },
        MsgWriter { io: w, chunks: ChunkWriter::new(), out: Vec::new(), sent: 0 },
    )
}
