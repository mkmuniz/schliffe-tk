# Homebrew formula for Schliffe (prebuilt binary from GitHub Releases).
#
# To publish a tap:
#   1. Create a repo named `homebrew-schliffe` under your account.
#   2. Copy this file to `Formula/schliffe.rb` there.
#   3. Fill the three `sha256` values from the release (see below), bump
#      `version`, and push.
# Users then install with:  brew install mkmuniz/schliffe/schliffe
#
# Fill the checksums after tagging a release (its workflow publishes
# SHA256SUMS). From this repo:
#   gh release download vX.Y.Z -p SHA256SUMS -O - | grep <target>
# or compute locally:  shasum -a 256 schliffe-vX.Y.Z-<target>.tar.gz
class Schliffe < Formula
  desc "Cuts token waste in Claude Code sessions (shell output, MCP results, prose)"
  homepage "https://github.com/mkmuniz/schliffe-tk"
  version "0.4.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/mkmuniz/schliffe-tk/releases/download/v#{version}/schliffe-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_APPLE_DARWIN_SHA256"
    end
    on_intel do
      url "https://github.com/mkmuniz/schliffe-tk/releases/download/v#{version}/schliffe-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_X86_64_APPLE_DARWIN_SHA256"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/mkmuniz/schliffe-tk/releases/download/v#{version}/schliffe-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_X86_64_UNKNOWN_LINUX_GNU_SHA256"
    end
  end

  def install
    bin.install "schliffe"
  end

  def caveats
    <<~EOS
      Activate Schliffe (shims for git/cargo/docker… + the Claude Code hook):
        schliffe setup
      Preview the changes first:  schliffe setup --dry-run
      Undo later:                 schliffe setup --uninstall
    EOS
  end

  test do
    assert_match "schliffe", shell_output("#{bin}/schliffe --version")
  end
end
