# Development

[Back to the README](../README.md) · [Contributing](../CONTRIBUTING.md)

Run these commands from the repository root:

```sh
cargo test                                     # unit tests and tests against a fake Stalwart
eval "$(scripts/setup-test-instance.sh)"       # throwaway Stalwart in Docker
cargo test -- --ignored --test-threads=1       # tests against that Stalwart
DATA_DIR=./data cargo run                      # start the app
```

On Unix, the terminal reset tests also need Python 3. They run with a throwaway database,
a pseudoterminal and a local server, leaving existing instances unchanged.

## Project layout

| Path | Contents |
| --- | --- |
| `src/` | the app: `stalwart/` (JMAP client), `aliases.rs`, `store.rs` (SQLite), `auth.rs`, `web/` (Addy.io API and browser UI) |
| `src/web/templates/` | askama templates: `layouts/`, `pages/`, `components/` (macros, fragments) and `icons/` |
| `src/web/static/` | the CSS and JavaScript served by the app, compiled into the binary |
| `assets-build/` | build for the generated static files, see its [README](../assets-build/README.md) |
| `tests/` | HTTP tests against a fake Stalwart (`web_mock.rs`) and a real one (`*_live.rs`, ignored by default) |
| `scripts/` | helper scripts, e.g. the throwaway Stalwart for the live tests |
| `docs/` | notes on Stalwart's behaviour and a Compose example |

The server renders HTML with [askama](https://github.com/askama-rs/askama) templates and
uses [htmx](https://htmx.org) for interactions. [Tailwind CSS](https://tailwindcss.com) and
[Basecoat](https://basecoatui.com), the shadcn/ui design system as plain CSS, provide the
styles. Icons come from [Lucide](https://lucide.dev). The binary serves all assets locally
without a CDN.

The repository includes the generated `src/web/static/app.css` and
`src/web/static/basecoat.min.js`, so building the Rust app doesn't need Node. After changing
Tailwind classes in `src/web/templates/` or `assets-build/styles.css`, run
`cd assets-build && npm ci && npm run build`.

The [Stalwart 0.16 notes](stalwart-0.16-notes.md) document Stalwart's alias API,
including renumbered indexes, the lack of optimistic locking and permissions.
