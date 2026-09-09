# frozen_string_literal: true

# Homebrew formula for lighter.
#
# Lives in a tap (fieldwork-ai/homebrew-tap) rather than homebrew-core: core
# will not take a formula that installs a binary needing a code-signing
# entitlement, and it is the entitlement that makes this work at all.
#
# Release binaries carry a notarized Developer ID signature and the hypervisor
# entitlement. post_install verifies signatures without modifying the signed payload.
class Lighter < Formula
  desc "Docker for macOS, on a virtual machine built for it"
  homepage "https://github.com/fieldwork-ai/lighter"
  url "https://github.com/fieldwork-ai/lighter/releases/download/v0.5.2/lighter-0.5.2-arm64.tar.gz"
  version "0.5.2"
  sha256 "dfed1e381635697e48d97b993ffe3bcedc8c08ebcdbb9f00ad08924f68026ee9"
  license any_of: ["MIT", "Apache-2.0"]

  # Apple Silicon only, and not by omission: there is no Intel path and there
  # will not be one.
  depends_on arch: :arm64
  # The client, which lighter is not. lighter is the daemon.
  depends_on "docker"
  depends_on macos: :sequoia

  def install
    bin.install "bin/lighter"
    pkgshare.install Dir["share/lighter/*"]
    prefix.install "LICENSE-MIT", "LICENSE-APACHE", "README.md"
  end

  # Ownership includes a digest of the final canonical Cellar path.
  # rubocop:disable FormulaAudit/InstallSteps
  def post_install
    # Brew owns upgrades; the CLI must never replace this keg itself.
    require "digest"
    require "json"
    managed_prefix = prefix.parent.realpath.to_s
    metadata = {
      schema: 1,
      method: "homebrew",
      prefix: managed_prefix,
      id:     Digest::SHA256.hexdigest(managed_prefix)[0, 24],
    }
    File.write(pkgshare/"installation.json", JSON.pretty_generate(metadata))
    system "/usr/bin/codesign", "--verify", "--strict", bin/"lighter"
    system "/usr/bin/codesign", "--verify", "--strict", "--deep", pkgshare/"lighter.app"
  end
  # rubocop:enable FormulaAudit/InstallSteps

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
