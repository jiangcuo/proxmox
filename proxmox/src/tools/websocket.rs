//! Websocket helpers
//!
//! Provides methods to read and write from websockets
//! The reader and writer take a reader/writer with AsyncRead/AsyncWrite
//! respectively and provides the same

use std::pin::Pin;
use std::task::{Context, Poll};
use std::cmp::min;
use std::io::{self, ErrorKind};
use std::future::Future;

use futures::select;
use anyhow::{bail, format_err, Error};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use hyper::{Body, Response, StatusCode};
use hyper::header::{
    HeaderMap,
    HeaderValue,
    UPGRADE,
    CONNECTION,
    SEC_WEBSOCKET_KEY,
    SEC_WEBSOCKET_PROTOCOL,
    SEC_WEBSOCKET_VERSION,
    SEC_WEBSOCKET_ACCEPT,
};

use futures::future::FutureExt;
use futures::ready;

use crate::io_format_err;
use crate::tools::byte_buffer::ByteBuffer;

#[repr(u8)]
#[derive(Debug, PartialEq, PartialOrd, Copy, Clone)]
/// Represents an OpCode of a websocket frame
pub enum OpCode {
    /// A fragmented frame
    Continuation = 0,
    /// A non-fragmented text frame
    Text = 1,
    /// A non-fragmented binary frame
    Binary = 2,
    /// A closing frame
    Close = 8,
    /// A ping frame
    Ping = 9,
    /// A pong frame
    Pong = 10,
}

impl OpCode {
    /// Tells whether it is a control frame or not
    pub fn is_control(self) -> bool {
        (self as u8 & 0b1000) > 0
    }
}

fn mask_bytes(mask: Option<[u8; 4]>, data: &mut Box<[u8]>) {
    let mask = match mask {
        Some([0,0,0,0]) | None => return,
        Some(mask) => mask,
    };

    if data.len() < 32 {
        for i in 0..data.len() {
            data[i] ^= mask[i%4];
        }
        return;
    }

    let mut newmask: u32 = u32::from_le_bytes(mask);

    let (prefix, middle, suffix) = unsafe { data.align_to_mut::<u32>() };

    for p in prefix {
        *p ^= newmask as u8;
        newmask = newmask.rotate_right(8);
    }

    for m in middle {
        *m ^= newmask;
    }

    for s in suffix {
        *s ^= newmask as u8;
        newmask = newmask.rotate_right(8);
    }
}

/// Can be used to create a complete WebSocket Frame.
///
/// Takes an optional mask, the data and the frame type
///
/// Examples:
///
/// A normal Frame
/// ```
/// # use proxmox::tools::websocket::*;
/// # use std::io;
/// # fn main() -> io::Result<()> {
/// let data = vec![0,1,2,3,4];
/// let frame = create_frame(None, &data, OpCode::Text)?;
/// assert_eq!(frame, vec![0b10000001, 5, 0, 1, 2, 3, 4]);
/// # Ok(())
/// # }
///
/// ```
///
/// A masked Frame
/// ```
/// # use proxmox::tools::websocket::*;
/// # use std::io;
/// # fn main() -> io::Result<()> {
/// let data = vec![0,1,2,3,4];
/// let frame = create_frame(Some([0u8, 1u8, 2u8, 3u8]), &data, OpCode::Text)?;
/// assert_eq!(frame, vec![0b10000001, 0b10000101, 0, 1, 2, 3, 0, 0, 0, 0, 4]);
/// # Ok(())
/// # }
///
/// ```
///
/// A ping Frame
/// ```
/// # use proxmox::tools::websocket::*;
/// # use std::io;
/// # fn main() -> io::Result<()> {
/// let data = vec![0,1,2,3,4];
/// let frame = create_frame(None, &data, OpCode::Ping)?;
/// assert_eq!(frame, vec![0b10001001, 0b00000101, 0, 1, 2, 3, 4]);
/// # Ok(())
/// # }
///
/// ```
pub fn create_frame(
    mask: Option<[u8; 4]>,
    data: &[u8],
    frametype: OpCode,
) -> io::Result<Vec<u8>> {
    let first_byte = 0b10000000 | (frametype as u8);
    let len = data.len();
    if (frametype as u8) & 0b00001000 > 0 && len > 125 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "Control frames cannot have data longer than 125 bytes",
        ));
    }

    let mask_bit = if mask.is_some() { 0b10000000 } else { 0b00000000 };

    let mut buf = Vec::new();
    buf.push(first_byte);

    if len < 126 {
        buf.push(mask_bit | (len as u8));
    } else if len < std::u16::MAX as usize {
        buf.push(mask_bit | 126);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        buf.push(mask_bit | 127);
        buf.extend_from_slice(&(len as u64).to_be_bytes());
    }

    if let Some(mask) = mask {
        buf.extend_from_slice(&mask);
    }
    let mut data = data.to_vec().into_boxed_slice();
    mask_bytes(mask, &mut data);

    buf.append(&mut data.into_vec());
    Ok(buf)
}

