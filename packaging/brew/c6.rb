class C6 < Formula
  desc "carbon6 command line: run and follow agent sessions through a carbon6 head"
  homepage "https://github.com/carbon6-dev/carbon6"
  version "@VERSION@"
  license any_of: ["MIT", "Apache-2.0"]
@SOURCES@

  bottle do
    root_url "@ROOT_URL@"
@BOTTLES@
  end

@DEPENDS_ON_ARCH@
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
