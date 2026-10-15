class C6w < Formula
  desc "carbon6 worker: runs agent sessions on this Mac for a carbon6 head"
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
    bin.install "c6w"
  end

  post_install_steps do
    mkdir_p "log", base: :var
  end

  # Runs as you, through a login shell so sessions get your PATH.
  service do
    run ["/bin/zsh", "-lc", "exec #{opt_bin}/c6w run"]
    keep_alive successful_exit: false
    environment_variables NO_COLOR: "1"
    log_path var/"log/c6w.log"
    error_log_path var/"log/c6w.log"
  end

  def caveats
    <<~EOS
      Enroll with a token from the head's Workers page, then start the worker:
        c6w enroll <token>
        brew services start c6w   (log: #{var}/log/c6w.log)
    EOS
  end

  test do
    assert_match "c6w", shell_output("#{bin}/c6w --version")
  end
end