/// Wraps a writer that implements AsyncWrite
///
/// Can be used to send websocket frames to any writer that implements
/// AsyncWrite. Every write to it gets encoded as a seperate websocket frame,
/// without fragmentation.
///
/// Example usage:
/// ```
/// # use proxmox::tools::websocket::*;
/// # use std::io;
/// # use tokio::io::{AsyncWrite, AsyncWriteExt};
/// async fn code<I: AsyncWrite + Unpin>(writer: I) -> io::Result<()> {
///     let mut ws = WebSocketWriter::new(None, false, writer);
///     ws.write(&[1u8,2u8,3u8]).await?;
///     Ok(())
/// }
/// ```
pub struct WebSocketWriter<W: AsyncWrite + Unpin> {
    writer: W,
    text: bool,
    mask: Option<[u8; 4]>,
    frame: Option<(Vec<u8>, usize, usize)>,
}

impl<W: AsyncWrite + Unpin> WebSocketWriter<W> {
    /// Creates a new WebSocketWriter which will use the given mask (if any),
    /// and mark the frames as either 'Text' or 'Binary'
    pub fn new(mask: Option<[u8; 4]>, text: bool, writer: W) -> WebSocketWriter<W> {
        WebSocketWriter {
            writer,
            text,
            mask,
            frame: None,
        }
    }

