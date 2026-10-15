class C6 < Formula
  desc "carbon6 command line: run and follow agent sessions through a carbon6 head"
  homepage "https://github.com/carbon6-dev/carbon6"
  url "@URL@"
  version "@VERSION@"
  sha256 "@SHA256@"
  license "Apache-2.0"

  bottle do
    root_url "@ROOT_URL@"
    sha256 cellar: :any_skip_relocation, @BOTTLE_TAG@: "@BOTTLE_SHA256@"
  end

  depends_on arch: :@ARCH@
  depends_on :macos

  def install
    bin.install "c6"
  end

  def caveats
    <<~EOS
      Sign in once with a token from the head (Settings → Sign-in, or `c6h api-token`):
        c6 login <url> --token <token>
    EOS
  end

  test do
    assert_match "c6", shell_output("#{bin}/c6 --version")
  end
end
