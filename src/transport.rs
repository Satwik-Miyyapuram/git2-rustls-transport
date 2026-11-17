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

use crate::Credentials;

const USER_AGENT_VALUE: &str = concat!("git2-rustls-transport/", env!("CARGO_PKG_VERSION"));
const MAX_REDIRECTS: usize = 5;

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
                    username: decode(parsed.username()),
                    password: decode(parsed.password().unwrap_or("")),
                });
            }
            parsed = without_userinfo(&parsed);
        }
        Ok(parsed.as_str().trim_end_matches('/').to_owned())
    }

    fn accepted_credentials(&self) -> Option<Credentials> {
        self.state().accepted.clone()
    }

    fn advertise(&self, url: &str, service: &str) -> Result<Stream, Error> {
        let base = self.base_for(url)?;
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
            let next = target
                .join(&location)
                .map_err(|e| Error::from_str(&format!("https transport: bad redirect: {e}")))?;
            self.check_redirect(&target, &next)?;
            target = next;
            response = self.get_authenticated(&target)?;
        }
        if response.status().is_redirection() {
            return Err(Error::from_str("https transport: too many redirects"));
        }
        // A client that follows redirects itself may have ended elsewhere.
        let final_url = response.url().clone();
        self.check_redirect(&target, &final_url)?;
        if !response.status().is_success() {
            return Err(Error::from_str(&http_error(response.status(), &target)));
        }
        require_content_type(&response, &format!("application/x-{service}-advertisement"))?;
        self.state().base = Some(base_from_info_refs(response.url())?);
        Ok(Stream::Advertisement(response))
    }

    /// Refuses a redirect off https, and forgets accepted credentials when the
    /// redirect leaves the origin (scheme, host or port), so they are not sent
    /// to another server.
    fn check_redirect(&self, from: &Url, to: &Url) -> Result<(), Error> {
        if to.scheme() != "https" {
            return Err(Error::from_str(&format!(
                "https transport: redirect to non-https URL {} refused",
                redact(to)
            )));
        }
        if to.origin() != from.origin() {
            self.state().accepted = None;
        }
        Ok(())
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
        let target = parse_url(&format!("{}/{service}", self.base_for(url)?))?;
        let mut request = self
            .client
            .post(target.clone())
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(CONTENT_TYPE, format!("application/x-{service}-request"))
            .header(ACCEPT, format!("application/x-{service}-result"));
        if let Some(c) = self.accepted_credentials() {
            request = request.basic_auth(c.username, Some(c.password));
        }

        Ok(Stream::Rpc(Box::new(RpcStream {
            request: Some(request),
            body: Vec::new(),
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
    Advertisement(Response),
    /// POST: request body written by libgit2, reply read back.
    Rpc(Box<RpcStream>),
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

/// The request body is buffered and sent with a `Content-Length` on the first
/// read. A stateless-RPC body is one negotiation round or one pack, so it is
/// bounded by the pack size.
pub(crate) struct RpcStream {
    /// The POST, until it is sent.
    request: Option<RequestBuilder>,
    body: Vec<u8>,
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
            let request = self.request.take().ok_or_else(|| {
                io::Error::other("https transport: request already failed")
            })?;
            let response = request
                .body(mem::take(&mut self.body))
                .send()
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

fn parse_url(raw: &str) -> Result<Url, Error> {
    Url::parse(raw).map_err(|e| {
        Error::from_str(&format!("https transport: bad URL {}: {e}", redact_raw(raw)))
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

/// `redact` for text that did not parse as a URL: drops the authority up to
/// its last `@`.
fn redact_raw(raw: &str) -> String {
    let Some((scheme, rest)) = raw.split_once("://") else {
        return raw.to_owned();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => raw.to_owned(),
    }
}

fn decode(component: &str) -> String {
    percent_decode_str(component).decode_utf8_lossy().into_owned()
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
    format!("https transport: HTTP {status} from {}", redact(url))
}

fn network_error(error: reqwest::Error) -> Error {
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
}
