//! The smart-HTTP subtransport: info/refs GET, stateless-RPC POST, and the
//! 401 credential retry.

use std::io::{self, Read, Write};
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard};

use git2::transport::{Service, SmartSubtransport, SmartSubtransportStream};
use git2::Error;
use percent_encoding::percent_decode_str;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION, USER_AGENT};
use reqwest::{StatusCode, Url};

use crate::{Credentials, Limits};

const USER_AGENT_VALUE: &str = concat!("git2-rustls-transport/", env!("CARGO_PKG_VERSION"));
const MAX_REDIRECTS: usize = 5;

pub(crate) type CredentialCallback = dyn Fn(&str) -> Option<Credentials> + Send + Sync;

#[derive(Default)]
struct AuthState {
    /// The remote's URL as libgit2 gave it, without userinfo. Credentials are
    /// only ever sent to its origin (scheme, host and port).
    start: Option<Url>,
    /// Base URL of the repository, updated when info/refs is redirected.
    base: Option<String>,
    /// Credentials that the server accepted, reused for the POST requests.
    accepted: Option<Credentials>,
}

/// One subtransport per remote, created by the factory in `lib.rs`.
pub(crate) struct Subtransport {
    /// Built by `register` with redirects off, so every redirect reaches
    /// `advertise` or `RpcStream::read`.
    client: Client,
    callback: Arc<CredentialCallback>,
    limits: Limits,
    state: Mutex<AuthState>,
}

