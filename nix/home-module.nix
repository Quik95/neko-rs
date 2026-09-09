self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.nekors;

  # Flags are passed through verbatim rather than mirrored as options: the CLI
  # is the source of truth, and `nekors --help` documents it better than this
  # file could.
  arguments = lib.escapeShellArgs cfg.extraArgs;
in {
  options.services.nekors = {
    enable = lib.mkEnableOption "nekors, a cat that chases the cursor";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.nekors;
      defaultText = lib.literalExpression "inputs.neko-rs.packages.\${system}.nekors";
      description = "The nekors package to run.";
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      example = ["--scale" "2" "--type" "inu"];
      description = ''
        Command line arguments for nekors. See `nekors --help`.
      '';
    };

    installKwinScript = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Link the companion KWin script into the user's KWin script directory.

        Without it nothing feeds the cursor position in and the animal sits
        still, so this only makes sense to turn off when running a different
        cursor source.
      '';
    };

    enableKwinScript = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Write the kwinrc entry that switches the script on.

        KWin reads kwinrc at startup, so a freshly enabled script needs a
        relog - or `qdbus org.kde.KWin /Scripting org.kde.kwin.Scripting.start`.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [cfg.package];

    xdg.dataFile."kwin/scripts/nekors" = lib.mkIf cfg.installKwinScript {
      source = "${cfg.package}/share/kwin/scripts/nekors";
      recursive = true;
    };

    # KWin decides what to load from kwinrc, keyed by the plugin id in
    # metadata.json.
    xdg.configFile."kwinrc".text = lib.mkIf cfg.enableKwinScript (lib.mkAfter ''
      [Plugins]
      nekorsEnabled=true
    '');

    systemd.user.services.nekors = {
      Unit = {
        Description = "A cat that chases the cursor";
        Documentation = ["man:nekors(1)"];
        PartOf = ["graphical-session.target"];
        After = ["graphical-session.target"];
      };

      Service = {
        ExecStart = "${lib.getExe cfg.package} ${arguments}";
        # The compositor can take the surface away on a session switch, and the
        # KWin script may come back later than we do.
        Restart = "on-failure";
        RestartSec = 3;
        Slice = "session.slice";
      };

      Install.WantedBy = ["graphical-session.target"];
    };
  };
}
