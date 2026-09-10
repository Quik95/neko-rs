{
  lib,
  rustPlatform,
  pkg-config,
  wayland,
  libxkbcommon,
  installShellFiles,
  makeWrapper,
}:
rustPlatform.buildRustPackage {
  pname = "nekors";
  version = (lib.importTOML ../Cargo.toml).workspace.package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset =
      lib.fileset.intersection
      (lib.fileset.gitTracked ../.)
      (lib.fileset.unions [
        ../Cargo.toml
        ../Cargo.lock
        ../LICENSE
        ../crates
        ../kwin-script
      ]);
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [pkg-config installShellFiles makeWrapper];
  buildInputs = [wayland libxkbcommon];

  # wayland is dlopened by the client library, so it has to stay findable at
  # runtime rather than only at link time.
  postInstall = ''
    wrapProgram $out/bin/nekors \
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [wayland libxkbcommon]}

    installShellCompletion --cmd nekors \
      --bash <($out/bin/nekors --completions bash) \
      --fish <($out/bin/nekors --completions fish) \
      --zsh <($out/bin/nekors --completions zsh)

    $out/bin/nekors --man > nekors.1
    installManPage nekors.1

    # The KWin script is half the program: without it nothing feeds the cursor
    # position in. It is installed into the package so the Home Manager module
    # can link it where KWin looks.
    mkdir -p $out/share/kwin/scripts/nekors
    cp -r kwin-script/. $out/share/kwin/scripts/nekors/
  '';

  meta = {
    description = "An oneko clone that chases the cursor across a Wayland desktop";
    homepage = "https://github.com/quik95/neko-rs";
    license = lib.licenses.eupl12;
    mainProgram = "nekors";
    platforms = lib.platforms.linux;
  };
}
