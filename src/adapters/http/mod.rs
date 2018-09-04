//! Minimalist HTTP client designed for use with yubihsm-connector
//!
//! This is not a full-fledged HTTP client and has been specifically designed
//! to work with yubihsm-connector, which uses HTTP as a wrapper for the
//! underlying YubiHSM encrypted channel protocol.
//!
//! We include this client rather than using a standard crate to minimize
//! dependencies/code surface as well as permit potential usage of this crate
//! in environments (e.g. Intel SGX) where it might be difficult to use a
//! full-fledged HTTP crate (e.g. hyper).

#![allow(unknown_lints, write_with_newline)]

use std::{
    fmt::Write as FmtWrite,
    io::{Read, Write as IoWrite},
    net::{TcpStream, ToSocketAddrs},
    str,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

mod config;
mod status;

pub use self::{config::HttpConfig, status::ConnectorStatus};
use super::{Adapter, AdapterError};

/// User-Agent string to supply
pub const USER_AGENT: &str = concat!("yubihsm.rs ", env!("CARGO_PKG_VERSION"));

/// Maximum size of the HTTP response from `yubihsm-connector`
pub const MAX_RESPONSE_SIZE: usize = 4096;

/// Delimiter string that separates HTTP headers from bodies
const HEADER_DELIMITER: &[u8] = b"\r\n\r\n";

/// HTTP response status indicating success
const HTTP_SUCCESS_STATUS: &str = "HTTP/1.1 200 OK";

/// The Content-Length Header
const CONTENT_LENGTH_HEADER: &str = "Content-Length: ";

/// The Transfer-Encoding Header
const TRANSFER_ENCODING_HEADER: &str = "Transfer-Encoding: ";

/// Write consistent `debug!(...) lines for adapters
macro_rules! http_debug {
    ($adapter:expr, $msg:expr) => {
        debug!("yubihsm-connector: host={} {}", $adapter.host, $msg);
    };
    ($adapter:expr, $fmt:expr, $($arg:tt)+) => {
        debug!(concat!("yubihsm-connector: host={} ", $fmt), $adapter.host, $($arg)+);
    };
}

/// HTTP(-ish) adapter which supports the minimal parts of the protocol
/// required to communicate with the yubihsm-connector service.
pub struct HttpAdapter {
    /// Host we're configured to connect to (i.e. the "Host" HTTP header)
    host: String,

    /// Configured timeout as a rust duration
    timeout: Duration,

    /// Socket to `yubihsm-connector` process
    socket: Arc<Mutex<TcpStream>>,
}

impl Adapter for HttpAdapter {
    type Config = HttpConfig;
    type Status = ConnectorStatus;

    /// Open a connection to a yubihsm-connector
    fn open(config: Self::Config) -> Result<Self, AdapterError> {
        let host = format!("{}:{}", config.addr, config.port);
        let timeout = Duration::from_millis(config.timeout_ms);
        let socket = connect(&host, timeout)?;

        Ok(Self {
            host,
            timeout,
            socket: Arc::new(Mutex::new(socket)),
        })
    }

    /// Reconnect to yubihsm-connector, closing the existing connection
    fn reconnect(&self) -> Result<(), AdapterError> {
        let mut socket = self.socket.lock().unwrap();
        *socket = connect(&self.host, self.timeout)?;
        Ok(())
    }

    /// GET /connector/status returning the result as adapter::Status
    fn status(&self) -> Result<ConnectorStatus, AdapterError> {
        let http_response = self.get("/connector/status")?;
        ConnectorStatus::parse(str::from_utf8(&http_response)?)
    }

    /// POST /connector/api with a given command message
    fn send_command(&self, uuid: Uuid, cmd: Vec<u8>) -> Result<Vec<u8>, AdapterError> {
        self.post("/connector/api", uuid, cmd)
    }
}

/// Open a socket to yubihsm-connector
fn connect(host: &str, timeout: Duration) -> Result<TcpStream, AdapterError> {
    // Resolve DNS, and for now pick the first available address
    // TODO: round robin DNS support?
    let socketaddr = &host.to_socket_addrs()?.next().ok_or_else(|| {
        adapter_err!(
            AddrInvalid,
            "couldn't resolve DNS for {}",
            host.split(':').next().unwrap()
        )
    })?;

    let socket = TcpStream::connect_timeout(socketaddr, timeout)?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;

    Ok(socket)
}

impl HttpAdapter {
    /// Make an HTTP GET request to the yubihsm-connector
    fn get(&self, path: &str) -> Result<Vec<u8>, AdapterError> {
        let mut request = String::new();

        write!(request, "GET {} HTTP/1.1\r\n", path)?;
        write!(request, "Host: {}\r\n", self.host)?;
        write!(request, "User-Agent: {}\r\n", USER_AGENT)?;
        write!(request, "Content-Length: 0\r\n\r\n")?;

        let mut socket = self.socket.lock().unwrap();

        let request_start = Instant::now();
        socket.write_all(request.as_bytes())?;

        let response = ResponseReader::read(&mut socket)?;
        let elapsed_time = Instant::now().duration_since(request_start);

        http_debug!(
            self,
            "method=GET path={} t={}ms)",
            path,
            elapsed_time.as_secs() * 1000 + u64::from(elapsed_time.subsec_millis())
        );

        Ok(response.into())
    }

    /// Make an HTTP POST request to the yubihsm-connector
    fn post(&self, path: &str, uuid: Uuid, mut body: Vec<u8>) -> Result<Vec<u8>, AdapterError> {
        let mut headers = String::new();

        write!(headers, "POST {} HTTP/1.1\r\n", path)?;
        write!(headers, "Host: {}\r\n", self.host)?;
        write!(headers, "User-Agent: {}\r\n", USER_AGENT)?;
        write!(headers, "X-Request-ID: {}\r\n", uuid)?;
        write!(headers, "Content-Length: {}\r\n\r\n", body.len())?;

        // It's friendlier to Nagle's algorithm if we combine the request
        // headers and body, especially if the request fits in a single packet
        let mut request: Vec<u8> = headers.into();
        request.append(&mut body);

        let mut socket = self.socket.lock().unwrap();

        let request_start = Instant::now();
        socket.write_all(&request)?;

        let response = ResponseReader::read(&mut socket)?;
        let elapsed_time = Instant::now().duration_since(request_start);

        http_debug!(
            self,
            "method=POST path={} uuid={} t={}ms",
            path,
            uuid,
            elapsed_time.as_secs() * 1000 + u64::from(elapsed_time.subsec_millis())
        );

        Ok(response.into())
    }
}

/// Buffered reader for short (i.e. 8kB or less) HTTP responses
struct ResponseReader {
    /// Data buffer
    buffer: [u8; MAX_RESPONSE_SIZE],

    /// Position inside of the data buffer
    pos: usize,

    /// Position at which the body begins
    body_offset: Option<usize>,

    /// Length of the body (if we're received it)
    content_length: usize,
}

impl ResponseReader {
    /// Create a new response buffer
    pub fn read(socket: &mut TcpStream) -> Result<Self, AdapterError> {
        let mut buffer = Self {
            buffer: [0u8; MAX_RESPONSE_SIZE],
            pos: 0,
            body_offset: None,
            content_length: 0,
        };

        buffer.read_headers(socket)?;
        buffer.read_body(socket)?;

        Ok(buffer)
    }

    /// Read some data into the internal buffer
    fn fill_buffer(&mut self, socket: &mut TcpStream) -> Result<usize, AdapterError> {
        let nbytes = socket.read(&mut self.buffer[..])?;
        self.pos += nbytes;
        Ok(nbytes)
    }

    /// Read the HTTP response headers
    fn read_headers(&mut self, socket: &mut TcpStream) -> Result<(), AdapterError> {
        assert!(self.body_offset.is_none(), "already read headers!");

        loop {
            self.fill_buffer(socket)?;

            // Scan the buffer for the header delimiter
            // TODO: this is less efficient than it should be
            let mut offset = 0;
            while self.buffer[offset..].len() > HEADER_DELIMITER.len() {
                if self.buffer[offset..].starts_with(HEADER_DELIMITER) {
                    self.body_offset = Some(offset + HEADER_DELIMITER.len());
                    break;
                } else {
                    offset += 1;
                }
            }

            if self.body_offset.is_some() {
                break;
            } else if self.pos + 1 >= MAX_RESPONSE_SIZE {
                adapter_fail!(
                    ResponseError,
                    "exceeded {}-byte response limit reading headers",
                    MAX_RESPONSE_SIZE
                );
            }
        }

        self.parse_headers()
    }

    /// Parse the HTTP headers, extracting the Content-Length
    fn parse_headers(&mut self) -> Result<(), AdapterError> {
        let body_offset = self.body_offset.unwrap();
        let header_str = str::from_utf8(&self.buffer[..body_offset])?;

        let mut header_iter = header_str.split("\r\n");

        // Ensure we got a 200 OK status
        match header_iter.next() {
            Some(HTTP_SUCCESS_STATUS) => (),
            Some(status) => adapter_fail!(
                ResponseError,
                "unexpected HTTP response status: \"{}\"",
                status
            ),
            None => adapter_fail!(ResponseError, "HTTP response status line missing!"),
        }

        for header in header_iter {
            if header.starts_with(CONTENT_LENGTH_HEADER) {
                let content_length: usize = header[CONTENT_LENGTH_HEADER.len()..].parse()?;

                if MAX_RESPONSE_SIZE - body_offset < content_length {
                    adapter_fail!(
                        ResponseError,
                        "response body length too large for buffer ({} bytes)",
                        content_length
                    );
                }

                self.content_length = content_length;
            } else if header.starts_with(TRANSFER_ENCODING_HEADER) {
                let transfer_encoding = &header[TRANSFER_ENCODING_HEADER.len()..];
                adapter_fail!(
                    ResponseError,
                    "adapter sent unsupported transfer encoding: {}",
                    transfer_encoding
                );
            }
        }

        Ok(())
    }

    /// Read the response body into the internal buffer
    fn read_body(&mut self, socket: &mut TcpStream) -> Result<(), AdapterError> {
        let body_end =
            self.content_length + self.body_offset.expect("not ready to read the body yet");

        while self.pos < body_end {
            self.fill_buffer(socket)?;
        }

        Ok(())
    }
}

impl Into<Vec<u8>> for ResponseReader {
    fn into(self) -> Vec<u8> {
        let body_offset = self
            .body_offset
            .expect("we should've already read the body");

        Vec::from(&self.buffer[body_offset..self.pos])
    }
}
