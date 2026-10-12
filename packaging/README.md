# Packaging & distribution

Every install path does the same two things: **get the `schliffe` binary onto
the system**, then **`schliffe setup`** to activate it (shims, PATH, Claude Code
hook). Activation lives in the binary (`schliffe setup`), so each path below is
just a different way to deliver the binary.

Preview activation with `schliffe setup --dry-run`; undo with
`schliffe setup --uninstall`.

| Path | User command | Publisher action needed |
|------|--------------|-------------------------|
| Shell script | `curl -fsSL …/install.sh \| bash` | none (already live) |
| Cargo | `cargo install schliffe && schliffe setup` | `cargo publish` |
| Homebrew | `brew install mkmuniz/schliffe/schliffe && schliffe setup` | create tap, fill checksums |
| npm | `npm i -g schliffe-tk && schliffe setup` | `npm publish` |
| Claude Code plugin | `claude plugin marketplace add mkmuniz/schliffe-tk` → `claude plugin install schliffe@schliffe-tk` → `/schliffe-setup` | none (files are in this repo) |

## Cargo (crates.io)

The crate is publish-ready (`cargo publish --dry-run` passes). To release:

```sh
cargo publish            # needs a crates.io token: cargo login <token>
```

Users then: `cargo install schliffe && schliffe setup`.

## Homebrew tap

1. Create a repo `homebrew-schliffe` under your account.
2. Copy `packaging/homebrew/schliffe.rb` to `Formula/schliffe.rb` there.
3. Fill the three `sha256` placeholders from the release's checksums:

   ```sh
   gh release download v0.4.0 -p SHA256SUMS -O -
   ```

4. Commit and push. Users: `brew install mkmuniz/schliffe/schliffe && schliffe setup`.

Bump `version` in the formula on each release and refresh the checksums.

## npm

`packaging/npm/` is a self-contained package. Its `postinstall` downloads the
prebuilt binary for the user's platform from the GitHub release and verifies it
against `SHA256SUMS`.

```sh
cd packaging/npm
npm publish              # needs npm login
```

Users then: `npm i -g schliffe-tk && schliffe setup`. Keep the package
`version` in step with the release tag (the download URL is derived from it).

## Claude Code plugin

The plugin manifest (`.claude-plugin/plugin.json`), marketplace
(`.claude-plugin/marketplace.json`), hooks (`hooks/hooks.json`) and the
`/schliffe-setup` command live at the repo root, so no separate publish step is
needed:

```
claude plugin marketplace add mkmuniz/schliffe-tk
claude plugin install schliffe@schliffe-tk
/schliffe-setup
```

The plugin ships the Claude Code hooks; `/schliffe-setup` runs
`schliffe setup --no-hook` so the hooks are not registered twice. The binary
still comes from one of the paths above.
