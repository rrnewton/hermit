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

      # Executables that manifests run as a hermit GUEST.
      #
      # HOW THIS LIST IS DERIVED, and it is mechanical rather than recalled.
      # Take every manifest cell with `ci: true`, take its `program:` entry, and
      # for the shell fixtures among them collect what they `command -v` or
      # `exec`. That yields 22 distinct executables across 47 fixtures, which
      # map onto the attributes below.
      #
      # The first version of this list was assembled by reading and was
      # INCOMPLETE: it omitted lua, m4, node, ruby, ssh-keygen, tclsh, uuidgen,
      # mcookie and hexdump -- nine executables belonging to seven attributes,
      # every one of them reached by a `ci: true` cell. `tclsh` in particular
      # was missed by a careful manual pass and found only by the mechanical
      # one, which is the argument for deriving rather than recalling.
      #
      # Those cells fail LOUDLY when a tool is absent rather than skipping --
      # `uuidgen-random.sh --prepare` runs `command -v uuidgen`, and
      # `lua-random.sh` has an explicit error path naming both candidates it
      # tried -- so an omission here costs a red cell, never a green one that
      # exercised nothing. That is the right direction, and it is also why the
      # omission was survivable rather than silent.
      guestTools = with pkgs; [
        bash coreutils diffutils findutils gnugrep gnused gawk
        openssl zstd gnutar gzip xz jq sqlite git perl python3 redis
        # Added after the mechanical derivation above; see the note.
        lua5_4 m4 nodejs openssh ruby tcl util-linux
      ];

      # Toolchain and native libraries needed to build Hermit and to compile the
      # project's C fixtures INSIDE the root with the pinned compiler.
      # A NIX IMAGE GIVES YOU PACKAGES AT STORE PATHS AND NO FHS SEARCH PATHS.
      # That single fact caused three separate build failures here, each hidden
      # until the one before it was fixed, and it is worth stating once:
      #
      #   1. nixpkgs splits these packages. The default output has
      #      `lib/libunwind-ptrace.so`; the `.pc` file and the headers live in
      #      `.dev`. With only the default output `ls /lib/libunwind*` looked
      #      right while `find / -name 'libunwind*.pc'` returned NOTHING.
      #   2. Adding `.dev` is necessary and NOT sufficient. The `.pc` files then
      #      exist, at their own store paths, and pkg-config's compiled-in search
      #      path contains only pkg-config's OWN store path -- so nothing finds
      #      them. `unwind-sys`'s build script panicked with "The system library
      #      `libunwind-ptrace` ... was not found". Hence PKG_CONFIG_PATH below.
      #   3. The C compiler has the same problem independently. With pkg-config
      #      fixed, reverie-sabre's vendored libelf failed on
      #      `fatal error: zlib.h: No such file or directory`, because nothing
      #      populates /usr/include either. Hence CPATH and LIBRARY_PATH below.
      #
      # `openssl.dev` was already listed and had defects 2 and 3 the whole time;
      # it simply was never exercised. Deriving all three paths from ONE list is
      # what stops the contents and the search paths drifting apart again.
      nativeLibs = with pkgs; [ libunwind elfutils zlib openssl ];

      # `rust-script` is a BUILD tool here, not a guest. Every entrypoint under
      # scripts/ and ci/ carries `#!/usr/bin/env -S rust-script --force` -- the
      # project's own script convention -- so without it the validate entrypoint
      # itself cannot execute inside the root, which was measured: `command -v
      # rust-script` returned nothing while python3, make, cargo and gcc all
      # resolved. Pinned by the same lock as everything else.
      buildTools = with pkgs; [
        rustToolchain gcc binutils gnumake cmake pkg-config rust-script
      ] ++ nativeLibs ++ map (p: p.dev) nativeLibs;
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
            mkdir -p tmp var/tmp
            chmod 1777 tmp var/tmp
          '';
          config = {
            Env = [
              # $CARGO_HOME/bin first: the fetch phase installs exactly-pinned
              # developer tools there (cargo-nextest at the version the DAG
              # names), and they must win over anything the image ships.
              "PATH=/build/.cargo/bin:/bin:/usr/bin"
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              # The run is offline by construction; make a stray fetch fail loudly
              # rather than silently reach a network that a rebuild will not have.
              "CARGO_NET_OFFLINE=true"
              # WITHOUT THIS, EVERY `.pc` FILE IN THE IMAGE IS UNREACHABLE.
              # A nix image does not populate /lib/pkgconfig; each package keeps
              # its own `lib/pkgconfig` under its store path, and pkg-config's
              # compiled-in search path contains only pkg-config's OWN store
              # path. Measured 2026-08-24 inside the built image:
              #   PKG_CONFIG_PATH=<unset>
              #   pkg-config --variable pc_path pkg-config
              #     -> /nix/store/...-pkg-config-0.29.2/lib/pkgconfig:.../share/pkgconfig
              # so `libunwind-ptrace.pc` was present at its store path and still
              # not findable, and the `unwind-sys` build script panicked. Note
              # that `openssl.dev` had the same defect and simply was not
              # exercised -- adding a `.dev` output is necessary and NOT
              # sufficient; it must also be on the search path.
              "PKG_CONFIG_PATH=${pkgs.lib.makeSearchPathOutput "dev" "lib/pkgconfig" nativeLibs}"
              "CPATH=${pkgs.lib.makeSearchPathOutput "dev" "include" nativeLibs}"
              "LIBRARY_PATH=${pkgs.lib.makeLibraryPath nativeLibs}"
            ];
            WorkingDir = "/src";
          };
        };

        # Convenience: the exact toolchain, so a bump can be inspected without
        # building the whole image.
        toolchain = rustToolchain;
      };
    };
}