    pub async fn send_control_frame(&mut self, mask: Option<[u8; 4]>, opcode: OpCode, data: &[u8]) -> Result<(), Error> {
        let frame = create_frame(mask, data, opcode)?;
        self.writer.write_all(&frame).await.map_err(Error::from)
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for WebSocketWriter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = Pin::get_mut(self);

        let frametype = match this.text {
            true => OpCode::Text,
            false => OpCode::Binary,
        };

        if this.frame.is_none() {
            // create frame buf
            let frame = match create_frame(this.mask, buf, frametype) {
                Ok(f) => f,
                Err(e) => {
                    return Poll::Ready(Err(e));
                }
            };
            this.frame = Some((frame, 0, buf.len()));
        }

        // we have a frame in any case, so unwrap is ok
        let (buf, pos, origsize) = this.frame.as_mut().unwrap();
        loop {
            match ready!(Pin::new(&mut this.writer).poll_write(cx, &buf[*pos..])) {
                Ok(size) => {
                    *pos += size;
                    if *pos == buf.len() {
                        let size = *origsize;
                        this.frame = None;
                        return Poll::Ready(Ok(size));
                    }
                },
                Err(err) => {
                    eprintln!("error in writer: {}", err);
                    return Poll::Ready(Err(err))
                },
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let this = Pin::get_mut(self);
        Pin::new(&mut this.writer).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let this = Pin::get_mut(self);
        Pin::new(&mut this.writer).poll_shutdown(cx)
    }
}

#[derive(Debug,PartialEq)]
/// Represents the header of a websocket Frame
pub struct FrameHeader {
    /// True if the frame is either non-fragmented, or the last fragment
    pub fin: bool,
    /// The optional mask of the frame
    pub mask: Option<[u8; 4]>,
    /// The frametype
    pub frametype: OpCode,
    /// The length of the header (without payload).
    pub header_len: u8,
    /// The length of the payload.
    pub payload_len: usize,
}

impl FrameHeader {
    /// Returns true if the frame is a control frame.
    pub fn is_control_frame(&self) -> bool {
        self.frametype.is_control()
    }

    /// Tries to parse a FrameHeader from bytes.
    ///
    /// When there are not enough bytes to completely parse the header,
    /// returns Ok(Err(size)) where size determines how many bytes
    /// are missing to parse further (this amount can change when more
    /// information is available)
    ///
    /// Example:
    /// ```
    /// # use proxmox::tools::websocket::*;
    /// # use std::io;
    /// # fn main() -> io::Result<()> {
    /// let frame = create_frame(None, &[0,1,2,3], OpCode::Ping)?;
    /// let header = FrameHeader::try_from_bytes(&frame[..1])?;
    /// match header {
    ///     Ok(_) => unreachable!(),
    ///     Err(x) => assert_eq!(x, 1),
    /// }
    /// let header = FrameHeader::try_from_bytes(&frame[..2])?;
    /// match header {
    ///     Err(x) => unreachable!(),
    ///     Ok(header) => assert_eq!(header, FrameHeader{
    ///         fin: true,
    ///         mask: None,
    ///         frametype: OpCode::Ping,
    ///         header_len: 2,
    ///         payload_len: 4,
    ///     }),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn try_from_bytes(data: &[u8]) -> io::Result<Result<FrameHeader, usize>> {
        let len = data.len();
        if len < 2 {
            return Ok(Err(2 - len));
        }

        let data = data;

        // we do not support extensions
        if data[0] & 0b01110000 > 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "Extensions not supported",
            ));
        }

        let fin = data[0] & 0b10000000 != 0;
        let frametype = match data[0] & 0b1111 {
            0 => OpCode::Continuation,
            1 => OpCode::Text,
            2 => OpCode::Binary,
            8 => OpCode::Close,
            9 => OpCode::Ping,
            10 => OpCode::Pong,
            other => {
                return Err(io::Error::new(ErrorKind::InvalidData, format!("Unknown OpCode {}", other)));
            }
        };

        if !fin && frametype.is_control() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "Control frames cannot be fragmented",
            ));
        }

        let mask_bit = data[1] & 0b10000000 != 0;
        let mut mask_offset = 2;
        let mut payload_offset = 2;
        if mask_bit {
            payload_offset += 4;
        }

        let mut payload_len: usize = (data[1] & 0b01111111).into();
        if payload_len == 126 {
            if len < 4 {
                return Ok(Err(4 - len));
            }
            payload_len = u16::from_be_bytes([data[2], data[3]]) as usize;
            mask_offset += 2;
            payload_offset += 2;
        } else if payload_len == 127 {
            if len < 10 {
                return Ok(Err(10 - len));
            }
            payload_len = u64::from_be_bytes([
                data[2], data[3], data[4], data[5], data[6], data[7], data[8], data[9],
            ]) as usize;
            mask_offset += 8;
            payload_offset += 8;
        }

        if payload_len > 125 && frametype.is_control() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "Control frames cannot carry more than 125 bytes of data",
            ));
        }

        let mask = match mask_bit {
            true => {
                if len < mask_offset + 4 {
                    return Ok(Err(mask_offset + 4 - len));
                }
                let mut mask = [0u8; 4];
                mask.copy_from_slice(&data[mask_offset as usize..payload_offset as usize]);
                Some(mask)
            }
            false => None,
        };

        Ok(Ok(FrameHeader {
            fin,
            mask,
            frametype,
            payload_len,
            header_len: payload_offset,
        }))
    }
}

/// Wraps a reader that implements AsyncRead and implements it itself.
///
/// On read, reads the underlying reader and tries to decode the frames and
/// simply returns the data stream.
/// When it encounters a control frame, calls the given callback.
///
/// Has an internal Buffer for storing incomplete headers.
pub struct WebSocketReader<R: AsyncRead> {
    reader: Option<R>,
    sender: mpsc::UnboundedSender<(OpCode, Box<[u8]>)>,
    read_buffer: Option<ByteBuffer>,
    header: Option<FrameHeader>,
    state: ReaderState<R>,
}

impl<R: AsyncReadExt> WebSocketReader<R> {
    /// Creates a new WebSocketReader with the given CallBack for control frames
    /// and a default buffer size of 4096.
    pub fn new(reader: R, sender: mpsc::UnboundedSender<(OpCode, Box<[u8]>)>) -> WebSocketReader<R> {
        Self::with_capacity(reader, 4096, sender)
    }