impl Subtransport {
    pub(crate) fn new(client: Client, callback: Arc<CredentialCallback>, limits: Limits) -> Self {
        Self {
            client,
            callback,
            limits,
            state: Mutex::new(AuthState::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, AuthState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// The repository base URL, without userinfo. On first use, credentials in
    /// the libgit2 URL (`https://user:token@host/repo`) seed `accepted`.
    fn base_for(&self, url: &str) -> Result<String, Error> {
        let mut state = self.state();
        if let Some(base) = &state.base {
            return Ok(base.clone());
        }
        let mut parsed = parse_url(url)?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            if state.accepted.is_none() {
                state.accepted = Some(Credentials {
                    username: decode(parsed.username())?,
                    password: decode(parsed.password().unwrap_or(""))?,
                });
            }
            parsed = without_userinfo(&parsed);
        }
        state.start = Some(parsed.clone());
        Ok(parsed.as_str().trim_end_matches('/').to_owned())
    }

    /// Whether `url` has the remote's own origin. Only then may credentials
    /// be sent or the callback be asked for them.
    fn is_start_origin(&self, url: &Url) -> bool {
        self.state()
            .start
            .as_ref()
            .is_some_and(|start| start.origin() == url.origin())
    }

    /// Accepted credentials, if `url` is on the remote's own origin.
    fn credentials_for(&self, url: &Url) -> Option<Credentials> {
        if self.is_start_origin(url) {
            self.state().accepted.clone()
        } else {
            None
        }
    }

    fn advertise(&self, url: &str, service: &str) -> Result<Stream, Error> {
        let base = self.base_for(url)?;
        let mut target = parse_url(&format!("{base}/info/refs?service={service}"))?;
        let mut response = self.get_authenticated(&target)?;
        // Redirects are followed only here: the client never follows them.
        for _ in 0..MAX_REDIRECTS {
            if !response.status().is_redirection() {
                break;
            }
            let location = response
                .headers()
                .get(LOCATION)
                .ok_or_else(|| Error::from_str("https transport: redirect without Location"))?
                .to_str()
                .map_err(|_| Error::from_str("https transport: bad Location: not ASCII"))?
                .to_owned();
            let next = target
                .join(&location)
                .map_err(|e| Error::from_str(&format!("https transport: bad redirect: {e}")))?;
            check_redirect(&next)?;
            // Every hop stays on the remote's origin, so no request (and no
            // POST to a base taken from here) ever goes anywhere else.
            if !self.is_start_origin(&next) {
                return Err(Error::from_str(&format!(
                    "https transport: info/refs redirected to another origin ({}); refused, \
                     requests never leave the remote's origin",
                    redact(&next)
                )));
            }
            // Userinfo in a Location would become an Authorization header.
            target = without_userinfo(&next);
            response = self.get_authenticated(&target)?;
        }
        if response.status().is_redirection() {
            return Err(Error::from_str("https transport: too many redirects"));
        }
        if !response.status().is_success() {
            return Err(Error::from_str(&http_error(response.status(), &target)));
        }
        require_content_type(&response, &format!("application/x-{service}-advertisement"))?;
        self.state().base = Some(base_from_info_refs(&target)?);
        Ok(Stream::Advertisement(Advertisement {
            response,
            remaining: self.limits.max_advertisement,
            limit: self.limits.max_advertisement,
        }))
    }

    /// GET with the accepted credentials. On a 401, asks the callback and
    /// retries once. `advertise` only calls it for the remote's own origin;
    /// the check below keeps the callback from ever seeing another one.
    fn get_authenticated(&self, url: &Url) -> Result<Response, Error> {
        let response = self.get(url, self.credentials_for(url).as_ref())?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        if !self.is_start_origin(url) {
            return Err(Error::from_str(&format!(
                "https transport: HTTP 401 from another origin ({}); refused",
                redact(url)
            )));
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
        // Keep them only if the server took them; a 403/404/5xx fails later.
        if retried.status().is_success() || retried.status().is_redirection() {
            self.state().accepted = Some(credentials);
        }
        Ok(retried)
    }

    fn get(&self, url: &Url, credentials: Option<&Credentials>) -> Result<Response, Error> {
        let mut request = self
            .client
            .get(url.clone())
            .header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(c) = credentials {
            request = request.basic_auth(&c.username, Some(&c.password));
        }
        request.send().map_err(network_error)
    }

    fn rpc(&self, url: &str, service: &str) -> Result<Stream, Error> {
        let target = parse_url(&format!("{}/{service}", self.base_for(url)?))?;
        let mut request = self
            .client
            .post(target.clone())
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(CONTENT_TYPE, format!("application/x-{service}-request"))
            .header(ACCEPT, format!("application/x-{service}-result"));
        if let Some(c) = self.credentials_for(&target) {
            request = request.basic_auth(c.username, Some(c.password));
        }

        Ok(Stream::Rpc(Box::new(RpcStream {
            request: Some(request),
            body: Vec::new(),
            max_body: self.limits.max_request_body,
            response: None,
            target,
            result_type: format!("application/x-{service}-result"),
        })))
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
    Advertisement(Advertisement),
    /// POST: request body written by libgit2, reply read back.
    Rpc(Box<RpcStream>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Advertisement(advertisement) => advertisement.read(buf),
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

/// The info/refs body, read through at most `limit` bytes.
pub(crate) struct Advertisement<R = Response> {
    response: R,
    remaining: usize,
    limit: usize,
}

impl<R: Read> Read for Advertisement<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            // At the limit: the body must end here.
            let mut probe = [0u8; 1];
            return match self.response.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::other(format!(
                    "https transport: info/refs advertisement exceeds the {}-byte limit \
                     (Limits::max_advertisement)",
                    self.limit
                ))),
            };
        }
        let len = buf.len().min(self.remaining);
        let n = self.response.read(&mut buf[..len])?;
        self.remaining -= n;
        Ok(n)
    }
}

/// The request body is buffered and sent with a `Content-Length` on the first
/// read. A stateless-RPC body is one negotiation round or one pack, so a push
/// holds its whole pack in memory, up to `max_body` bytes.
pub(crate) struct RpcStream {
    /// The POST, until it is sent.
    request: Option<RequestBuilder>,
    body: Vec<u8>,
    max_body: usize,
    response: Option<Response>,
    target: Url,
    result_type: String,
}

impl Write for RpcStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.request.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "request body already sent",
            ));
        }
        if buf.len() > self.max_body.saturating_sub(self.body.len()) {
            // Drop the request so nothing partial is ever sent.
            self.request = None;
            self.body = Vec::new();
            return Err(io::Error::other(format!(
                "https transport: request body exceeds the {}-byte limit \
                 (Limits::max_request_body)",
                self.max_body
            )));
        }
        self.body.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for RpcStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.response.is_none() {
            // Reading means the request body is complete: send it.
            let request = self
                .request
                .take()
                .ok_or_else(|| io::Error::other("https transport: request already failed"))?;
            let response = request
                .body(mem::take(&mut self.body))
                .send()
                .map_err(|e| io::Error::other(network_error(e).message().to_owned()))?;
            if response.status().is_redirection() {
                return Err(io::Error::other(
                    "https transport: redirect on POST refused (only info/refs is followed)",
                ));
            }
            if !response.status().is_success() {
                return Err(io::Error::other(http_error(
                    response.status(),
                    &self.target,
                )));
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

fn parse_url(raw: &str) -> Result<Url, Error> {
    Url::parse(raw).map_err(|e| {
        Error::from_str(&format!(
            "https transport: bad URL {}: {e}",
            redact_raw(raw)
        ))
    })
}

fn without_userinfo(url: &Url) -> Url {
    let mut clean = url.clone();
    // These fail only for URLs that cannot carry userinfo, so have none.
    let _ = clean.set_username("");
    let _ = clean.set_password(None);
    clean
}

/// The URL for error messages, never with credentials in it.
fn redact(url: &Url) -> String {
    without_userinfo(url).to_string()
}

/// `redact` for text that did not parse as a URL: drops everything after
/// `://` up to the last `@`. A password may hold an unencoded `/`, so the
/// authority's end cannot be trusted; this may over-redact a path with `@`.
fn redact_raw(raw: &str) -> String {
    let Some((scheme, rest)) = raw.split_once("://") else {
        return raw.to_owned();
    };
    match rest.rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => raw.to_owned(),
    }
}

/// Percent-decodes a userinfo part of the remote URL. Non-UTF-8 is an error:
/// a lossy decode would silently send different credentials.
fn decode(component: &str) -> Result<String, Error> {
    percent_decode_str(component)
        .decode_utf8()
        .map(|text| text.into_owned())
        .map_err(|_| {
            Error::from_str("https transport: credentials in the remote URL are not valid UTF-8")
        })
}

/// Strips `/info/refs` (and its query) to get the repository base URL.
fn base_from_info_refs(url: &Url) -> Result<String, Error> {
    let path = url.path();
    let stripped = path
        .strip_suffix("/info/refs")
        .ok_or_else(|| Error::from_str("https transport: redirect did not end at /info/refs"))?;
    let mut base = without_userinfo(url);
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
    format!("https transport: HTTP {status} from {}", redact(url))
}

/// Refuses a redirect off https. (The client is also `https_only`.)
fn check_redirect(to: &Url) -> Result<(), Error> {
    if to.scheme() == "https" {
        Ok(())
    } else {
        Err(Error::from_str(&format!(
            "https transport: redirect to non-https URL {} refused",
            redact(to)
        )))
    }
}

fn network_error(mut error: reqwest::Error) -> Error {
    // reqwest drops userinfo from request URLs today; do not rely on it.
    if let Some(url) = error.url_mut() {
        *url = without_userinfo(url);
    }
    Error::from_str(&format!("https transport: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_error_redacts_userinfo() {
        let url = Url::parse("https://alice:hunter2@example.com/repo.git/info/refs").unwrap();
        let text = http_error(StatusCode::NOT_FOUND, &url);
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("alice"), "{text}");
        assert!(text.contains("example.com/repo.git/info/refs"), "{text}");
    }

    #[test]
    fn parse_url_error_redacts_userinfo() {
        let err = parse_url("https://alice:hunter2@exa mple.com/repo.git").unwrap_err();
        assert!(!err.message().contains("hunter2"), "{}", err.message());
        assert!(err.message().contains("bad URL"), "{}", err.message());
    }

    #[test]
    fn parse_url_error_redacts_password_with_slash() {
        let err = parse_url("https://alice:pa/ss@example.com/repo.git").unwrap_err();
        assert!(!err.message().contains("pa/ss"), "{}", err.message());
        assert!(!err.message().contains("alice"), "{}", err.message());
    }

    #[test]
    fn non_utf8_userinfo_is_an_error() {
        let sub = Subtransport::new(Client::new(), Arc::new(|_: &str| None), Limits::default());
        let err = sub
            .base_for("https://alice:%FF@example.com/repo.git")
            .unwrap_err();
        assert!(err.message().contains("UTF-8"), "{}", err.message());
        assert!(sub.state().accepted.is_none());
    }

    #[test]
    fn network_error_redacts_userinfo() {
        let client = Client::builder().https_only(true).build().unwrap();
        let error = client
            .get("http://alice:hunter2@127.0.0.1:1/repo.git")
            .send()
            .unwrap_err();
        assert!(error.url().is_some(), "error carries no URL: {error}");
        let text = network_error(error).message().to_owned();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("alice"), "{text}");
    }

    /// Serves one canned reply per connection, recording whether each request
    /// carried an Authorization header.
    fn serve(replies: Vec<&'static str>) -> (Url, std::thread::JoinHandle<Vec<bool>>) {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!(
            "http://{}/repo.git",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let handle = std::thread::spawn(move || {
            let mut auth = Vec::new();
            for reply in replies {
                let (mut tcp, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(tcp.try_clone().unwrap());
                let mut seen = false;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    seen |= line.to_ascii_lowercase().starts_with("authorization:");
                }
                auth.push(seen);
                let head =
                    format!("HTTP/1.1 {reply}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                tcp.write_all(head.as_bytes()).unwrap();
            }
            auth
        });
        (url, handle)
    }

    #[test]
    fn credentials_refused_with_403_are_not_kept() {
        let (url, server) = serve(vec!["401 Unauthorized", "403 Forbidden", "200 OK"]);
        let callback = |_: &str| {
            Some(Credentials {
                username: "alice".to_owned(),
                password: "hunter2".to_owned(),
            })
        };
        let client = Client::builder().no_proxy().build().unwrap();
        let sub = Subtransport::new(client, Arc::new(callback), Limits::default());
        sub.base_for(url.as_str()).unwrap();
        let response = sub.get_authenticated(&url).unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(sub.state().accepted.is_none());
        // The next request goes out without them.
        let next = sub.get(&url, sub.credentials_for(&url).as_ref()).unwrap();
        assert_eq!(next.status(), StatusCode::OK);
        assert_eq!(server.join().unwrap(), vec![false, true, false]);
    }

    fn advertisement(body: &[u8], limit: usize) -> io::Result<Vec<u8>> {
        let mut reader = Advertisement {
            response: io::Cursor::new(body.to_vec()),
            remaining: limit,
            limit,
        };
        let mut out = Vec::new();
        reader.read_to_end(&mut out).map(|_| out)
    }

    #[test]
    fn advertisement_exactly_at_the_limit_is_accepted() {
        assert_eq!(advertisement(b"0123456789", 10).unwrap(), b"0123456789");
    }

    #[test]
    fn advertisement_one_byte_over_the_limit_is_refused() {
        let err = advertisement(b"0123456789a", 10).unwrap_err();
        assert!(err.to_string().contains("10-byte limit"), "{err}");
    }
}
