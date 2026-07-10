#!/bin/sh
# cortex installer — the CLI only.
#
#   curl -sSL https://get.cortex.dev | sh
#
# (For the full Aix OS stack — brains, tunnel, server — use ./install.sh.)
#
# Downloads a static binary, verifies its checksum, installs it, and then —
# because this tool's entire claim is that undo works — points you at
# `cortex verify --self`, which proves that claim on your own machine.
#
# POSIX sh, no bashisms: this runs on whatever is on the box.
set -eu

REPO="${CORTEX_REPO:-cortex-run/cortex}"
VERSION="${CORTEX_VERSION:-latest}"
BIN_DIR="${CORTEX_BIN_DIR:-}"
BIN_NAME="cortex"

# ── output ───────────────────────────────────────────────────────────────
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m')
    RED=$(printf '\033[31m'); GREEN=$(printf '\033[32m')
    YELLOW=$(printf '\033[33m'); RESET=$(printf '\033[0m')
else
    BOLD=; DIM=; RED=; GREEN=; YELLOW=; RESET=
fi

say()  { printf '  %s\n' "$*"; }
ok()   { printf '  %s✔%s %s\n' "$GREEN" "$RESET" "$*"; }
warn() { printf '  %s!%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die()  { printf '\n%s✘%s %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "\`$1\` is required but not installed."
}

# ── platform ─────────────────────────────────────────────────────────────
detect_target() {
    os=$(uname -s)
    arch=$(uname -m)

    [ "$os" = "Linux" ] || die "cortex is Linux-only: it uses OverlayFS and systemd.
    (detected: $os)"

    case "$arch" in
        x86_64|amd64)  echo "x86_64-unknown-linux-musl" ;;
        aarch64|arm64) echo "aarch64-unknown-linux-musl" ;;
        *) die "unsupported architecture: $arch" ;;
    esac
}

# Prefer a system path if writable, else a user path. Never install silently
# somewhere that is not on PATH.
detect_bin_dir() {
    if [ -n "$BIN_DIR" ]; then echo "$BIN_DIR"; return; fi
    if [ "$(id -u)" = 0 ]; then echo /usr/local/bin; return; fi
    if [ -w /usr/local/bin ] 2>/dev/null; then echo /usr/local/bin; return; fi
    echo "$HOME/.local/bin"
}

# ── preflight ────────────────────────────────────────────────────────────
preflight() {
    # OverlayFS is the transaction engine; without it cortex cannot sandbox.
    if ! grep -qw overlay /proc/filesystems 2>/dev/null; then
        warn "the kernel does not report overlayfs support"
        warn "cortex needs it to run changes in a sandbox before committing"
    fi
    command -v systemctl >/dev/null 2>&1 || \
        warn "systemd not found — service templates will not be usable"
    command -v docker >/dev/null 2>&1 || \
        say "${DIM}docker not found — container templates will be skipped${RESET}"
}

main() {
    need uname; need mktemp; need tar
    if command -v curl >/dev/null 2>&1; then
        fetch()    { curl -fsSL "$1"; }
        fetch_to() { curl -fsSL -o "$2" "$1"; }
    elif command -v wget >/dev/null 2>&1; then
        fetch()    { wget -qO- "$1"; }
        fetch_to() { wget -qO "$2" "$1"; }
    else
        die "need curl or wget"
    fi

    printf '\n%scortex%s %s\n\n' "$BOLD" "$RESET" \
        "${DIM}run any change transactionally · verify it · undo it with proof${RESET}"

    target=$(detect_target)
    bin_dir=$(detect_bin_dir)
    preflight

    if [ "$VERSION" = "latest" ]; then
        VERSION=$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
            | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
        [ -n "$VERSION" ] || die "could not determine the latest version.
    Pin one explicitly:  CORTEX_VERSION=v0.1.0 sh"
    fi

    asset="$BIN_NAME-$VERSION-$target.tar.gz"
    base="https://github.com/$REPO/releases/download/$VERSION"

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT INT TERM

    say "downloading ${BOLD}$BIN_NAME $VERSION${RESET} ($target)"
    fetch_to "$base/$asset" "$tmp/$asset" || die "download failed: $base/$asset"

    # A binary that mutates your system as root is worth verifying.
    if fetch_to "$base/$asset.sha256" "$tmp/$asset.sha256" 2>/dev/null; then
        if command -v sha256sum >/dev/null 2>&1; then
            want=$(cut -d' ' -f1 < "$tmp/$asset.sha256")
            got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
            [ "$want" = "$got" ] || die "checksum mismatch
    expected $want
    got      $got"
            ok "checksum verified"
        fi
    else
        warn "no published checksum for this release; skipping verification"
    fi

    tar -xzf "$tmp/$asset" -C "$tmp"
    [ -f "$tmp/$BIN_NAME" ] || die "archive did not contain $BIN_NAME"
    chmod +x "$tmp/$BIN_NAME"

    mkdir -p "$bin_dir" 2>/dev/null || true
    if [ -w "$bin_dir" ]; then
        mv "$tmp/$BIN_NAME" "$bin_dir/$BIN_NAME"
    elif command -v sudo >/dev/null 2>&1; then
        say "installing to $bin_dir (needs sudo)"
        sudo mv "$tmp/$BIN_NAME" "$bin_dir/$BIN_NAME"
    else
        die "cannot write to $bin_dir and sudo is unavailable.
    Retry with:  CORTEX_BIN_DIR=\$HOME/.local/bin sh"
    fi
    ok "installed $bin_dir/$BIN_NAME"

    case ":$PATH:" in
        *":$bin_dir:"*) ;;
        *) warn "$bin_dir is not on your PATH"
           say  "${DIM}add:  export PATH=\"$bin_dir:\$PATH\"${RESET}" ;;
    esac

    install_policy "$bin_dir"

    # cortex mounts overlays and writes /var/lib/cortex; it needs root for
    # real work. Say so now rather than at the first confusing failure.
    printf '\n'
    say "${BOLD}cortex needs root${RESET} to sandbox changes and record undo history."
    say "${DIM}run it with sudo. cortex enforces its own deny-by-default policy${RESET}"
    say "${DIM}in ${POLICY_FILE}, so root invocation is still constrained.${RESET}"

    printf '\n%s▸%s %s\n' "$BOLD" "$RESET" "next"
    say "${BOLD}cortex verify --self${RESET}   ${DIM}prove undo works, on this machine${RESET}"
    say "${BOLD}cortex try \"run nginx on port 8080\"${RESET}"
    say "${BOLD}cortex status${RESET}          ${DIM}what is applied, what is undoable${RESET}"
    say "${BOLD}cortex undo${RESET}            ${DIM}reverse it, with proof${RESET}"
    printf '\n'
}

