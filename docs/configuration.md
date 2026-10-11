# Configuration and deployment

[Back to the README](../README.md)

Configure the app through the web UI.

If you change the Stalwart server URL, stalias revokes its API tokens and restarts domain
and token setup. Mailbox IDs belong to the original server. Updating only the API key
keeps your tokens and selected domains.

Environment variables:

| Variable | Default | |
| --- | --- | --- |
| `PORT` | `8080` | port inside the container |
| `DATA_DIR` | `/data` | where `stalias.db` lives; must be writable for UID 65532 |
| `TRUST_PROXY` | `false` | use `X-Forwarded-For` for client IPs; enable only behind a reverse proxy you control |
| `HIBP_CHECK` | `true` | check new admin passwords with [Have I Been Pwned](https://haveibeenpwned.com/API/v3#PwnedPasswords). Only the first 5 characters of the SHA-1 hash are sent. Set `false` to make no outgoing request. If an enabled check fails or returns an invalid response, stalias refuses setup, password changes and local resets. Existing logins work without HIBP |

## Reverse proxy

For production, put stalias behind a reverse proxy with HTTPS.

Forward `Host` and `X-Forwarded-Proto` so stalias displays the correct server URL. Preserve
the original `Host`, or pass it in `X-Forwarded-Host`. Over plain HTTP, browsers send no
`Sec-Fetch-Site` header, so stalias compares `Origin` with `Host`. If the proxy rewrites
`Host`, form submissions fail with "Cross-site request refused". In nginx, use
`proxy_set_header Host $http_host;`. Using `$host` drops the port and breaks setups
that use a port other than 80 or 443.

Set `TRUST_PROXY=true` and configure the proxy to set `X-Forwarded-For`. This lets the
login rate limit count each client separately. Otherwise, all requests share the proxy's
IP budget: ten failed or running attempts block further attempts for up to a minute.

## Stalwart API key

Create an API key in Stalwart's admin UI and replace its permissions with exactly this list:

`authenticate`, `sysAccountGet`, `sysAccountQuery`, `sysAccountUpdate`, `sysDomainGet`,
`sysDomainQuery`

The `sysAccountUpdate` permission also allows password resets for any mailbox. The key
therefore has write access to every mailbox, but cannot grant roles or delete accounts.
stalias stores it in plain text in the database on the data volume. Only the file's owner
has read access when stalias creates it. Protect the volume and the web UI.

## Requirements and limits

In the web UI, you can search and sort aliases, and filter by status, domain or creation
date. Search matches the address or description. Bitwarden puts the website into the
alias description.

- Do not configure a catch-all on the alias domains: it would accept mail for disabled aliases.
- Stalwart creates a JMAP identity for each alias, so mail clients can send from it.
- Aliases are unique across all mailboxes on a Stalwart server. If a randomly generated
  name is taken, stalias generates another.
- Generated names use three words by default: an adverb, an adjective and a noun, as in
  `possibly-cosmic-cougar`. There are about 52.6 million combinations. Under Settings →
  Alias configuration, you can switch to ten random characters, as in `4gh9nzibm3`, with
  about 3.7 quadrillion combinations. This affects new aliases only. Updating from 0.1.2
  or earlier changes the default from characters to words.
- stalias reads at most 500 mailboxes (Stalwart's per-request limit).
- Settings, tokens and sessions are stored in `/data/stalias.db` (SQLite). Tokens are
  stored as hashes and shown only once.
- Stalwart records no creation date for aliases. stalias saves the date in its own database
  when you create an alias through the web UI or API. Existing aliases and those created
  directly in Stalwart show no date and don't appear in results filtered by creation date.

## Verify the image

GitHub Actions builds the release images with a signed build provenance attestation.
The multi-arch image also has a keyless [cosign](https://github.com/sigstore/cosign) signature.
Verify either before running the image:

```sh
gh attestation verify oci://ghcr.io/untoastedtoast/stalias:latest --owner UntoastedToast

cosign verify ghcr.io/untoastedtoast/stalias:latest \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp '^https://github.com/UntoastedToast/stalias/\.github/workflows/release\.yml@refs/tags/v'
```
