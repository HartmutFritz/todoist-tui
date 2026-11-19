{ pkgs, package }:
{
  inherit package;
  runtime = pkgs.runCommand "todoist-tui-runtime-check" {
    nativeBuildInputs = [ pkgs.python3 ];
  } ''
    python3 ${./runtime-check.py} \
      ${package}/bin/todoist-tui \
      ${pkgs.lib.getExe pkgs.neovim} \
      ${pkgs.runtimeShell}
    touch "$out"
  '';
}
