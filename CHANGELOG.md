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
- An info/refs redirect that ends at another origin (scheme, host, port) is
  refused before any POST. Credentials are only sent to the remote URL's
  origin; a 401 at another origin fails without calling the callback.
  Userinfo in a redirect's `Location` is dropped.
- `register` takes a `reqwest::blocking::ClientBuilder` (TLS roots, proxy and
  timeouts are the caller's choice) and builds it with `reqwest` redirects off
  and `https_only(true)`. The transport follows info/refs redirects itself; a
  redirect on a POST or to a non-https URL is refused.
- `register` takes `Limits`: `max_request_body` caps one buffered POST body
  (a push's whole pack; `DEFAULT_MAX_REQUEST_BODY` is 256 MiB) and
  `max_advertisement` caps the info/refs response
  (`DEFAULT_MAX_ADVERTISEMENT` is 64 MiB).
- Non-UTF-8 percent-encoded credentials in the remote URL are an error.
- Error messages never include the URL's username or password.
- Minimum supported Rust version: 1.88.
