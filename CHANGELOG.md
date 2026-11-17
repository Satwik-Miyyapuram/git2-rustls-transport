# Changelog

## 0.1.0

- First release. Registers a `https` smart transport for libgit2 (via `git2`
  0.21) that talks HTTP with blocking `reqwest` and rustls, so clone, fetch and
  push work without OpenSSL.
- Smart HTTP for `git-upload-pack` and `git-receive-pack` (info/refs GET, POST
  with the request body buffered and sent with `Content-Length`).
- `register` is `unsafe`: it must not race with git2 transport creation.
- Credentials in the remote URL (`https://user:token@host/repo`) are used and
  stripped from the request URL.
- On 401 at the remote's own origin, calls the caller's credential callback
  and retries once with basic auth. Accepted credentials are reused for later
  POSTs on the same transport. libgit2's `RemoteCallbacks::credentials` is not
  consulted.
- Credentials are only sent to the remote URL's origin (scheme, host, port).
  A 401 after a redirect to another origin fails without calling the
  callback.
- `register` takes a `reqwest::blocking::ClientBuilder` (TLS roots, proxy and
  timeouts are the caller's choice) and builds it with `reqwest` redirects off
  and `https_only(true)`. The transport follows info/refs redirects itself; a
  redirect on a POST or to a non-https URL is refused.
- `register` takes `max_request_body`, a cap on one buffered POST body (a
  push's whole pack); `DEFAULT_MAX_REQUEST_BODY` is 256 MiB.
- Error messages never include the URL's username or password.
- Minimum supported Rust version: 1.88.
