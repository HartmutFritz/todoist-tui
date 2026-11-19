{ lib }:
# An allowlist keeps credentials, caches, logs and local tooling out of builds,
# including when this expression is called directly from a working checkout.
lib.fileset.toSource {
  root = ../.;
  fileset = lib.fileset.unions [
    ../Cargo.toml
    ../Cargo.lock
    ../config.example.toml
    ../LICENSE
    ../README.md
    (lib.fileset.fileFilter (file: file.hasExt "rs") ../src)
  ];
}
