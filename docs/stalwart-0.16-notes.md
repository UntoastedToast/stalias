# Stalwart 0.16 JMAP behaviour relevant to stalias

These observations were verified on 2026-10-10 against `stalwartlabs/stalwart:v0.16.22`,
using a throwaway instance from `scripts/setup-test-instance.sh`. All management calls
use `POST /jmap/` with `using: ["urn:ietf:params:jmap:core", "urn:stalwart:jmap"]`.

## Session and account id

- `GET /jmap/session` works with Basic auth (user/password) or `Authorization: Bearer <API key>`.
- The `accountId` for `x:` management calls comes from the session's `accounts` map
  (`"b"` on a fresh instance). If you omit it, the server fills it in.
- A fresh server runs in *bootstrap mode*, with only `x:Bootstrap/get|set` available.
  After `x:Bootstrap/set`, restart the container to leave this mode. The response contains
  the permanent admin credentials. stalias does not need this bootstrap step.

## Aliases (`x:Account`)

- `aliases` is a map with positional keys (`{"0": {...}, "1": {...}}`). Each entry is
  `{name, domainId, enabled, description}`.
- Patch paths work: add `"aliases/N": {...}`, disable `"aliases/N/enabled": false`, remove `"aliases/N": null`.
- Keys change when an entry is removed: after removing `aliases/0`, the remaining entry
  was renumbered to `0`. Re-read the account and look up the alias by name immediately
  before writing. Never cache an index.
- There is no optimistic locking. `x:Account/get` returns no `state`, and the server accepts
  a bogus `ifInState`. stalias must serialise writes with one mutex around read-modify-write.
- The server silently accepts duplicate local parts within one account. stalias must
  check for these itself.
- Across accounts, creating or updating a duplicate returns `primaryKeyViolation`.
  `objectId` names the account that already owns the address. Aliases also collide with
  account names.
- Each alias gets a JMAP `Identity` automatically.
- The domain has `subAddressing` (`+tag`) enabled by default and an optional `catchAllAddress`.

## Delivery

- SMTP `RCPT TO` returns `250` for an enabled alias and `550 5.1.2 Mailbox does not exist`
  for a disabled or removed alias. Stalwart rejects the message during SMTP without
  accepting it and sending a bounce later.

## Minimal API key

```json
["x:ApiKey/set", {"create": {"k": {"description": "stalias", "allowedIps": {},
  "permissions": {"@type": "Replace", "permissions": {
    "authenticate": true, "sysAccountGet": true, "sysAccountQuery": true,
    "sysAccountUpdate": true, "sysDomainGet": true, "sysDomainQuery": true}}}}}, "c1"]
```

- Without `authenticate`, even `/jmap/session` returns 403.
- This key can list domains and accounts and edit aliases. It cannot read API keys,
  destroy accounts or grant roles.
- `sysAccountUpdate` also lets the key set another account's password (`credentials/0/secret`).
  Treat it as a credential with write access to every mailbox and document this in the README.
