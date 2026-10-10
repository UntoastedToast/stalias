<!-- markdownlint-disable MD033 -->
<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/logo-dark.svg">
    <img src="docs/images/logo-light.svg" alt="stalias" height="64">
  </picture>
</h1>

<p align="center">
  <a href="https://github.com/UntoastedToast/stalias/releases"><img src="https://img.shields.io/github/v/release/UntoastedToast/stalias?include_prereleases&label=version" alt="Latest version"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/built%20with-Rust-dea584?logo=rust&logoColor=white" alt="Built with Rust"></a>
</p>

Email aliases for [Stalwart Mail Server](https://stalw.art), created from your password manager.

stalias provides an [Addy.io](https://addy.io)-compatible API for creating and managing
Stalwart aliases. It was originally built for Bitwarden's "Forwarded email alias"
generator. Other clients can use the supported API endpoints too, provided they let you
set a custom Addy.io server URL.

Create a separate address for each new login and receive its mail in your existing
mailbox. You can disable or delete individual aliases through the web UI or API, and search
them by address or description (Bitwarden puts the website into the description), filter by
status, domain and creation date, and sort them.
Disabling an alias stops incoming mail immediately: Stalwart rejects it with
`550 Mailbox does not exist`.

A built-in setup wizard guides you through configuration in the browser, with detailed
instructions at each step. It explains how to connect Stalwart, select alias domains and
create a mailbox token, then shows the settings to enter in Bitwarden.

<p>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/dashboard-dark.png">
    <img src="docs/images/dashboard-light.png" alt="The stalias dashboard with a mailbox's aliases" width="100%">
  </picture>
</p>

- Works with the Community edition of Stalwart 0.16 or newer, using your existing mail
  server and mail ports without a catch-all.
- Stalwart stores the aliases alongside your existing users, domains and mailboxes.
- Runs in a container of about 20 MB with one volume.

## Quick start

stalias runs as its own container next to Stalwart.

```sh
docker run -d --name stalias \
  --network <network-of-your-stalwart-container> \
  -p 127.0.0.1:8080:8080 \
  -v stalias-data:/data \
  ghcr.io/untoastedtoast/stalias:latest
```

You can also run it as a Compose service ([example](docs/examples/compose.yml)), a Coolify
application using the image, or a Portainer stack. For production, put it behind a reverse
proxy with HTTPS.

Forward `Host` and `X-Forwarded-Proto` so stalias displays the correct server URL. Preserve
the original `Host`, or pass it in `X-Forwarded-Host`. Over plain HTTP, browsers send no
`Sec-Fetch-Site` header, so stalias compares `Origin` with `Host`. If the proxy rewrites
`Host`, every form submission fails with "Cross-site request refused". In nginx, use
`proxy_set_header Host $http_host;`. (`$host` drops the port, which breaks every setup that
uses a port other than 80 or 443.)

Set `TRUST_PROXY=true` and have the proxy set `X-Forwarded-For` so the login rate limit
counts each client separately. Otherwise, all requests count as coming from the proxy,
and ten wrong passwords across any clients lock everyone out for a minute.

Open the web UI to begin setup. On its first start, stalias prints a setup code to the
container log (`docker logs stalias`). The setup wizard explains what to enter and where
to find the required credentials as you work through four steps:

1. Choose an admin password (protected by the setup code).
2. Connect to Stalwart with its URL and an API key. stalias tests the connection before it
   saves anything.
3. Pick the domains that may be used for aliases.
4. Create an API token for a mailbox and copy the Bitwarden settings shown in the wizard.

### Verify the image

Release images are built by GitHub Actions and carry a signed build provenance attestation. The
multi-arch image is also signed with [cosign](https://github.com/sigstore/cosign) (keyless). Check
either before you run it:

```sh
gh attestation verify oci://ghcr.io/untoastedtoast/stalias:latest --owner UntoastedToast

cosign verify ghcr.io/untoastedtoast/stalias:latest \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp '^https://github.com/UntoastedToast/stalias/\.github/workflows/release\.yml@refs/tags/v'
```

### The Stalwart API key

Create an API key in Stalwart's admin UI and replace its permissions with exactly this list:

`authenticate`, `sysAccountGet`, `sysAccountQuery`, `sysAccountUpdate`, `sysDomainGet`,
`sysDomainQuery`

The `sysAccountUpdate` permission also allows password resets for any mailbox. Treat the
key as a credential with write access to your mailboxes. It cannot grant roles or delete
accounts. stalias stores it in plain text in the database on the data volume. The database
file is created with read access limited to its owner; protect the volume and the web UI.

### Bitwarden

Generator → Username → type **Forwarded email alias** → service **Addy.io**, then:

| Field | Value |
| --- | --- |
| Email domain name | one of the domains you selected |
| API access token | the token from stalias |
| Self-host server URL | the public address of stalias |

Create a separate token for each mailbox you want to use. Each token belongs to one
mailbox, which receives mail sent to aliases created with that token.

## API

stalias implements the following alias management endpoints from the [Addy.io API](https://app.addy.io/docs).
Authenticate requests with `Authorization: Bearer <token>`:

| Method | Path | |
| --- | --- | --- |
| `POST` | `/api/v1/aliases` | create (`domain`, `description`, `format: custom` + `local_part`) |
| `GET` | `/api/v1/aliases` | list the token's aliases |
| `GET` `PATCH` `DELETE` | `/api/v1/aliases/{id}` | show, change the description, delete |
| `POST` | `/api/v1/active-aliases` | enable (`{"id": ...}`) |
| `DELETE` | `/api/v1/active-aliases/{id}` | disable |

The alias `id` is its full email address because Stalwart has no separate alias ID.
Addy.io's forwarding features, including recipients, labels and statistics, don't apply
here and are omitted.

Bitwarden's generator sends `POST /api/v1/aliases` with `{domain, description}` and reads
`data.email`, which stalias includes in its response.

For other clients, set their Addy.io server URL to your stalias address and use a stalias
API token. Compatibility depends on the endpoints and response fields the client uses.
stalias implements the subset listed above; clients that require additional endpoints
or UUID alias IDs may need changes.

## Requirements and limits

- Do not configure a catch-all on the alias domains: it would accept mail for disabled aliases.
- Stalwart creates a JMAP identity for each alias, so mail clients can send from it.
- Aliases are unique across all mailboxes on a Stalwart server. If a randomly generated
  name is taken, stalias generates another.
- stalias reads at most 500 mailboxes (Stalwart's per-request limit).
- Settings, tokens and sessions are stored in `/data/stalias.db` (SQLite). Tokens are
  stored as hashes and shown only once.
- Stalwart records no creation date for aliases, so stalias remembers the date of every alias
  created through it (web UI or API) in the same database. Aliases that already existed, or
  that you create in Stalwart directly, show no date and are left out when you filter by
  creation date.

## Configuration

Configure the app through the web UI.

Changing the Stalwart server URL revokes existing stalias API tokens and restarts domain
and token setup, because mailbox IDs belong to that server. Updating only the API key
keeps the tokens and selected domains.

These are the only environment variables:

| Variable | Default | |
| --- | --- | --- |
| `PORT` | `8080` | port inside the container |
| `DATA_DIR` | `/data` | where `stalias.db` lives; must be writable for UID 65532 |
| `TRUST_PROXY` | `false` | believe `X-Forwarded-For` (only enable behind a reverse proxy you control) |

## Development

```sh
cargo test                                     # unit tests and tests against a fake Stalwart
eval "$(scripts/setup-test-instance.sh)"       # throwaway Stalwart in Docker
cargo test -- --ignored --test-threads=1       # tests against that Stalwart
DATA_DIR=./data cargo run                      # start the app
```

### Project layout

| Path | Contents |
| --- | --- |
| `src/` | the app: `stalwart/` (JMAP client), `aliases.rs`, `store.rs` (SQLite), `auth.rs`, `web/` (Addy.io API and browser UI) |
| `src/web/templates/` | askama templates: `layouts/`, `pages/`, `components/` (macros, fragments) and `icons/` |
| `src/web/static/` | the CSS and JavaScript served by the app, compiled into the binary |
| `assets-build/` | build for the generated static files, see its [README](assets-build/README.md) |
| `tests/` | HTTP tests against a fake Stalwart (`web_mock.rs`) and a real one (`*_live.rs`, ignored by default) |
| `scripts/` | helper scripts, e.g. the throwaway Stalwart for the live tests |
| `docs/` | notes on Stalwart's behaviour and a Compose example |

The UI uses server-rendered HTML with [askama](https://github.com/askama-rs/askama) templates
and [htmx](https://htmx.org). [Tailwind CSS](https://tailwindcss.com) and
[Basecoat](https://basecoatui.com) (the shadcn/ui design system as plain CSS) provide the
styles; icons come from [Lucide](https://lucide.dev). The binary serves all assets locally,
without a CDN.

`src/web/static/app.css` and `src/web/static/basecoat.min.js` are generated files committed
to the repository, so you can build the Rust app without Node. After changing Tailwind
classes in `src/web/templates/` or `assets-build/styles.css`, run
`cd assets-build && npm ci && npm run build`.

[docs/stalwart-0.16-notes.md](docs/stalwart-0.16-notes.md) documents Stalwart's alias API,
including renumbered indexes, the lack of optimistic locking and permissions.

## Credits

[staliaswarden](https://github.com/romdim/staliaswarden) by romdim and its
[Stalwart 0.16 fork](https://github.com/goingdark-social/staliaswarden) by goingdark-social
introduced the idea of connecting Bitwarden to Stalwart. stalias uses regular account
aliases instead of Enterprise-only masked emails and shares no code with either project.

The web UI uses [Basecoat](https://basecoatui.com) (MIT), [htmx](https://htmx.org) (0BSD),
[Tailwind CSS](https://tailwindcss.com) (MIT), [Lucide](https://lucide.dev) icons (ISC) and the
[Geist](https://vercel.com/font) font (SIL OFL 1.1).

## License

[AGPL-3.0-only](LICENSE)
