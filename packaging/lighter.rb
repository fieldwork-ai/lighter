# Homebrew formula for lighter.
#
# Lives in a tap (fieldwork-ai/homebrew-tap) rather than homebrew-core: core
# will not take a formula that installs a binary needing a code-signing
# entitlement, and it is the entitlement that makes this work at all.
#
# The install signs the binary ad-hoc with `com.apple.security.virtualization`.
# That is not a workaround — it is how Apple intends an unsigned local build to
# get the entitlement, and the same thing `make sign` does in a checkout.
# Without it every start fails with `HV_DENIED`, which says nothing about
# signing.
class Lighter < Formula
  desc "Docker for macOS, on a virtual machine built for it"
  homepage "https://github.com/fieldwork-ai/lighter"
  url "https://github.com/fieldwork-ai/lighter/releases/download/v0.2.0/lighter-0.2.0-arm64.tar.gz"
  sha256 "9e011a954b70e5ea3e2f163e662524352154f502513060adc42d2319b3392e93"
  license "MIT"

  # Apple Silicon only, and not by omission: there is no Intel path and there
  # will not be one.
  depends_on arch: :arm64
  # The client, which lighter is not. lighter is the daemon.
  depends_on "docker"
  depends_on macos: :sequoia

  def install
    bin.install "bin/lighter"
    pkgshare.install Dir["share/lighter/*"]
    prefix.install "LICENSE", "README.md"
  end

  def post_install
    # Verify the signature; if unsigned or stripped, sign ad-hoc with the virtualization entitlement.
    unless quiet_system("/usr/bin/codesign", "--verify", bin/"lighter")
      system "/usr/bin/codesign", "--force", "--sign", "-",
             "--entitlements", pkgshare/"entitlements.plist",
             "--options", "runtime",
             bin/"lighter"
    end
  end

  def caveats
    <<~EOS
      Start it, and point the Docker CLI at it:

        lighter start

      To have it start when you log in:

        lighter install

      If anything is wrong, this says what:

        lighter doctor
    EOS
  end

  service do
    run [opt_bin/"lighter", "run"]
    keep_alive successful_exit: false
    log_path var/"log/lighter.log"
    error_log_path var/"log/lighter.log"
  end

  test do
    assert_match "lighter", shell_output("#{bin}/lighter --help")
    # `doctor` exits non-zero when something is missing, which in a sandbox is
    # everything — so this checks that it runs and reports, not that it passes.
    output = shell_output("#{bin}/lighter doctor", 1)
    assert_match "hardware virtualization", output
  end
end
