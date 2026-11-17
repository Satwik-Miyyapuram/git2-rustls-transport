# git2-rustls-transport

A libgit2 transport for `https://` remotes, written in Rust. It plugs into
[`git2`](https://crates.io/crates/git2) and performs the smart-HTTP requests
with blocking [`reqwest`](https://crates.io/crates/reqwest) and rustls, so
libgit2 can clone, fetch and push over https without OpenSSL. This is mainly
useful on Android, where OpenSSL is awkward to build.

## Usage

```rust,no_run
use std::time::Duration;

use git2_rustls_transport::{register, Credentials, Limits};

// Configure TLS roots, proxy and timeouts on the builder. `register` finishes
// it and sets the redirect policy itself (see Notes).
let builder = reqwest::blocking::Client::builder()
    .connect_timeout(Duration::from_secs(30))
    // See "Timeouts" below. Never `None`: a stalled server would hang forever.
    .timeout(Duration::from_secs(300));

// Called on 401 at the remote's own origin. Return None to fail the request.
// SAFETY: called once at start-up, before any other git2 use.
unsafe {
    register(builder, Limits::default(), |_url| {
        Some(Credentials {
            username: "user".into(),
            password: "token".into(),
        })
    })
}
.expect("register transport");

let _repo = git2::build::RepoBuilder::new()
    .clone("https://example.com/org/repo.git", std::path::Path::new("repo"))
    .expect("clone");
```

## Notes

- `register` installs the transport process-wide and is `unsafe`: it must not
  run while another thread creates a git2 transport. Call it once per process,
  early, before any https clone or fetch.
- The scheme `"https"` is matched as a prefix, so every https URL goes through
  this transport once it is registered.
- Authentication is HTTP basic auth only. Credentials in the URL
  (`https://user:token@host/repo`) are sent first. A 401 is retried once with
  the `register` callback's credentials. A 401 on a POST is not retried.
- libgit2's `RemoteCallbacks::credentials` is not consulted; only the callback
  passed to `register` is.
- Origins (scheme, host and port): requests never leave the remote URL's
  origin. An info/refs redirect to another origin is refused before that
  origin is contacted, even if a later hop would come back, so no request,
  credential (from the URL or the callback), pack or fetch negotiation is
  ever sent anywhere else. Userinfo in a redirect's `Location` is dropped.
- Redirects: `register` builds the client with `reqwest`'s redirects off
  (`Policy::none()`) and `https_only(true)`, replacing any redirect policy on
  the builder. The transport follows up to 5 redirects of the info/refs GET
  itself and refuses a redirect to a non-https URL. A redirect of a POST is
  refused, so a request body is never sent anywhere but its own URL.
- Memory: the request body of each POST is buffered in memory and sent with a
  `Content-Length`. For a push this is the whole pack, and while the buffer
  grows a push can use up to about twice the pack's size in RAM.
  `Limits::max_request_body` caps the body (`DEFAULT_MAX_REQUEST_BODY` is
  256 MiB); a larger push fails with an error naming the limit. Fetch and
  clone responses are streamed, not buffered. `Limits::max_advertisement`
  caps the info/refs response (`DEFAULT_MAX_ADVERTISEMENT` is 64 MiB), so a
  server cannot stream an endless ref list.
- Timeouts: `reqwest`'s blocking client has no separate read timeout. Its
  `timeout` covers sending a request plus waiting for the response headers as
  one budget (for a push, that includes uploading the pack and the server
  processing it), and then each read of the response body separately. Set it
  above your largest push's upload time. The builder default is 30 s.
  `timeout(None)` disables it, and then a stalled server can hang a clone,
  fetch or push forever.
- The crate enables no TLS feature itself. TLS settings (trust roots, client
  certificates, proxy) come from the `reqwest` builder you pass in.
- On Android, `reqwest`'s platform verifier needs a JVM and JNI setup. Use
  explicit root certificates if you cannot provide one.

## Platform status

- Windows (desktop, verified locally): clone, fetch, push and the 401 basic-auth
  retry pass against a local `git http-backend` behind a self-signed TLS server.
- Linux: not run locally. The CI workflow runs the tests on Linux.
- Android (`aarch64-linux-android`, `x86_64-linux-android`): not built locally,
  and no emulator run has been done. The CI workflow builds the library for
  both targets with the NDK.

## Building and testing

```sh
cargo test
```

The tests need `git` on `PATH`. The minimum supported Rust version is 1.88
(set by the ICU crates that `reqwest` pulls in through `url`; the test-only
dependencies `rcgen` and `time` need it too). CI tests on 1.88.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT licence ([LICENSE-MIT](LICENSE-MIT))

at your option.
