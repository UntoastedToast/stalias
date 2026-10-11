# Admin password and recovery

[Back to the README](../README.md)

Change your admin password under **Settings → Security**, including during setup or when
Stalwart is unreachable. Enter your current password and the new password twice. After
the change, stalias logs out every device and redirects you to the login page. Your
Stalwart settings, aliases and API tokens stay unchanged.

## Local reset

If you lose access, run the reset command in a terminal on the server:

```sh
docker exec -it stalias /stalias reset-admin-password
# Native installation, using the same DATA_DIR as the running server:
DATA_DIR=/path/to/data stalias reset-admin-password
```

The command uses [rpassword](https://docs.rs/rpassword/7.5.4/rpassword/) to read the new
password and confirmation without displaying either. You must use an existing database
with an admin account; the server can keep running. Password arguments and password
environment variables are not accepted.

The reset uses the same password policy and HIBP check as the web UI and logs out every
device. If another password change finishes first, the reset stops without overwriting it.
`HIBP_CHECK=false` disables the online check for that process.

## Login and resource limits

Login and current-password checks share a limit of ten failed or running attempts per IP
and twenty per admin account in 60 seconds. The setup code has separate counters with the
same limits. Once a limit is reached, stalias returns HTTP 429 with a calculated
`Retry-After`. Rejected attempts don't extend the window, and successful authentication
clears the corresponding failures. Accounts are never permanently locked.

Counters stay in server memory and assume one server instance per database. A local reset
doesn't clear an active throttle; it expires within its existing window.

At most two expensive password jobs run at once. If both slots are busy, stalias returns
HTTP 503 and `Retry-After: 1` without counting a failed attempt. Cancelling a request
doesn't free its slot until the worker finishes.

Setup, login and password-change forms accept at most 64 KiB. Each password field accepts
at most 4096 raw UTF-8 bytes before NFC normalization and 128 Unicode codepoints after it.
Other forms and the alias API keep their existing limits.

## Password policy and upgrades

New passwords must have 12 to 128 characters after NFC normalization, contain no
whitespace or control characters, and score at least three in
[zxcvbn](https://crates.io/crates/zxcvbn). This is the project's password policy; it does
not claim full NIST compliance. Login and current-password checks don't repeat these
creation checks.

stalias has no MFA. There is also no compatibility fallback for password hashes created
before NFC normalization was introduced. Decomposed Unicode passwords from those versions may need
a local reset after upgrading. Stored passwords that exceed the input limits also need
a local reset. Password and session updates use a transaction and require no database
schema migration.

## Breach-check failures

When enabled, the HIBP check uses the padded SHA-1 range protocol over HTTPS, with a
three-second connection timeout and a five-second total timeout. It refuses redirects,
streams at most 1 MiB and validates every response record before accepting a result.
Network errors, timeouts, HTTP errors, oversized responses and invalid formats return
HTTP 503, leaving credentials and sessions unchanged. Logs contain error categories only.
