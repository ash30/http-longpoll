let pkgs = import (builtins.fetchTarball {
  name = "nixpkgs-unstable";
  url = "https://github.com/nixos/nixpkgs/archive/035f8c0853c2977b24ffc4d0a42c74f00b182cd8.tar.gz";
  # Hash obtained using `nix-prefetch-url --unpack <url>`
  sha256 = "10mkjpj3wigr6w5azrq0nf784kncf6pplm075ndniakhbwkwjwb2";
}) {
  config.allowUnfree = true; 
  overlays = [ 
     # https://github.com/oxalica/rust-overlay/commit/0bf05d8534406776a0fbc9ed8d4ef5bd925b056a
     # Why does this break?
    (import (fetchTarball "https://github.com/oxalica/rust-overlay/archive/2e7ccf572ce0f0547d4cf4426de4482936882d0e.tar.gz"))
  ];
};
  rustc = pkgs.rust-bin.nightly.latest.default.override { extensions = ["rust-src"];};
  cargo = pkgs.rust-bin.nightly.latest.default;
  rustPlatform = pkgs.makeRustPlatform {
    rustc = rustc;
    cargo = cargo;
  };
in
pkgs.mkShell {
  nativeBuildInputs = [
    rustc
    cargo
  ];
  buildInputs = [
    pkgs.rust-bin.stable.latest.rust-analyzer # LSP Server
    pkgs.rust-bin.stable.latest.rustfmt       # Formatter
    pkgs.rust-bin.stable.latest.clippy        # Linter
  ];
  RUST_SRC_PATH = "${rustc}/lib/rustlib/src/rust/library/";
}


