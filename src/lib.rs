use std::fmt::{self, Write};

use async_trait::async_trait;
use bytes::{Buf, BytesMut};
use futures::SinkExt;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Decoder, Encoder, Framed};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct RequestId(IdRepr);

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(untagged)]
enum IdRepr {
    I32(i32),
    String(String),
}

impl From<i32> for RequestId {
    fn from(id: i32) -> RequestId {
        RequestId(IdRepr::I32(id))
    }
}

impl From<String> for RequestId {
    fn from(id: String) -> RequestId {
        RequestId(IdRepr::String(id))
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            IdRepr::I32(it) => fmt::Display::fmt(it, f),
            // Use debug here, to make it clear that `92` and `"92"` are
            // different, and to reduce WTF factor if the sever uses `" "` as an
            // ID.
            IdRepr::String(it) => fmt::Debug::fmt(it, f),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Request {
    pub id: RequestId,
    pub method: String,
    #[serde(default = "serde_json::Value::default")]
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Response {
    // JSON RPC allows this to be null if it was impossible
    // to decode the request's id. Ignore this special case
    // and just die horribly.
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResponseError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug)]
#[allow(unused)]
pub enum ErrorCode {
    // Defined by JSON RPC:
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,
    ServerErrorStart = -32099,
    ServerErrorEnd = -32000,

    /// Error code indicating that a server received a notification or
    /// request before the server has received the `initialize` request.
    ServerNotInitialized = -32002,
    UnknownErrorCode = -32001,

    // Defined by the protocol:
    /// The client has canceled a request and a server has detected
    /// the cancel.
    RequestCanceled = -32800,

    /// The server detected that the content of a document got
    /// modified outside normal conditions. A server should
    /// NOT send this error code if it detects a content change
    /// in it unprocessed messages. The result even computed
    /// on an older state might still be useful for the client.
    ///
    /// If a client decides that a result is not of any use anymore
    /// the client should cancel the request.
    ContentModified = -32801,

    /// The server cancelled the request. This error code should
    /// only be used for requests that explicitly support being
    /// server cancellable.
    ///
    /// @since 3.17.0
    ServerCancelled = -32802,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Notification {
    pub method: String,
    #[serde(default = "serde_json::Value::default")]
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
}

impl Response {
    pub fn new_ok<R: Serialize>(id: RequestId, result: R) -> Response {
        Response {
            id,
            result: Some(serde_json::to_value(result).unwrap()),
            error: None,
        }
    }
    pub fn new_err(id: RequestId, code: i32, message: String) -> Response {
        let error = ResponseError {
            code,
            message,
            data: None,
        };
        Response {
            id,
            result: None,
            error: Some(error),
        }
    }
}

impl Request {
    pub fn new<P: Serialize>(id: RequestId, method: String, params: P) -> Request {
        Request {
            id,
            method,
            params: serde_json::to_value(params).unwrap(),
        }
    }
    pub fn extract<P: DeserializeOwned>(self, method: &str) -> Result<(RequestId, P), Request> {
        if self.method == method {
            let params = serde_json::from_value(self.params).unwrap_or_else(|err| {
                panic!("Invalid request\nMethod: {}\n error: {}", method, err)
            });
            Ok((self.id, params))
        } else {
            Err(self)
        }
    }

    pub fn is_shutdown(&self) -> bool {
        self.method == "shutdown"
    }
    pub fn is_initialize(&self) -> bool {
        self.method == "initialize"
    }
}

impl Notification {
    pub fn new(method: String, params: impl Serialize) -> Notification {
        Notification {
            method,
            params: serde_json::to_value(params).unwrap(),
        }
    }
    pub fn extract<P: DeserializeOwned>(self, method: &str) -> Result<P, Notification> {
        if self.method == method {
            let params = serde_json::from_value(self.params).unwrap_or_else(|err| {
                panic!("Invalid notification\nMethod: {}\n error: {}", method, err)
            });
            Ok(params)
        } else {
            Err(self)
        }
    }
    pub fn is_exit(&self) -> bool {
        self.method == "exit"
    }
    pub fn is_initialized(&self) -> bool {
        self.method == "initialized"
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum IncomingMessage {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

/// lsp over tcp stream decoder
///
/// [base protocol](https://microsoft.github.io/language-server-protocol/specifications/specification-current/#headerPart)
#[derive(Debug, Clone)]
pub struct LspTcpDecoder {
    pub content_length: usize,
    pub content_type: String,
    pub header_consumed: bool,
}

impl Default for LspTcpDecoder {
    fn default() -> Self {
        Self {
            content_length: 0,
            content_type: "application/vscode-jsonrpc; charset=utf-8".to_string(),
            header_consumed: Default::default(),
        }
    }
}

impl LspTcpDecoder {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn consume_body(
        &mut self,
        src: &mut BytesMut,
    ) -> Result<Option<IncomingMessage>, std::io::Error> {
        let ret = Ok(Some(
            serde_json::from_slice(&src[..self.content_length])
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        ));
        src.advance(self.content_length);
        self.header_consumed = false;
        self.content_length = 0;
        ret
    }
}

impl Decoder for LspTcpDecoder {
    type Item = IncomingMessage;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if !src.windows(4).any(|s| s == [b'\r', b'\n', b'\r', b'\n']) {
            Ok(None)
        } else if self.header_consumed {
            tracing::trace!("expect {} bytes got {}", self.content_length, src.len());
            if self.content_length >= src.len() {
                Ok(None)
            } else {
                self.consume_body(src)
            }
        } else {
            let stop_at = src
                .windows(4)
                .position(|s| s == [b'\r', b'\n', b'\r', b'\n'])
                .unwrap();
            let headers = String::from_utf8(src[..stop_at].to_vec()).unwrap();
            for header in headers.split("\r\n") {
                let mut segs = header.split(":");
                let key = segs.next().unwrap();
                if key == "Content-Length" {
                    self.content_length = segs.next().unwrap().trim().parse().unwrap();
                } else if key == "Content-Type" {
                    self.content_type = segs.next().unwrap().trim().to_string();
                } else {
                    tracing::error!("unknown header {}", key);
                }
            }
            if self.content_length == 0 {
                let msg = "empty content length or missing Content-Length header";
                tracing::error!(msg);
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, msg));
            }
            self.header_consumed = true;
            src.advance(stop_at + 4);
            if self.content_length <= src.len() {
                self.consume_body(src)
            } else {
                src.reserve(self.content_length);
                Ok(None)
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LspTcpEncoder {
    pub content_type: Option<String>,
}

impl Encoder<Vec<u8>> for LspTcpEncoder {
    type Error = std::io::Error;

    fn encode(&mut self, item: Vec<u8>, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let ret = if let Some(ct) = self.content_type.as_ref() {
            dst.write_str(&format!(
                "Content-Length: {}\r\nContent-Type: {}\r\n\r\n",
                item.len(),
                ct
            ))
        } else {
            dst.write_str(&format!("Content-Length: {}\r\n\r\n", item.len()))
        };
        ret.map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        dst.extend_from_slice(&item);
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct LspTcpCodec {
    pub encoder: LspTcpEncoder,
    pub decoder: LspTcpDecoder,
}
impl Encoder<Vec<u8>> for LspTcpCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: Vec<u8>, dst: &mut BytesMut) -> Result<(), Self::Error> {
        self.encoder.encode(item, dst)
    }
}

impl Decoder for LspTcpCodec {
    type Item = IncomingMessage;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        self.decoder.decode(src)
    }
}

#[async_trait]
pub trait LspServer {
    type I: From<Vec<u8>> + Send;
    type T: AsyncWrite + AsyncRead + Unpin + Send;
    type U: Encoder<Self::I, Error = std::io::Error>
        + Decoder<Item = IncomingMessage, Error = std::io::Error>
        + Send;

    fn get_framed(&mut self) -> &mut Framed<Self::T, Self::U>;

    async fn run(&mut self) -> std::io::Result<()>;

    async fn dispatch_message(&mut self, msg: IncomingMessage) -> std::io::Result<Vec<u8>> {
        match msg {
            IncomingMessage::Request(req) => self.handle_request(req).await,
            IncomingMessage::Notification(notify) => self.handle_notification(notify).await,
            _ => unreachable!(),
        }
    }
    async fn handle_request(&mut self, req: Request) -> std::io::Result<Vec<u8>>;
    async fn handle_notification(&mut self, notify: Notification) -> std::io::Result<Vec<u8>>;

    async fn send_notification(&mut self, notify: Notification) -> std::io::Result<()> {
        let framed = self.get_framed();
        framed
            .send(
                serde_json::to_string(&notify)
                    .unwrap()
                    .as_bytes()
                    .to_vec()
                    .into(),
            )
            .await?;
        Ok(())
    }
}