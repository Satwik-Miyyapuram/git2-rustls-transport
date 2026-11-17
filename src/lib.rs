//! A libgit2 smart-HTTP transport written on blocking `reqwest` and rustls.
//!
//! libgit2 can only speak `https://` if it was built with OpenSSL, Secure
//! Transport or WinHTTP. Vendoring OpenSSL is unwanted on Android, so this
//! crate registers an `https` subtransport with libgit2 that performs the HTTP
//! exchanges itself. The caller supplies the [`reqwest::blocking::Client`], so
//! the trust store (for example `rustls-platform-verifier`) is the caller's
//! choice.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let client = reqwest::blocking::Client::builder()
//!     // Redirects are handled by this crate for info/refs only.
//!     .redirect(reqwest::redirect::Policy::none())
//!     .build()?;
//! git2_rustls_transport::register(client, |_url| None)?;
//! git2::Repository::clone("https://github.com/octocat/Hello-World.git", "hello")?;
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::sync::Arc;

use git2::transport::Transport;

mod transport;

use transport::{CredentialCallback, Subtransport};

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
/// `credentials` is called with the URL of a request that returned `401`. It
/// returns `None` to give up. Credentials that succeed are cached for the
/// remote's later requests (the POST that follows info/refs).
///
/// libgit2's transport registry is process-global, so call this once at
/// start-up. A second call returns an error from libgit2.
pub fn register<F>(client: reqwest::blocking::Client, credentials: F) -> Result<(), git2::Error>
where
    F: Fn(&str) -> Option<Credentials> + Send + Sync + 'static,
{
    let callback: Arc<CredentialCallback> = Arc::new(credentials);
    // SAFETY: git2 documents this function as requiring external
    // synchronisation with other transport creation. The caller contract above
    // (register once, at start-up) provides that.
    unsafe {
        git2::transport::register("https", move |remote| {
            let subtransport = Subtransport::new(client.clone(), Arc::clone(&callback));
            Transport::smart(remote, true, subtransport)
        })
    }
}
