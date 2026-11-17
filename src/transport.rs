//! The smart-HTTP subtransport: info/refs GET, stateless-RPC POST, and the
//! 401 credential retry.

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use git2::transport::{Service, SmartSubtransport, SmartSubtransportStream};
use git2::Error;
use reqwest::blocking::{Body, Client, Response};
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION, USER_AGENT};
use reqwest::{StatusCode, Url};

use crate::Credentials;

const USER_AGENT_VALUE: &str = concat!("git2-rustls-transport/", env!("CARGO_PKG_VERSION"));
const MAX_REDIRECTS: usize = 5;
/// Request-body chunks buffered between libgit2 and the HTTP thread. This bounds
/// memory while a pack is streamed.
const BODY_QUEUE_DEPTH: usize = 4;

pub(crate) type CredentialCallback = dyn Fn(&str) -> Option<Credentials> + Send + Sync;

#[derive(Default)]
struct AuthState {
    /// Base URL of the repository, updated when info/refs is redirected.
    base: Option<String>,
    /// Credentials that the server accepted, reused for the POST requests.
    accepted: Option<Credentials>,
}

/// One subtransport per remote, created by the factory in `lib.rs`.
pub(crate) struct Subtransport {
    client: Client,
    callback: Arc<CredentialCallback>,
    state: Mutex<AuthState>,
}

impl Subtransport {
    pub(crate) fn new(client: Client, callback: Arc<CredentialCallback>) -> Self {
        Self {
            client,
            callback,
            state: Mutex::new(AuthState::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, AuthState> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn base_for(&self, url: &str) -> String {
        self.state()
            .base
            .clone()
            .unwrap_or_else(|| url.trim_end_matches('/').to_owned())
    }

    fn accepted_credentials(&self) -> Option<Credentials> {
        self.state().accepted.clone()
    }

    fn advertise(&self, url: &str, service: &str) -> Result<Stream, Error> {
        let base = self.base_for(url);
        let mut target = parse_url(&format!("{base}/info/refs?service={service}"))?;
        let mut response = self.get_authenticated(&target)?;
        // Redirects are followed only here. A client built with
        // `Policy::none()` shows them to us; otherwise reqwest already followed
        // them and `response.url()` carries the final location.
        for _ in 0..MAX_REDIRECTS {
            if !response.status().is_redirection() {
                break;
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| Error::from_str("https transport: redirect without Location"))?
                .to_owned();
            target = target
                .join(&location)
                .map_err(|e| Error::from_str(&format!("https transport: bad redirect: {e}")))?;
            response = self.get_authenticated(&target)?;
        }
        if response.status().is_redirection() {
            return Err(Error::from_str("https transport: too many redirects"));
        }
        if !response.status().is_success() {
            return Err(Error::from_str(&http_error(response.status(), &target)));
        }
        require_content_type(&response, &format!("application/x-{service}-advertisement"))?;
        self.state().base = Some(base_from_info_refs(response.url())?);
        Ok(Stream::Advertisement(response))
    }

    fn get_authenticated(&self, url: &Url) -> Result<Response, Error> {
        let response = self.get(url, self.accepted_credentials().as_ref())?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let credentials = (self.callback)(url.as_str()).ok_or_else(|| {
            Error::from_str("https transport: 401 and the credential callback gave no credentials")
        })?;
        let retried = self.get(url, Some(&credentials))?;
        if retried.status() == StatusCode::UNAUTHORIZED {
            return Err(Error::from_str(
                "https transport: 401 after retry with the supplied credentials",
            ));
        }
        self.state().accepted = Some(credentials);
        Ok(retried)
    }

    fn get(&self, url: &Url, credentials: Option<&Credentials>) -> Result<Response, Error> {
        let mut request = self.client.get(url.clone()).header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(c) = credentials {
            request = request.basic_auth(&c.username, Some(&c.password));
        }
        request.send().map_err(network_error)
    }

    fn rpc(&self, url: &str, service: &str) -> Result<Stream, Error> {
        let target = parse_url(&format!("{}/{service}", self.base_for(url)))?;
        let (body_tx, body_rx) = mpsc::sync_channel::<Vec<u8>>(BODY_QUEUE_DEPTH);
        let (reply_tx, reply_rx) = mpsc::channel();

        // The request runs on its own thread so that libgit2's writes stream
        // into the body while the POST is in flight.
        let client = self.client.clone();
        let request_url = target.clone();
        let credentials = self.accepted_credentials();
        let request_type = format!("application/x-{service}-request");
        let accept_type = format!("application/x-{service}-result");
        thread::Builder::new()
            .name(format!("{service}-post"))
            .spawn(move || {
                let mut request = client
                    .post(request_url)
                    .header(USER_AGENT, USER_AGENT_VALUE)
                    .header(CONTENT_TYPE, request_type)
                    .header(ACCEPT, accept_type)
                    .body(Body::new(QueueReader::new(body_rx)));
                if let Some(c) = credentials {
                    request = request.basic_auth(c.username, Some(c.password));
                }
                // The receiver is gone only when the transfer was abandoned.
                let _ = reply_tx.send(request.send());
            })
            .map_err(|e| Error::from_str(&format!("https transport: thread spawn: {e}")))?;

        Ok(Stream::Rpc(RpcStream {
            body: Some(body_tx),
            reply: reply_rx,
            response: None,
            target,
            result_type: format!("application/x-{service}-result"),
        }))
    }
}

impl SmartSubtransport for Subtransport {
    fn action(
        &self,
        url: &str,
        action: Service,
    ) -> Result<Box<dyn SmartSubtransportStream>, Error> {
        let stream = match action {
            Service::UploadPackLs => self.advertise(url, "git-upload-pack")?,
            Service::ReceivePackLs => self.advertise(url, "git-receive-pack")?,
            Service::UploadPack => self.rpc(url, "git-upload-pack")?,
            Service::ReceivePack => self.rpc(url, "git-receive-pack")?,
        };
        Ok(Box::new(stream))
    }

    fn close(&self) -> Result<(), Error> {
        Ok(())
    }
}

/// What libgit2 reads and writes for one action.
pub(crate) enum Stream {
    /// info/refs body, read-only.
    Advertisement(Response),
    /// POST: request body written by libgit2, reply read back.
    Rpc(RpcStream),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Advertisement(response) => response.read(buf),
            Stream::Rpc(rpc) => rpc.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Advertisement(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "info/refs stream is read-only",
            )),
            Stream::Rpc(rpc) => rpc.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) struct RpcStream {
    body: Option<SyncSender<Vec<u8>>>,
    reply: Receiver<reqwest::Result<Response>>,
    response: Option<Response>,
    target: Url,
    result_type: String,
}

impl Write for RpcStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let body = self.body.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "request body already sent")
        })?;
        body.send(buf.to_vec()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "server closed the connection while the request was being sent",
            )
        })?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for RpcStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.response.is_none() {
            // Reading means the request is complete: closing the sender ends the body.
            self.body = None;
            let response = self
                .reply
                .recv()
                .map_err(|_| io::Error::other("https transport: request thread ended early"))?
                .map_err(io::Error::other)?;
            if response.url() != &self.target || response.status().is_redirection() {
                return Err(io::Error::other(
                    "https transport: redirect on POST refused (only info/refs is followed)",
                ));
            }
            if !response.status().is_success() {
                return Err(io::Error::other(http_error(response.status(), &self.target)));
            }
            check_content_type(&response, &self.result_type).map_err(io::Error::other)?;
            self.response = Some(response);
        }
        match self.response.as_mut() {
            Some(response) => response.read(buf),
            None => Ok(0),
        }
    }
}

