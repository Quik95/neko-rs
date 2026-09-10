{pkgs, ...}: {
  languages.rust = {
    enable = true;
    channel = "stable";
    components = ["rustc" "cargo" "clippy" "rustfmt" "rust-analyzer"];
  };

  packages = with pkgs; [
    pkg-config
    wayland
    wayland-protocols
    wayland-scanner
    libxkbcommon
    libxkbcommon.dev
    jq
    kdePackages.qttools # qdbus, for poking KWin's /Scripting

    # The same tools CI runs, so a failure there is reproducible here.
    cargo-deny
    zizmor
  ];

  env.RUST_LOG = "nekors=info";

  scripts.install-kwin-script.exec = ''
    set -eu
    dest="$HOME/.local/share/kwin/scripts/nekors"
    mkdir -p "$dest"
    cp -r "$DEVENV_ROOT/kwin-script/." "$dest/"
    echo "installed to $dest"
    echo "enable with: kwriteconfig6 --file kwinrc --group Plugins --key nekorsEnabled true"
  '';

  scripts.reload-kwin-script.exec = ''
    set -eu
    install-kwin-script
    state="''${XDG_RUNTIME_DIR:-/tmp}/nekors-kwin-script"

    # KWin keeps a registration under the plugin name even when the script
    # failed to evaluate, and a later loadScript under that name is then a
    # no-op. Loading under a fresh name every time sidesteps it; the previous
    # name is remembered so it can be unloaded first.
    if [ -f "$state" ]; then
      qdbus org.kde.KWin /Scripting org.kde.kwin.Scripting.unloadScript "$(cat "$state")" >/dev/null || true
    fi
    qdbus org.kde.KWin /Scripting org.kde.kwin.Scripting.unloadScript nekors >/dev/null || true

    name="nekors-$(date +%s)"
    qdbus org.kde.KWin /Scripting org.kde.kwin.Scripting.loadScript \
      "$HOME/.local/share/kwin/scripts/nekors/contents/code/main.js" "$name" >/dev/null
    qdbus org.kde.KWin /Scripting org.kde.kwin.Scripting.start
    printf %s "$name" > "$state"
    echo "loaded as $name"
  '';

  git-hooks.hooks = {
    rustfmt.enable = true;
    clippy = {
      enable = true;
      settings.denyWarnings = true;
    };
    alejandra.enable = true;
    actionlint.enable = true;
  };
}
