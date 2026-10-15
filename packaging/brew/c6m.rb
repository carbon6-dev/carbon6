class C6m < Formula
  desc "carbon6 memex: the carbon6 knowledge store, attached to a carbon6 head"
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
    bin.install "c6m"
  end

  post_install_steps do
    mkdir_p "log", base: :var
  end

  service do
    run [opt_bin/"c6m", "run"]
    keep_alive successful_exit: false
    environment_variables NO_COLOR: "1", PATH: std_service_path_env
    log_path var/"log/c6m.log"
    error_log_path var/"log/c6m.log"
  end

  def caveats
    <<~EOS
      Enroll with a token from the head's Settings → Memex, then start the memex:
        c6m enroll <token>
        brew services start c6m   (log: #{var}/log/c6m.log)
      It finds ffmpeg (brew install ffmpeg) for video and audio.
    EOS
  end

  test do
    assert_match "c6m", shell_output("#{bin}/c6m --version")
  end
end
