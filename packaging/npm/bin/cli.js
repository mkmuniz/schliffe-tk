#!/usr/bin/env node
// Thin launcher: exec the vendored native binary with the same args and stdio,
// propagating its exit code. The PATH shims created by `schliffe setup` point
// straight at the native binary (not this launcher), so per-command filtering
// never pays the Node startup cost — only direct `schliffe ...` calls do.
const path = require("path");
const fs = require("fs");
const { spawnSync } = require("child_process");

const exe = process.platform === "win32" ? "schliffe.exe" : "schliffe";
const bin = path.join(__dirname, "..", "vendor", exe);

if (!fs.existsSync(bin)) {
  console.error(
    "schliffe: native binary missing (postinstall did not complete).\n" +
      "Reinstall, or see https://github.com/mkmuniz/schliffe-tk#install",
  );
  process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`schliffe: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