POLICY_DIR=/etc/cortex
POLICY_FILE="$POLICY_DIR/policy.toml"

# Seed the root-owned policy that constrains cortex even when it runs as root.
#
# cortex refuses to honour a policy file it does not trust, so this must be
# root-owned and not group/world-writable. We never install a NOPASSWD sudoers
# rule automatically: a rule pointing at a user-writable binary is a local
# privilege escalation, and we cannot audit the operator's intent from here.
install_policy() {
    bin_dir="$1"
    [ "$(id -u)" = 0 ] || command -v sudo >/dev/null 2>&1 || return 0

    if [ -e "$POLICY_FILE" ]; then
        ok "policy already present at $POLICY_FILE (left untouched)"
        return 0
    fi

    as_root mkdir -p "$POLICY_DIR" || return 0
    tmp_pol="$tmp/policy.toml"
    cat > "$tmp_pol" <<'POLICY'
# cortex authorization policy. Root-owned; cortex refuses to load it otherwise.
#
# First matching rule wins. No match is a refusal (deny-by-default), so a
# grant of "may run cortex as root" is bounded by what is written here.
#
# Selectors: template:<id>, workflow:<kind>, irreversible, undo, or *
# Decisions: allow | audit | deny | needs_approval

[[rules]]
op = "undo"
decision = "allow"
name = "undo is always permitted"

# Anything cortex cannot reverse must be consented to explicitly
# (cortex --yes-irreversible), and is journaled as irreversible.
[[rules]]
op = "irreversible"
decision = "needs_approval"
name = "irreversible operations need explicit consent"

# Reversible operations: every one journals an inverse and a post-condition
# that proves the inverse worked. Narrow these as you like, e.g.
#   [[rules]]
#   op = "template:docker.run"
#   decision = "allow"
#   [rules.args]
#   image = "ghcr.io/your-org/*"
[[rules]]
op = "template:*"
decision = "allow"
name = "reversible templates"

[[rules]]
op = "workflow:*"
decision = "allow"
name = "reversible workflows"

# Everything else is denied by the absence of a rule.
POLICY
    if as_root install -o root -g root -m 0644 "$tmp_pol" "$POLICY_FILE"; then
        ok "wrote $POLICY_FILE (root-owned, deny-by-default)"
    else
        warn "could not write $POLICY_FILE; cortex will use its built-in default policy"
    fi

    check_sudoers_safety "$bin_dir/$BIN_NAME"
}

as_root() {
    if [ "$(id -u)" = 0 ]; then "$@"; else sudo "$@"; fi
}

# Explain how to grant passwordless invocation *safely*, and refuse to
# recommend it when the binary path is writable by a non-root user — that
# would be a local privilege escalation, not a convenience.
check_sudoers_safety() {
    bin="$1"
    owner=$(stat -c '%U' "$bin" 2>/dev/null || echo '?')
    dir_owner=$(stat -c '%U' "$(dirname "$bin")" 2>/dev/null || echo '?')

    printf '\n%s▸%s %s\n' "$BOLD" "$RESET" "passwordless invocation (optional)"
    if [ "$owner" != root ] || [ "$dir_owner" != root ]; then
        warn "NOT eligible: $bin is owned by '$owner' (dir: '$dir_owner')."
        say  "${DIM}A NOPASSWD rule on a user-writable path is a root escalation:${RESET}"
        say  "${DIM}replace the binary, get root. Install to a root-owned path first.${RESET}"
        return 0
    fi
    say "${DIM}$bin is root-owned, so a narrow NOPASSWD rule is safe:${RESET}"
    say ""
    say "  sudo groupadd -f cortex && sudo usermod -aG cortex \$USER"
    say "  echo '%cortex ALL=(root) NOPASSWD: $bin' | sudo tee /etc/sudoers.d/cortex"
    say "  sudo chmod 0440 /etc/sudoers.d/cortex && sudo visudo -c"
    say ""
    say "${DIM}Safe because cortex enforces $POLICY_FILE on every operation:${RESET}"
    say "${DIM}the grant is 'may exec this binary', not 'may do anything as root'.${RESET}"
}

main "$@"
