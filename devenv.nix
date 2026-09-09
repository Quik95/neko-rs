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
    kdePackages.kdbusaddons # qdbus for poking KWin's /Scripting
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
    qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.unloadScript nekors || true
    qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.loadScript \
      "$HOME/.local/share/kwin/scripts/nekors/contents/code/main.js" nekors
    qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.start
    echo "reloaded"
  '';

  git-hooks.hooks = {
    rustfmt.enable = true;
    clippy = {
      enable = true;
      settings.denyWarnings = true;
    };
    alejandra.enable = true;
  };
}
