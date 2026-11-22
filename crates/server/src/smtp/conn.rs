//! Line-oriented SMTP connection that can switch to TLS (STARTTLS).

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub enum Stream {
    Plain(TcpStream),
    ServerTls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
    ClientTls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    /// Placeholder while switching to TLS.
    Gone,
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::ServerTls(s) => Pin::new(s).poll_read(cx, buf),
            Stream::ClientTls(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Gone => Poll::Ready(Err(io::ErrorKind::NotConnected.into())),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::ServerTls(s) => Pin::new(s).poll_write(cx, buf),
            Stream::ClientTls(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Gone => Poll::Ready(Err(io::ErrorKind::NotConnected.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::ServerTls(s) => Pin::new(s).poll_flush(cx),
            Stream::ClientTls(s) => Pin::new(s).poll_flush(cx),
            Stream::Gone => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::ServerTls(s) => Pin::new(s).poll_shutdown(cx),
            Stream::ClientTls(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Gone => Poll::Ready(Ok(())),
        }
    }
}

pub struct Conn {
    pub stream: Stream,
    buf: Vec<u8>,
    pub timeout: Duration,
}

pub enum DataError {
    TooLarge,
    Io(io::Error),
}

impl Conn {
    pub fn new(stream: Stream) -> Conn {
        Conn { stream, buf: Vec::with_capacity(4096), timeout: Duration::from_secs(300) }
    }

    pub fn is_tls(&self) -> bool {
        matches!(self.stream, Stream::ServerTls(_) | Stream::ClientTls(_))
    }

    async fn fill(&mut self) -> io::Result<usize> {
        let mut chunk = [0u8; 8192];
        let n = tokio::time::timeout(self.timeout, self.stream.read(&mut chunk))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timeout"))??;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(n)
    }

    /// Next line without CRLF. `None` at EOF.
    pub async fn read_line(&mut self, max: usize) -> io::Result<Option<String>> {
        loop {
            if let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                return Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()));
            }
            if self.buf.len() > max {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
            }
            if self.fill().await? == 0 {
                return Ok(None);
            }
        }
    }

    /// Message data up to the terminating "." line, dot-unstuffed.
    pub async fn read_data(&mut self, max: usize) -> Result<Vec<u8>, DataError> {
        let mut out = Vec::with_capacity(64 * 1024);
        let mut too_large = false;
        loop {
            let line = loop {
                if let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
                    break self.buf.drain(..=pos).collect::<Vec<u8>>();
                }
                if self.buf.len() > 1_000_000 {
                    // Overlong line: take it as is.
                    break std::mem::take(&mut self.buf);
                }
                match self.fill().await {
                    Ok(0) => return Err(DataError::Io(io::ErrorKind::UnexpectedEof.into())),
                    Ok(_) => {}
                    Err(e) => return Err(DataError::Io(e)),
                }
            };
            let trimmed = line.strip_suffix(b"\n").unwrap_or(&line);
            let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
            if trimmed == b"." {
                break;
            }
            if too_large {
                continue;
            }
            let content = trimmed.strip_prefix(b".").unwrap_or(trimmed);
            out.extend_from_slice(content);
            out.extend_from_slice(b"\r\n");
            if out.len() > max {
                too_large = true;
                out.clear();
            }
        }
        if too_large { Err(DataError::TooLarge) } else { Ok(out) }
    }

    pub async fn write(&mut self, data: &[u8]) -> io::Result<()> {
        tokio::time::timeout(self.timeout, async {
            self.stream.write_all(data).await?;
            self.stream.flush().await
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timeout"))?
    }

    pub async fn reply(&mut self, text: &str) -> io::Result<()> {
        let mut s = text.replace('\n', "\r\n");
        s.push_str("\r\n");
        self.write(s.as_bytes()).await
    }

    /// Bytes received after a STARTTLS command must be discarded (RFC 3207).
    pub fn has_buffered(&self) -> bool {
        !self.buf.is_empty()
    }

    pub async fn accept_tls(&mut self, acceptor: TlsAcceptor) -> io::Result<()> {
        let Stream::Plain(tcp) = std::mem::replace(&mut self.stream, Stream::Gone) else {
            return Err(io::Error::other("already TLS"));
        };
        self.buf.clear();
        let tls = tokio::time::timeout(Duration::from_secs(30), acceptor.accept(tcp))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timeout"))??;
        self.stream = Stream::ServerTls(Box::new(tls));
        Ok(())
    }

    pub async fn connect_tls(&mut self, cfg: Arc<rustls::ClientConfig>, host: &str) -> io::Result<()> {
        let Stream::Plain(tcp) = std::mem::replace(&mut self.stream, Stream::Gone) else {
            return Err(io::Error::other("already TLS"));
        };
        self.buf.clear();
        let name = rustls::pki_types::ServerName::try_from(host.trim_end_matches('.').to_string()).map_err(io::Error::other)?;
        let tls = tokio::time::timeout(Duration::from_secs(30), TlsConnector::from(cfg).connect(name, tcp))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timeout"))??;
        self.stream = Stream::ClientTls(Box::new(tls));
        Ok(())
    }

    /// Read a (possibly multi-line) SMTP reply: (code, lines).
    pub async fn read_reply(&mut self) -> io::Result<(u16, Vec<String>)> {
        let mut lines = Vec::new();
        loop {
            let line = self.read_line(8192).await?.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
            if line.len() < 3 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad reply: {line}")));
            }
            let code: u16 = line[..3].parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("bad reply: {line}")))?;
            let more = line.as_bytes().get(3) == Some(&b'-');
            lines.push(line.get(4..).unwrap_or("").to_string());
            if !more {
                return Ok((code, lines));
            }
        }
    }
}
