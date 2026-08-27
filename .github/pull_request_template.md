## What does this change?

<!-- One or two sentences. What is different after this? -->

## Why?

<!-- What problem does it fix? Link the issue if there is one, like: Fixes #12 -->

## How did you test it?

<!-- Tell us what you actually ran or clicked. "It builds" is not testing. -->

## Checks

<!-- Tick each box once you have done it. Put an x inside, like [x] -->

- [ ] `cargo fmt --all`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] `node --check extension/background.js` passes (only if you changed the extension)

## The rules

<!-- These are in CONTRIBUTING.md. Tick the ones that apply to your change. -->

- [ ] No `.unwrap()`, `.expect()`, `panic!` or `unreachable!` in real code
- [ ] Nothing blocks the GTK main loop
- [ ] Any new parser is tested against real output I captured, not guessed
- [ ] Any new subprocess reads stdout and stderr at the same time

## Anything else?

<!-- Screenshots help a lot for anything you can see. Delete this if empty. -->
