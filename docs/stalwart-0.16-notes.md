# Stalwart 0.16 JMAP behaviour relevant to stalias

Verified against `stalwartlabs/stalwart:v0.16.22` (throwaway instance from `scripts/setup-test-instance.sh`), 2026-10-10.
All management calls use `POST /jmap/` with `using: ["urn:ietf:params:jmap:core", "urn:stalwart:jmap"]`.

## Session and account id

- `GET /jmap/session` works with Basic auth (user/password) or `Authorization: Bearer <API key>`.
- The `accountId` for `x:` management calls is the id from the session's `accounts` map (`"b"` on a fresh instance).
  It can be omitted from requests; the server fills it in.
- A fresh server runs in *bootstrap mode*. Only `x:Bootstrap/get|set` is available until the container is restarted
  after `x:Bootstrap/set` (the response contains the permanent admin credentials). stalias does not need to do this.

## Aliases (`x:Account`)

- `aliases` is a **map with positional keys** (`{"0": {...}, "1": {...}}`), each entry `{name, domainId, enabled, description}`.
- Patch paths work: add `"aliases/N": {...}`, disable `"aliases/N/enabled": false`, remove `"aliases/N": null`.
- **Keys are not stable.** After removing `aliases/0` the remaining entry was renumbered to `0`.
  Always re-read the account and look the alias up by name right before writing. Never cache an index.
- **No optimistic locking.** `x:Account/get` returns no `state`, and a bogus `ifInState` is accepted. Writes must be
  serialised inside stalias (one mutex around read-modify-write).
- Duplicate local parts **within one account** are accepted silently, so stalias must check for them itself.
- Uniqueness **across accounts** is enforced: creating or updating gives `primaryKeyViolation`
  (`objectId` names the account that already owns the address). Aliases also collide with account names.
- Each alias gets a JMAP `Identity` automatically.
- The domain has `subAddressing` (`+tag`) enabled by default and an optional `catchAllAddress`.

## Delivery

- SMTP `RCPT TO` for an enabled alias: `250`. For a disabled or removed alias: `550 5.1.2 Mailbox does not exist`.
  Nothing is accepted and bounced later, so a disabled alias really stops receiving.

## Minimal API key

```json
["x:ApiKey/set", {"create": {"k": {"description": "stalias", "allowedIps": {},
  "permissions": {"@type": "Replace", "permissions": {
    "authenticate": true, "sysAccountGet": true, "sysAccountQuery": true,
    "sysAccountUpdate": true, "sysDomainGet": true, "sysDomainQuery": true}}}}}, "c1"]
```

- `authenticate` is required, otherwise even `/jmap/session` returns 403.
- With this key: list domains and accounts, edit aliases. Denied: reading API keys, destroying accounts, granting roles.
- **Caveat:** `sysAccountUpdate` also lets the key set another account's password (`credentials/0/secret`).
  The key must be treated like a credential with write access to every mailbox. Document this in the README.
