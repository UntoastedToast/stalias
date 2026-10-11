# API

[Back to the README](../README.md)

stalias supports these alias management endpoints from the [Addy.io API](https://app.addy.io/docs).
Authenticate each request with `Authorization: Bearer <token>`:

| Method | Path | |
| --- | --- | --- |
| `POST` | `/api/v1/aliases` | create (`domain`, `description`, `format`: `random_words`, `random_characters` or `custom` + `local_part`) |
| `GET` | `/api/v1/aliases` | list the token's aliases |
| `GET` `PATCH` `DELETE` | `/api/v1/aliases/{id}` | show, change the description, delete |
| `POST` | `/api/v1/active-aliases` | enable (`{"id": ...}`) |
| `DELETE` | `/api/v1/active-aliases/{id}` | disable |

An alias's `id` is its full email address; Stalwart has no separate alias ID.
stalias omits Addy.io's forwarding features, including recipients, labels and statistics,
because they don't apply to Stalwart aliases.

Bitwarden's generator sends `POST /api/v1/aliases` with `{domain, description}` and reads
`data.email` from the response. Bitwarden sends no `format`, so stalias uses the alias
style you selected (see [Configuration](configuration.md#requirements-and-limits)).

For other clients, set their Addy.io server URL to your stalias address and use a stalias
API token. Check which endpoints and response fields the client needs. Clients that
require endpoints beyond those listed here, or UUID alias IDs, may need changes.
