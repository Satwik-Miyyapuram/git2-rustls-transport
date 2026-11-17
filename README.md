# git2-rustls-transport

A libgit2 transport for `https://` remotes, written in Rust. It plugs into
[`git2`](https://crates.io/crates/git2) and performs the smart-HTTP requests
with blocking [`reqwest`](https://crates.io/crates/reqwest) and rustls, so
libgit2 can clone, fetch and push over https without OpenSSL. This is mainly
useful on Android, where OpenSSL is awkward to build.

## Usage

```rust,no_run
use git2_rustls_transport::{register, Credentials};

// Build the client yourself. With `Policy::none()` the transport follows
// info/refs redirects itself (see Notes).
let client = reqwest::blocking::Client::builder()
    .redirect(reqwest::redirect::Policy::none())
    // The blocking client times out whole requests after 30 s by default, which
    // cuts off large clones. Bound only the connect.
    .timeout(None)
    .connect_timeout(std::time::Duration::from_secs(30))
    .build()
    .expect("client");

// Called on 401. Return None to fail the request.
// SAFETY: called once at start-up, before any other git2 use.
unsafe {
    register(client, |_url| {
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
- Redirects: with `Policy::none()` on the client, the transport follows up to
  5 info/refs redirects itself. With a following policy, `reqwest` follows
  them. Either way a redirect off `https` is refused, credentials are dropped
  when the redirect changes scheme, host or port, and a redirected POST is
  refused.
- The request body of each POST is buffered in memory and sent with a
  `Content-Length`. For a push this is the whole pack.
- The crate enables no TLS feature itself. TLS settings (trust roots, client
  certificates, proxy) come from the `reqwest` client you pass in.
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

The tests need `git` on `PATH`.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT licence ([LICENSE-MIT](LICENSE-MIT))

at your option.