/// Adapts the bounded channel to `Read` so reqwest can stream the body.
struct QueueReader {
    rx: Receiver<Vec<u8>>,
    current: Vec<u8>,
    pos: usize,
}

impl QueueReader {
    fn new(rx: Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            current: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for QueueReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.current.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.current = chunk;
                    self.pos = 0;
                }
                // Sender dropped: end of request body.
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.current.len() - self.pos);
        out[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn parse_url(raw: &str) -> Result<Url, Error> {
    Url::parse(raw).map_err(|e| Error::from_str(&format!("https transport: bad URL {raw}: {e}")))
}

/// Strips `/info/refs` (and its query) to get the repository base URL.
fn base_from_info_refs(url: &Url) -> Result<String, Error> {
    let path = url.path();
    let stripped = path.strip_suffix("/info/refs").ok_or_else(|| {
        Error::from_str("https transport: redirect did not end at /info/refs")
    })?;
    let mut base = url.clone();
    base.set_query(None);
    base.set_path(stripped);
    Ok(base.as_str().trim_end_matches('/').to_owned())
}

fn require_content_type(response: &Response, expected: &str) -> Result<(), Error> {
    check_content_type(response, expected).map_err(|m| Error::from_str(&m))
}

fn check_content_type(response: &Response, expected: &str) -> Result<(), String> {
    let actual = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let media_type = actual.split(';').next().unwrap_or("").trim();
    if media_type == expected {
        Ok(())
    } else {
        Err(format!(
            "https transport: expected {expected}, got {actual:?} (not a smart HTTP server?)"
        ))
    }
}

fn http_error(status: StatusCode, url: &Url) -> String {
    format!("https transport: HTTP {status} from {url}")
}

fn network_error(error: reqwest::Error) -> Error {
    Error::from_str(&format!("https transport: {error}"))
}
