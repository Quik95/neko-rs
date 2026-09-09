{
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
      default = pkgs.callPackage ./package.nix {};
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix {}";
      description = ''
        The nekors package to run.

        Built from the sources next to this file, so the module works however
        it was pulled in - as a flake input, by path, or from a fetched
        tarball. Set it to `inputs.neko-rs.packages.''${system}.nekors` to share
        the flake's build instead.
      '';
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

        The script still has to be switched on in kwinrc
        (`[Plugins] nekorsEnabled=true`). That entry is deliberately left to
        you: kwinrc is a whole-file target in Home Manager, so writing it here
        would collide with plasma-manager or with a hand-managed kwinrc. Under
        plasma-manager, add
        `programs.plasma.configFile.kwinrc.Plugins.nekorsEnabled = true`.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [cfg.package];

    xdg.dataFile."kwin/scripts/nekors" = lib.mkIf cfg.installKwinScript {
      source = "${cfg.package}/share/kwin/scripts/nekors";
      recursive = true;
    };

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
