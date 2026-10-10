# Contributing

Bug reports and pull requests are welcome.

- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` before opening a PR.
- Changes that touch how stalias talks to Stalwart should also pass the live tests:
  `eval "$(scripts/setup-test-instance.sh)" && cargo test -- --ignored --test-threads=1`
  (needs Docker; it starts and removes a throwaway Stalwart container).
- If you change Tailwind classes in `src/web/templates/` or `assets-build/styles.css`, rebuild the committed CSS
  with `cd assets-build && npm ci && npm run build`; CI fails when `src/web/static/` is out of date.
- Keep it small: stalias manages aliases and nothing else. Mail delivery, users and domains stay in Stalwart.
- By contributing you agree that your work is licensed under AGPL-3.0-only, like the rest of the project.