    pub fn with_capacity(reader: R, capacity: usize, sender: mpsc::UnboundedSender<(OpCode, Box<[u8]>)>) -> WebSocketReader<R> {
        WebSocketReader {
            reader: Some(reader),
            sender,
            read_buffer: Some(ByteBuffer::with_capacity(capacity)),
            header: None,
            state: ReaderState::NoData,
        }
    }
}

struct ReadResult<R> {
    len: usize,
    reader: R,
    buffer: ByteBuffer,
}

enum ReaderState<R> {
    NoData,
    WaitingForData(Pin<Box<dyn Future<Output = io::Result<ReadResult<R>>> + Send + 'static>>),
    HaveData,
}

unsafe impl<R: Sync> Sync for ReaderState<R> {}

impl<R: AsyncReadExt + Unpin + Send + 'static> AsyncRead for WebSocketReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = Pin::get_mut(self);
        let mut offset = 0;

        loop {
            match &mut this.state {
                ReaderState::NoData => {
                    let mut reader = match this.reader.take() {
                        Some(reader) => reader,
                        None => return Poll::Ready(Err(io_format_err!("no reader"))),
                    };

                    let mut buffer = match this.read_buffer.take() {
                        Some(buffer) => buffer,
                        None => return Poll::Ready(Err(io_format_err!("no buffer"))),
                    };

                    let future = async move {
                        buffer.read_from_async(&mut reader)
                            .await
                            .map(move |len| ReadResult { len, reader, buffer })
                    };

                    this.state = ReaderState::WaitingForData(future.boxed());
                },
                ReaderState::WaitingForData(ref mut future) => {
                    match ready!(future.as_mut().poll(cx)) {
                        Ok(ReadResult { len, reader, buffer }) => {
                            this.reader = Some(reader);
                            this.read_buffer = Some(buffer);
                            this.state = ReaderState::HaveData;
                            if len == 0 {
                                return Poll::Ready(Ok(0));
                            }
                        },
                        Err(err) => return Poll::Ready(Err(err)),
                    }
                },
                ReaderState::HaveData => {
                    let mut read_buffer = match this.read_buffer.take() {
                        Some(read_buffer) => read_buffer,
                        None => return Poll::Ready(Err(io_format_err!("no buffer"))),
                    };

                    let mut header = match this.header.take() {
                        Some(header) => header,
                        None => {
                            let header = match FrameHeader::try_from_bytes(&read_buffer[..])? {
                                Ok(header) => header,
                                Err(_) => {
                                    this.state = ReaderState::NoData;
                                    this.read_buffer = Some(read_buffer);
                                    continue;
                                }
                            };

                            read_buffer.consume(header.header_len as usize);
                            header
                        },
                    };

                    if header.is_control_frame() {
                        if read_buffer.len() >= header.payload_len {

                            let mut data = read_buffer.remove_data(header.payload_len);
                            mask_bytes(header.mask, &mut data);
                            if let Err(err) = this.sender.send((header.frametype, data)) {
                                eprintln!("error sending control frame: {}", err);
                            }

                            this.state =  if read_buffer.is_empty() {
                                ReaderState::NoData
                            } else {
                                ReaderState::HaveData
                            };
                            this.read_buffer = Some(read_buffer);
                            continue;
                        } else {
                            this.header = Some(header);
                            this.read_buffer = Some(read_buffer);
                            this.state = ReaderState::NoData;
                            continue;
                        }
                    }

                    let len = min(buf.len() - offset, min(header.payload_len, read_buffer.len()));

                    let mut data = read_buffer.remove_data(len);
                    mask_bytes(header.mask, &mut data);
                    buf[offset..offset+len].copy_from_slice(&data);
                    offset += len;

                    header.payload_len -= len;

                    if header.payload_len > 0 {
                        this.header = Some(header);
                    }

                    this.state = if read_buffer.is_empty() {
                        ReaderState::NoData
                    } else {
                        ReaderState::HaveData
                    };
                    this.read_buffer = Some(read_buffer);

                    if offset > 0 {
                        return Poll::Ready(Ok(offset));
                    }
                },
            }
        }
    }
}

/// Global Identifier for WebSockets, see RFC6455
pub const MAGIC_WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Provides methods for connecting a WebSocket endpoint with another
pub struct WebSocket {
    text: bool,
}

