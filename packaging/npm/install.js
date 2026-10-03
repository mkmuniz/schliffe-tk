#!/usr/bin/env node
// Postinstall: download the Schliffe prebuilt binary for this platform from
// the GitHub release, verify it against the release's SHA256SUMS, and place it
// in vendor/. No npm dependencies — only Node built-ins plus the system `tar`
// (present on macOS, Linux, and Windows 10+). On any failure it prints how to
// install manually and exits 0, so `npm install` never hard-fails on a binary
// fetch (the user can still run install.sh).
const fs = require("fs");
const os = require("os");
const path = require("path");
const https = require("https");
const crypto = require("crypto");
const { execFileSync } = require("child_process");

const VERSION = require("./package.json").version;
const REPO = "mkmuniz/schliffe-tk";

const TARGETS = {
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "linux-x64": "x86_64-unknown-linux-gnu",
  "win32-x64": "x86_64-pc-windows-msvc",
};

function fail(msg) {
  console.error(`schliffe: ${msg}`);
  console.error(
    "schliffe: install manually instead — see\n" +
      "  https://github.com/mkmuniz/schliffe-tk#install",
  );
  process.exit(0); // never hard-fail npm install over a binary download
}

// Follows redirects (GitHub release assets redirect to a CDN).
function get(url) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { "User-Agent": "schliffe-npm" } }, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume();
          resolve(get(res.headers.location));
          return;
        }
        if (res.statusCode !== 200) {
          reject(new Error(`HTTP ${res.statusCode} for ${url}`));
          return;
        }
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve(Buffer.concat(chunks)));
      })
      .on("error", reject);
  });
}

async function main() {
  const key = `${process.platform}-${process.arch}`;
  const target = TARGETS[key];
  if (!target) fail(`no prebuilt binary for ${key}`);
  const isWindows = process.platform === "win32";
  const ext = isWindows ? "zip" : "tar.gz";
  const asset = `schliffe-v${VERSION}-${target}.${ext}`;
  const base = `https://github.com/${REPO}/releases/download/v${VERSION}`;

  let archive, sums;
  try {
    archive = await get(`${base}/${asset}`);
    sums = (await get(`${base}/SHA256SUMS`)).toString("utf8");
  } catch (e) {
    fail(`download failed: ${e.message}`);
  }

  const expected = sums
    .split("\n")
    .map((l) => l.trim().split(/\s+/))
    .find((p) => p[1] === asset);
  if (!expected) fail(`${asset} not listed in SHA256SUMS`);
  const actual = crypto.createHash("sha256").update(archive).digest("hex");
  if (actual !== expected[0]) {
    fail(`checksum mismatch for ${asset} (expected ${expected[0]}, got ${actual})`);
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "schliffe-"));
  const archivePath = path.join(tmp, asset);
  fs.writeFileSync(archivePath, archive);
  try {
    // `tar` reads .zip too on Windows 10+.
    execFileSync("tar", ["-xf", archivePath, "-C", tmp]);
  } catch (e) {
    fail(`extract failed: ${e.message}`);
  }

  const exe = isWindows ? "schliffe.exe" : "schliffe";
  const extractedDir = path.join(tmp, `schliffe-v${VERSION}-${target}`);
  const src = path.join(extractedDir, exe);
  const vendor = path.join(__dirname, "vendor");
  fs.mkdirSync(vendor, { recursive: true });
  const dest = path.join(vendor, exe);
  fs.copyFileSync(src, dest);
  if (!isWindows) fs.chmodSync(dest, 0o755);
  fs.rmSync(tmp, { recursive: true, force: true });

  console.log(
    "schliffe: binary installed. Activate it with:\n" +
      "  schliffe setup        (preview: schliffe setup --dry-run)",
  );
}

main().catch((e) => fail(e.message));
