# shellcheck shell=bash
# Homebrew's names for the macOS architectures carbon6 builds for. Sourced.

# The bottle tag: Big Sur (macOS 11) is the oldest macOS Rust targets, and
# Homebrew pours a bottle on that release and every later one.
bottle_tag() {
    case $1 in
        arm64) echo arm64_big_sur ;;
        x86_64) echo big_sur ;;
        *) echo "unsupported architecture $1" >&2; return 1 ;;
    esac
}

# The formula block that selects the architecture.
on_arch() {
    case $1 in
        arm64) echo on_arm ;;
        x86_64) echo on_intel ;;
    esac
}