impl WebSocket {
    /// Returns a new WebSocket instance and the generates the correct
    /// WebSocket response from request headers
    pub fn new(headers: HeaderMap<HeaderValue>) -> Result<(Self, Response<Body>), Error> {
        let protocols = headers
            .get(UPGRADE)
            .ok_or_else(|| format_err!("missing Upgrade header"))?
            .to_str()?;

        let version = headers
            .get(SEC_WEBSOCKET_VERSION)
            .ok_or_else(|| format_err!("missing websocket version"))?
            .to_str()?;

        let key = headers
            .get(SEC_WEBSOCKET_KEY)
            .ok_or_else(|| format_err!("missing websocket key"))?
            .to_str()?;

        let ws_proto = headers
            .get(SEC_WEBSOCKET_PROTOCOL)
            .ok_or_else(|| format_err!("missing websocket key"))?
            .to_str()?;

        let text = ws_proto == "text";

        if protocols != "websocket" {
            bail!("invalid protocol name");
        }

        if version != "13" {
            bail!("invalid websocket version");
        }

        // we ignore extensions

        let mut sha1 = openssl::sha::Sha1::new();
        let data = format!("{}{}", key, MAGIC_WEBSOCKET_GUID);
        sha1.update(data.as_bytes());
        let response_key = base64::encode(sha1.finish());

        let response = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(UPGRADE, HeaderValue::from_static("websocket"))
            .header(CONNECTION, HeaderValue::from_static("Upgrade"))
            .header(SEC_WEBSOCKET_ACCEPT, response_key)
            .header(SEC_WEBSOCKET_PROTOCOL, ws_proto)
            .body(Body::empty())?;

        Ok((Self { text }, response))
    }

    async fn copy_to_websocket<R, W>(
        mut reader: &mut R,
        writer: &mut WebSocketWriter<W>,
        receiver: &mut mpsc::UnboundedReceiver<(OpCode, Box<[u8]>)>) -> Result<bool, Error>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        let mut buf = ByteBuffer::new();
        let mut eof = false;
        loop {
            if !buf.is_full() {
                let bytes = select!{
                    res = buf.read_from_async(&mut reader).fuse() => res?,
                    res = receiver.recv().fuse() => {
                        let (opcode, msg) = res.ok_or(format_err!("control channel closed"))?;
                        match opcode {
                            OpCode::Ping => {
                                writer.send_control_frame(None, OpCode::Pong, &msg).await?;
                                continue;
                            }
                            OpCode::Close => {
                                writer.send_control_frame(None, OpCode::Close, &msg).await?;
                                return Ok(true);
                            }
                            _ => {
                                // ignore other frames
                                continue;
                            }
                        }
                    }
                };

                if bytes == 0 {
                    eof = true;
                }
            }
            if buf.len() > 0 {
                let bytes = writer.write(&buf).await?;
                if bytes == 0 {
                    eof = true;
                }
                buf.consume(bytes);
            }

            if eof && buf.is_empty() {
                return Ok(false);
            }
        }
    }

    /// Takes two endpoints and connects them via a websocket, where the
    /// 'upstream' endpoint sends and receives WebSocket frames, while
    /// 'downstream' only expects and sends raw data.
    /// This method takes care of copying the data between endpoints, and
    /// sending correct responses for control frames (e.g. a Pont to a Ping).
    pub async fn serve_connection<S, L>(&self, upstream: S, downstream: L) -> Result<(), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        L: AsyncRead + AsyncWrite + Unpin + Send,
    {

        let (usreader, uswriter) = tokio::io::split(upstream);
        let (mut dsreader, mut dswriter) = tokio::io::split(downstream);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut wsreader = WebSocketReader::new(usreader, tx);
        let mut wswriter = WebSocketWriter::new(None, self.text, uswriter);


        let ws_future = tokio::io::copy(&mut wsreader, &mut dswriter);
        let term_future = Self::copy_to_websocket(&mut dsreader, &mut wswriter, &mut rx);

        let res = select!{
            res = ws_future.fuse() => match res {
                Ok(_) => Ok(()),
                Err(err) => Err(Error::from(err)),
            },
            res = term_future.fuse() => match res {
                Ok(sent_close) if !sent_close => {
                    // status code 1000 => 0x03E8
                    wswriter.send_control_frame(None, OpCode::Close, &[0x03, 0xE8]).await?;
                    Ok(())
                }
                Ok(_) => Ok(()),
                Err(err) => Err(err),
            }
        };

        res
    }
}
