# frozen_string_literal: true

# Homebrew formula for pooltime; the published copy lives in chrisjenx/homebrew-tap (Formula/pooltime.rb).
# Installs the prebuilt release binary (the version is read from the URLs). After a release, run
# `packaging/homebrew/update-formula.sh <version>` to fill in the URLs and sha256s.
class Pooltime < Formula
  desc "Pool Claude Max and ChatGPT/Codex accounts behind one local endpoint"
  homepage "https://github.com/chrisjenx/pooltime"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/chrisjenx/pooltime/releases/download/v1.0.0/pooltime-1.0.0-aarch64-apple-darwin.tar.gz"
      sha256 "5256635b9f89067387b8ac85022e56435aa79b1bbc4a2c21168680193d9cf364" # sha256 filled by release
    end
    on_intel do
      url "https://github.com/chrisjenx/pooltime/releases/download/v1.0.0/pooltime-1.0.0-x86_64-apple-darwin.tar.gz"
      sha256 "5179be31c00f9eb0d23f3a3941cdb7d5925937f06891be5f8f77fb90a412091c" # sha256 filled by release
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/chrisjenx/pooltime/releases/download/v1.0.0/pooltime-1.0.0-aarch64-unknown-linux-musl.tar.gz"
      sha256 "3d66848a8fb2b2c1c4c30b11c15615d1a5c4045ca76d37c5ac86a8c41903186d" # sha256 filled by release
    end
    on_intel do
      url "https://github.com/chrisjenx/pooltime/releases/download/v1.0.0/pooltime-1.0.0-x86_64-unknown-linux-musl.tar.gz"
      sha256 "ee74cd7cc028a172594c6ea2b650283dbf20f94a6bd35f32f2b6683ecea89914" # sha256 filled by release
    end
  end

  def install
    bin.install "pooltime"
  end

  def caveats
    <<~EOS
      Finish with the setup wizard (accounts, background service, Claude Code profiles):
        pooltime setup

      Run the daemon either with `pooltime service install` (what `pooltime setup` offers)
      or with `brew services start pooltime`, not both: they would compete for the same port.

      Recommended on macOS, for notifications that open the dashboard when clicked:
        brew install terminal-notifier
    EOS
  end

  service do
    run [opt_bin/"pooltime", "serve"]
    keep_alive true
    log_path var/"log/pooltime.log"
    error_log_path var/"log/pooltime.log"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/pooltime --version")
  end
end
