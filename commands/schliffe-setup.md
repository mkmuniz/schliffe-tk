---
description: Wire Schliffe's shell-command shim (git, cargo, docker…) after installing the plugin
---

# Schliffe setup

The Schliffe plugin already provides the Claude Code hooks (MCP result trimming,
image shrinking, pasted-log warnings). This command wires the **shell-command
shim** — the part a plugin cannot do, because it needs `~/.schliffe/shims` first
on your `PATH`.

## Steps

1. Confirm the `schliffe` binary is installed and on PATH:

   ```
   schliffe --version
   ```

   If that fails, install it first (pick one):
   - `brew install mkmuniz/schliffe/schliffe`
   - `cargo install schliffe`
   - `npx schliffe-tk install`
   - `curl -fsSL https://raw.githubusercontent.com/mkmuniz/schliffe-tk/main/install.sh | bash`

2. Show exactly what will change (nothing is written yet):

   ```
   schliffe setup --dry-run --no-hook
   ```

3. Apply it. `--no-hook` is important: the plugin already registers the hooks,
   so this only creates the shims and updates PATH — it will not touch
   `~/.claude/settings.json`:

   ```
   schliffe setup --no-hook
   ```

4. Open a new terminal (and restart Claude Code) so the new PATH takes effect.

To undo the shim later: `schliffe setup --uninstall`. To remove the hooks,
uninstall the plugin.

Report the output of the dry-run to the user and confirm before applying.
