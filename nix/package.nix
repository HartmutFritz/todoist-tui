{
  lib,
  rustPlatform,
  cmake,
  makeWrapper,
  neovim,
  wl-clipboard,
  xclip,
  xsel,
  cacert,
}:
let
  manifest = builtins.fromTOML (builtins.readFile ../Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = manifest.package.name;
  version = manifest.package.version;
  src = import ./source.nix { inherit lib; };

  cargoLock.lockFile = ../Cargo.lock;
  # Keep the Cargo-selected AWS-LC and bundled SQLite versions, including when
  # newer nixpkgs defaults require externally installed C libraries.
  dontCargoUseSystemLibs = true;
  nativeBuildInputs = [ cmake makeWrapper ];
  nativeCheckInputs = [ neovim ];
  doCheck = true;
  # Loopback mock HTTP servers and embedded Neovim run without network access.
  checkFlags = [ "--test-threads=4" ];
  preCheck = ''
    export HOME="$TMPDIR/test-home"
    export XDG_CONFIG_HOME="$HOME/.config"
    export XDG_DATA_HOME="$HOME/.local/share"
    export XDG_CACHE_HOME="$HOME/.cache"
    export XDG_STATE_HOME="$HOME/.local/state"
    export TERM=xterm-256color
    export SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt
    unset TODOIST_API_TOKEN TODOIST_TUI_TOKEN_PATH TODOIST_TEST_API_TOKEN
    unset TODOIST_TUI_CONFIG_PATH TODOIST_TUI_DB_PATH
    mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_STATE_HOME"
  '';

  postInstall = ''
    install -Dm644 config.example.toml "$out/share/todoist-tui/config.example.toml"
    install -Dm644 LICENSE "$out/share/licenses/todoist-tui/LICENSE"
    wrapProgram "$out/bin/todoist-tui" \
      --suffix PATH : ${lib.makeBinPath [ neovim wl-clipboard xclip xsel ]} \
      --run ${lib.escapeShellArg ''
        if [ -z "''${SSL_CERT_FILE+x}" ] && [ -z "''${SSL_CERT_DIR+x}" ]; then
          if [ -n "''${NIX_SSL_CERT_FILE:-}" ]; then
            export SSL_CERT_FILE="$NIX_SSL_CERT_FILE"
          else
            for bundle in \
              /etc/ssl/certs/ca-certificates.crt \
              /etc/pki/tls/certs/ca-bundle.crt \
              /etc/ssl/ca-bundle.pem \
              /etc/pki/tls/cert.pem \
              /etc/ssl/cert.pem; do
              if [ -r "$bundle" ]; then
                export SSL_CERT_FILE="$bundle"
                break
              fi
            done
            if [ -z "''${SSL_CERT_FILE+x}" ]; then
              export SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt
            fi
          fi
        fi
      ''}
  '';

  meta = {
    inherit (manifest.package) description homepage;
    license = lib.licenses.mit;
    mainProgram = "todoist-tui";
    platforms = [ "x86_64-linux" "aarch64-linux" ];
  };
}
