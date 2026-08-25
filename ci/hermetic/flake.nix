{
  # Hermetic validate base image, stage 2 (OPT-IN; the default validate path is
  # unchanged).
  #
  # WHY NIX AND NOT JUST AN OCI DIGEST. A registry digest pins the ARTIFACT: if
  # the registry loses the blob, the image is gone and a validate run from a
  # month ago cannot be reproduced. A flake.lock pins the INPUTS, so the image
  # can be rebuilt from source at that lock even after third-party upgrades. A
  # receipt should name BOTH -- the digest for what ran, the lock for how to
  # rebuild it. See ci/hermetic/README.md for what a month-old rebuild actually
  # depends on staying available; the honest answer is not "nothing".
  #
  # WHAT THE IMAGE PINS, and why each is here rather than inherited from the host:
  #   * the Rust toolchain. `rust-toolchain.toml` says `channel = "nightly"`,
  #     which is a MOVING target -- the single largest source of "it built
  #     differently today". Pinned here to an exact dated nightly.
  #   * the C/C++ toolchain and native development libraries. Measured on this
  #     project: host `gcc` defaulting to `-march=x86-64-v2` put SSE4.1 into a
  #     static glibc that the emulator did not advertise, and a missing
  #     `libunwind-ptrace` broke a pinned build that had the compiler right.
  #   * every system executable a manifest runs AS A HERMIT GUEST. These are not
  #     build dependencies -- they are the program under test. A different
  #     `openssl` or `sqlite3` is a different guest.

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/b134951a4c9f3c995fd7be05f3243f8ecd65d798";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ (import rust-overlay) ];
      };

      # Exact dated nightly. Bumping this is a reviewed change to the lock and
      # the digest together, never a silent drift.
      rustToolchain = pkgs.rust-bin.nightly."2026-07-29".default.override {
        extensions = [ "rust-src" "rustfmt" "clippy" ];
      };

      # These are the exact versions used by the portable CI contract. The
      # pinned nixpkgs revision carries older releases (rust-script 0.34.0 and
      # cargo-nextest 0.9.72), so using those attributes directly would make an
      # offline run green under tools different from the ones CI selected.
      pinnedRustPlatform = pkgs.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };
      rustScriptVersion = "0.36.0";
      rustScript = pinnedRustPlatform.buildRustPackage {
        pname = "rust-script";
        version = rustScriptVersion;
        src = pkgs.fetchFromGitHub {
          owner = "fornwall";
          repo = "rust-script";
          rev = rustScriptVersion;
          hash = "sha256-Bb8ULD2MmZiSW/Tx5vAAHv95OMJ0EdWgR+NFhBkTlDU=";
        };
        cargoHash = "sha256-c1Ia3+WkbSsOAgrTh+OY0eYDD72Z7qmIir9RehUB7GU=";
        doCheck = false;
      };
      cargoNextestVersion = "0.9.100";
      cargoNextest = pinnedRustPlatform.buildRustPackage {
        pname = "cargo-nextest";
        version = cargoNextestVersion;
        src = pkgs.fetchFromGitHub {
          owner = "nextest-rs";
          repo = "nextest";
          rev = "cargo-nextest-${cargoNextestVersion}";
          hash = "sha256-MbgX/n6TC5hz66gvRAc7A0xFWbF2Ec68gMxCgPFpeoQ=";
        };
        cargoHash = "sha256-QXrsLNpxN9pg2OGYnYydeb8WjtFeIAtto3hPU9XGWXU=";
        cargoBuildFlags = [ "-p" "cargo-nextest" ];
        cargoTestFlags = [ "-p" "cargo-nextest" ];
        # The pinned nixpkgs cargo-auditable predates Rust edition 2024.
        auditable = false;
      };

      # Executables that the selected portable population runs as hermit guests.
      # This was re-audited mechanically from ci/expected-e2e-plan.json, each
      # selected manifest entry's requirements/program, and commands invoked by
      # shell fixtures. The second line supplies the previously missing lua, m4,
      # node, ssh-keygen, ruby, tclsh, uuidgen, mcookie, hexdump and ps.
      guestTools = with pkgs; [
        bash coreutils diffutils findutils gnugrep gnused gawk
        openssl zstd gnutar gzip xz jq sqlite git perl python3 redis
        lua5_4 gnum4 nodejs openssh ruby tcl util-linux procps
      ];

      # Toolchain and native libraries needed to build Hermit and to compile the
      # project's C fixtures INSIDE the root with the pinned compiler.
      buildTools = with pkgs; [
        rustToolchain gcc binutils gnumake cmake pkg-config
        libunwind elfutils zlib openssl.dev rustScript cargoNextest
      ];
    in
    {
      packages.${system} = {
        image = pkgs.dockerTools.buildLayeredImage {
          name = "hermit-hermetic-validate";
          tag = "nix";
          # Fixed timestamp: a build whose output moves with the wall clock
          # cannot be checked for reproducibility.
          created = "1970-01-01T00:00:01Z";
          contents = guestTools ++ buildTools ++ [
            pkgs.dockerTools.binSh
            pkgs.dockerTools.usrBinEnv
            pkgs.dockerTools.caCertificates
          ];
          # A nix-built root is minimal: it has no FHS scratch directories at
          # all. Measured -- without this, `check-detcore-backend-abstraction.sh`
          # failed at `mktemp: failed to create directory via template
          # '/tmp/tmp.XXXXXXXXXX': No such file or directory`, and it failed in
          # the NEGATIVE CONTROL, so the lint reported itself untrustworthy
          # rather than passing vacuously. That is the good failure mode, but the
          # directories have to exist. Sticky-bit 1777 as on a normal system.
          extraCommands = ''
            mkdir -p tmp var/tmp usr/bin
            chmod 1777 tmp var/tmp

            # Ten selected portable cells name these FHS paths literally. Nix
            # places their providers in /bin, while usrBinEnv creates only
            # /usr/bin/env; add exactly the audited compatibility paths.
            for command in bash date du find python3 sort tr; do
              ln -s "/bin/$command" "usr/bin/$command"
            done
          '';
          config = {
            Env = [
              "PATH=/bin:/usr/bin"
              "HERMIT_RUST_SCRIPT_VERSION=${rustScriptVersion}"
              "HERMIT_CARGO_NEXTEST_VERSION=${cargoNextestVersion}"
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              # The run is offline by construction; make a stray fetch fail loudly
              # rather than silently reach a network that a rebuild will not have.
              "CARGO_NET_OFFLINE=true"
            ];
            WorkingDir = "/src";
          };
        };

        # Convenience outputs, so version bumps can be inspected without
        # building the whole image.
        toolchain = rustToolchain;
        rust-script = rustScript;
        cargo-nextest = cargoNextest;
      };
    };
}
