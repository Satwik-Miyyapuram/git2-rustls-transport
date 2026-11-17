//! A libgit2 smart-HTTP transport written on blocking `reqwest` and rustls.
//!
//! libgit2 can only speak `https://` if it was built with OpenSSL, Secure
//! Transport or WinHTTP. Vendoring OpenSSL is unwanted on Android, so this
//! crate registers an `https` subtransport with libgit2 that performs the HTTP
//! exchanges itself. The caller supplies a [`reqwest::blocking::ClientBuilder`],
//! so the trust store (for example `rustls-platform-verifier`), proxy and
//! timeouts are the caller's choice. The crate sets the redirect policy itself.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use std::time::Duration;
//!
//! let builder = reqwest::blocking::Client::builder()
//!     .connect_timeout(Duration::from_secs(30))
//!     // Bounds sending a request (a push uploads its whole pack here) plus
//!     // waiting for the response headers, and then each read of the
//!     // response body separately. `None` would let a stalled server hang
//!     // the operation forever.
//!     .timeout(Duration::from_secs(300));
//! // SAFETY: called once at start-up, before any git2 operation.
//! unsafe {
//!     git2_rustls_transport::register(
//!         builder,
//!         git2_rustls_transport::DEFAULT_MAX_REQUEST_BODY,
//!         |_url| None,
//!     )?
//! };
//! git2::Repository::clone("https://github.com/octocat/Hello-World.git", "hello")?;
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::sync::Arc;

use git2::transport::Transport;

mod transport;

use transport::{CredentialCallback, Subtransport};

/// Default cap on one POST body (256 MiB). A push buffers its whole pack in
/// memory before sending, so this bounds the memory a push can use.
pub const DEFAULT_MAX_REQUEST_BODY: usize = 256 * 1024 * 1024;

/// Basic-auth credentials returned by the credential callback.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Registers the `https` transport with libgit2.
///
/// `builder` is finished here with redirects turned off in `reqwest`
/// (`Policy::none()`) and `https_only(true)`, overriding any redirect policy
/// set on it. The transport follows up to 5 redirects of the info/refs GET
/// itself, refuses redirects to non-https URLs, and refuses any redirect of a
/// POST, so a request body is only ever sent to the URL it was meant for.
///
/// `max_request_body` caps one POST body in bytes. A push buffers its whole
/// pack in memory, so a push whose pack is larger fails with an error naming
/// the limit. [`DEFAULT_MAX_REQUEST_BODY`] is 256 MiB.
///
/// `credentials` is called with the URL of a request that returned `401`. It
/// returns `None` to give up. Credentials that succeed are cached for the
/// remote's later requests (the POST that follows info/refs).
///
/// Credentials in the remote URL (`https://user:token@host/repo`) are used
/// first. libgit2's `RemoteCallbacks::credentials` is never consulted; only
/// `credentials` here is.
///
/// Credentials are only sent to the remote URL's own origin (scheme, host and
/// port). After a redirect to another origin, requests carry no credentials,
/// and a `401` there fails without calling `credentials`.
///
/// libgit2's transport registry is process-global, so call this once at
/// start-up. A second call returns an error from libgit2.
///
/// # Safety
///
/// This wraps [`git2::transport::register`], which is not synchronised with
/// transport creation. The caller must ensure that no other thread creates a
/// git2 transport (clone, fetch, push, `Remote::connect`, ...) or registers a
/// transport while this runs, for example by calling it at start-up before
/// any other git2 use.
pub unsafe fn register<F>(
    builder: reqwest::blocking::ClientBuilder,
    max_request_body: usize,
    credentials: F,
) -> Result<(), git2::Error>
where
    F: Fn(&str) -> Option<Credentials> + Send + Sync + 'static,
{
    let client = builder
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .build()
        .map_err(|e| git2::Error::from_str(&format!("https transport: building client: {e}")))?;
    let callback: Arc<CredentialCallback> = Arc::new(credentials);
    // SAFETY: the caller upholds the synchronisation contract above.
    unsafe {
        git2::transport::register("https", move |remote| {
            let subtransport =
                Subtransport::new(client.clone(), Arc::clone(&callback), max_request_body);
            Transport::smart(remote, true, subtransport)
        })
    }
}
