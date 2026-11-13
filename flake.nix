{
  description = "Proxy d3d9.dll that draws WoW 3.3.5a text from vector outlines";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    nixpkgs,
    crane,
    fenix,
    ...
  }: let
    system = "x86_64-linux";
    pkgs = nixpkgs.legacyPackages.${system};
    inherit (pkgs) lib;
    rust = fenix.packages.${system};

    # Wow.exe is 32-bit, so is the DLL. Stable rustc with the target's
    # prebuilt std; nixpkgs' mingw-w64 gcc drives the link.
    target = "i686-pc-windows-gnu";
    mingw = pkgs.pkgsCross.mingw32.stdenv.cc;
    objdump = "${mingw.bintools.bintools}/bin/${mingw.targetPrefix}objdump";
    # nixpkgs builds mingw gcc with mcfgthreads, whose libgcc_eh would pull
    # in mcfgthread; the libraries Rust ships for the target (rust-mingw:
    # libgcc_eh, static winpthreads) come first on the search path instead.
    selfContained = "${rust.targets.${target}.stable.rust-mingw}/lib/rustlib/${target}/lib/self-contained";
    toolchain = rust.combine [
      rust.stable.cargo
      rust.stable.rustc
      rust.stable.rust-std
      rust.targets.${target}.stable.rust-std
    ];
    craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
    # clippy and rustfmt for the checks; the DLL's toolchain stays as it is.
    lintToolchain = rust.combine [toolchain rust.stable.clippy rust.stable.rustfmt];
    craneLint = craneLib.overrideToolchain lintToolchain;

    # The target's alone, so that a host build in the dev shell (the tests:
    # cargo test-host) links against the host's libgcc, not rust-mingw's.
    cross = {
      CARGO_BUILD_TARGET = target;
      CARGO_TARGET_I686_PC_WINDOWS_GNU_RUSTFLAGS = "-L native=${selfContained}";
      CARGO_TARGET_I686_PC_WINDOWS_GNU_LINKER = "${mingw}/bin/${mingw.targetPrefix}cc";
    };

    common =
      cross
      // {
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./build.rs
            ./src
            ./shaders/decoder.ps3
            # the source for d3dcompiler_47 and a probe (draw.rs's diagnose)
            ./shaders/decoder.hlsl
            ./shaders/bench_uv.ps3
          ];
        };
        strictDeps = true;
        doCheck = false;
        cargoExtraArgs = "--lib";
        nativeBuildInputs = [mingw];
      };

    deps = craneLib.buildDepsOnly common;

    dll = craneLib.buildPackage (common
      // {
        cargoArtifacts = deps;
        installPhaseCommand = ''
          install -Dm644 target/${target}/release/d3d9.dll $out/d3d9.dll
        '';
      });

    # The bench (examples/bench.rs) is a 64-bit Windows exe that drives the
    # decoder through D3D9 under Wine; benchApp runs it.
    benchExe = let
      t = "x86_64-pc-windows-gnu";
      cc = pkgs.pkgsCross.mingwW64.stdenv.cc;
      args =
        common
        // {
          CARGO_BUILD_TARGET = t;
          # nixpkgs' mingwW64 defaults to ucrt, Rust's libraries to msvcrt: the
          # whole runtime (crt2.o too) from rust-mingw.
          RUSTFLAGS = "-C link-self-contained=yes";
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${cc}/bin/${cc.targetPrefix}cc";
          nativeBuildInputs = [cc];
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./build.rs
              ./src
              ./examples
              ./shaders
            ];
          };
          pname = "wow-font-d3d9-bench";
          cargoExtraArgs = "--example bench";
        };
    in
      (craneLib.overrideToolchain (rust.combine [toolchain rust.targets.${t}.stable.rust-std rust.targets.${t}.stable.rust-mingw])).buildPackage (args
        // {
          cargoArtifacts = null;
          installPhaseCommand = ''
            install -Dm755 target/${t}/release/examples/bench.exe $out/bench.exe
          '';
        });

    # The DLL's code as Windows builds it, warnings as errors (the allowed
    # lints are in Cargo.toml).
    clippy = craneLint.cargoClippy (common
      // {
        cargoArtifacts = deps;
        cargoClippyExtraArgs = "-- -D warnings";
      });

    # The encoder's and the cache records' invariants, on the host.
    testCrate = {
      pname = "wow-font-d3d9-tests";
      version = "0.0.0";
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./tests
          ./src/atlas.rs
          ./src/glyph.rs
          ./src/coverage.rs
          ./src/union.rs
          ./src/record.rs
          ./src/ports.rs
          ./shaders/decoder.ps3
        ];
      };
      strictDeps = true;
      cargoArtifacts = null;
      cargoVendorDir = craneLib.vendorCargoDeps {cargoLock = ./tests/Cargo.lock;};
      cargoExtraArgs = "--manifest-path tests/Cargo.toml";
    };
    # By nextest, as `cargo test-host` in the dev shell: a process a test,
    # each with its time. Here every failure is shown, not the first alone.
    tests = craneLib.cargoNextest (testCrate // {cargoNextestExtraArgs = "--no-fail-fast";});
    clippyTests = craneLint.cargoClippy (testCrate // {cargoClippyExtraArgs = "--all-targets -- -D warnings";});

    # Every .rs as rustfmt has it: the DLL, the bench, build.rs, the tests.
    fmt = pkgs.runCommand "rustfmt" {nativeBuildInputs = [lintToolchain];} ''
      cd ${lib.fileset.toSource {
        root = ./.;
        fileset =
          lib.fileset.intersection
          (lib.fileset.unions [./build.rs ./src ./tests ./examples])
          (lib.fileset.fileFilter (f: f.hasExt "rs") ./.);
      }}
      rustfmt --check --edition 2024 $(find . -name '*.rs' | LC_ALL=C sort)
      touch $out
    '';

    # The shaders as committed: name, profile, extension.
    shaderList = "decoder:ps_3_0:ps3 bench:vs_3_0:vs3 bench_uv:ps_3_0:ps3";

    # The committed bytecode is what the HLSL compiles to (the DLL embeds
    # shaders/decoder.ps3; the bench takes the others).
    shaders = pkgs.runCommand "shaders-match" {nativeBuildInputs = [pkgs.vkd3d];} ''
      cp -r ${./shaders} shaders
      chmod -R u+w shaders
      cd shaders
      for s in ${shaderList}; do
        IFS=: read -r name profile ext <<< "$s"
        vkd3d-compiler -x hlsl -p "$profile" -b d3dbc -o "$TMPDIR/$name.$ext" "$name.hlsl"
        cmp "$name.$ext" "$TMPDIR/$name.$ext" || { echo "shaders/$name.$ext is stale (nix run .#shaders)"; exit 1; }
      done
      touch $out
    '';

    # Writes shaders/*.ps3 of the checkout it runs in.
    shadersApp = pkgs.writeShellApplication {
      name = "wow-font-shaders";
      runtimeInputs = [pkgs.vkd3d];
      text = ''
        [ -f shaders/decoder.hlsl ] || { echo "run from the repository's root" >&2; exit 1; }
        for s in ${shaderList}; do
          IFS=: read -r name profile ext <<< "$s"
          vkd3d-compiler -x hlsl -p "$profile" -b d3dbc -o "shaders/$name.$ext" "shaders/$name.hlsl"
          echo "shaders/$name.$ext: $(stat -c %s "shaders/$name.$ext") bytes"
        done
      '';
    };

    # What each pixel shader compiles to, without a GPU: instructions,
    # fetches, flow control, temporaries (vkd3d's limit for ps_3_0 is 32)
    # and size, for shaders/decoder.hlsl, or for the
    # .hlsl/.ps3 files named. --dump prints the disassembly too. Static
    # counts: a loop's body counts once, and the decoder waits on fetches
    # more than on arithmetic, so fewer instructions need not mean faster;
    # the bench says that.
    asmApp = pkgs.writeShellApplication {
      name = "wow-font-asm";
      runtimeInputs = [pkgs.vkd3d pkgs.gawk];
      text = ''
        [ -f shaders/decoder.hlsl ] || { echo "run from the repository's root" >&2; exit 1; }
        dump=0
        files=()
        for a in "$@"; do
          case "$a" in
            --dump) dump=1 ;;
            *) files+=("$a") ;;
          esac
        done
        [ ''${#files[@]} -gt 0 ] || files=(shaders/decoder.hlsl)
        tmp=$(mktemp -d)
        trap 'rm -rf "$tmp"' EXIT
        printf '%-28s %6s %6s %6s %5s %6s\n' shader instr fetch flow temps bytes
        for f in "''${files[@]}"; do
          name=$(basename "$f")
          case "$f" in
            *.hlsl)
              # vkd3d resolves #include from the working directory.
              (cd "$(dirname "$f")" && vkd3d-compiler -x hlsl -p ps_3_0 -b d3dbc -o "$tmp/$name.ps3" "$(basename "$f")") || continue
              bc="$tmp/$name.ps3"
              ;;
            *) bc="$f" ;;
          esac
          vkd3d-compiler -x d3dbc -b d3d-asm -o "$tmp/$name.asm" "$bc"
          awk -v name="$name" -v bytes="$(stat -c %s "$bc")" '
            { sub(/^[ \t]+/, "") }
            /^$|^ps_|^vs_|^dcl|^def/ { next }
            { n++ }
            /^texld/ { t++ }
            /^(if|else|endif|loop|endloop|rep|endrep|break|call|ret|label)/ { c++ }
            { while (match($0, /r[0-9]+/)) { r = substr($0, RSTART + 1, RLENGTH - 1) + 1; if (r > m) m = r; $0 = substr($0, RSTART + RLENGTH) } }
            END { printf "%-28s %6d %6d %6d %5d %6d\n", name, n, t, c, m, bytes }
          ' "$tmp/$name.asm"
          [ "$dump" = 1 ] && cat "$tmp/$name.asm"
        done
      '';
    };

    # The bench under Wine with DXVK, so a shader takes the same road as in
    # the game: shaders/decoder.ps3 first, then each --ps NAME=FILE. --lavapipe runs it on Mesa's CPU
    # Vulkan in a virtual X server: no GPU or display needed, times only
    # rough. Everything else goes to bench.exe (examples/bench.rs). Output
    # in ~/.cache/wow-font-d3d9/bench unless --out says otherwise.
    benchApp = pkgs.writeShellApplication {
      name = "wow-font-bench";
      runtimeInputs = [pkgs.wineWow64Packages.stable pkgs.xvfb-run];
      text = ''
        cache="''${XDG_CACHE_HOME:-$HOME/.cache}/wow-font-d3d9"
        mkdir -p "$cache/bench-run"
        lavapipe=0
        args=(--out "$cache/bench" --ps "decoder=${./shaders/decoder.ps3}")
        while [ $# -gt 0 ]; do
          case "$1" in
            --lavapipe) lavapipe=1 ;;
            *) args+=("$1") ;;
          esac
          shift
        done
        # The exe next to DXVK's d3d9.dll, which Wine then loads before its own.
        run="$cache/bench-run"
        install -m755 ${benchExe}/bench.exe "$run/bench.exe"
        install -m644 ${pkgs.dxvk.bin}/x64/d3d9.dll "$run/d3d9.dll"
        export WINEPREFIX="$cache/wine" WINEDEBUG=-all WINEDLLOVERRIDES=d3d9=n
        export DXVK_LOG_LEVEL=warn DXVK_LOG_PATH=none
        if [ "$lavapipe" = 1 ]; then
          VK_ICD_FILENAMES=${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.x86_64.json \
            xvfb-run -a wine "$run/bench.exe" "''${args[@]}"
        else
          wine "$run/bench.exe" "''${args[@]}"
        fi
      '';
    };

    # The bench as a test, for CI: the committed decoder on lavapipe against
    # the exact reference, Roboto with and without an outline, failing when
    # a scale has more texels off by over 16/255 than its limit. Once in
    # white and once in black, the two ends of the alpha's tone. The decoder
    # does 0.82, 0, 0.000 and 0.001 % now (Roboto's contours overlap, so the
    # union is under test too; at 0.5x an outline's alpha near 1 is steep in
    # its distance, which six chords have to a hundredth of a texel): the
    # limits leave a third at 0.5x and tens of texels elsewhere, for a newer
    # Mesa.
    gateApp = pkgs.writeShellApplication {
      name = "wow-font-gate";
      text = let
        font = f: px: outline: "--font 'Z:${pkgs.roboto}/share/fonts/truetype/${f}.ttf:${toString px}:${toString outline}'";
      in ''
        for color in ffffff 000000; do
          ${benchApp}/bin/wow-font-bench --lavapipe --no-time \
            --scales 0.5,1,2,4 --color "$color" \
            ${font "Roboto-Regular" 32 1} \
            ${font "Roboto-Regular" 14 0} \
            ${font "Roboto-Bold" 20 1} \
            --max-gross 0.5:1.1,1:0.02,2:0.02,4:0.02 \
            "$@"
        done
      '';
    };

    # What the DLL imports and exports, exactly: a runtime DLL creeping in
    # (mcfgthread, winpthread, libgcc) would make it fail to load on a
    # machine without one.
    pe = pkgs.runCommand "dll-imports-exports" {} ''
      dump=$(${objdump} -p ${dll}/d3d9.dll)
      imports=$(grep 'DLL Name' <<< "$dump" | awk '{print $3}' | LC_ALL=C sort | xargs)
      want="KERNEL32.dll USERENV.dll WS2_32.dll api-ms-win-core-synch-l1-2-0.dll bcryptprimitives.dll msvcrt.dll ntdll.dll"
      [ "$imports" = "$want" ] || { echo "imports: $imports"; echo "want:    $want"; exit 1; }
      exports=$(awk '/\+base\[/ && $NF != "RVA" {print $NF}' <<< "$dump" | LC_ALL=C sort | xargs)
      want="D3DPERF_BeginEvent D3DPERF_EndEvent D3DPERF_GetStatus D3DPERF_QueryRepeatFrame D3DPERF_SetMarker D3DPERF_SetOptions D3DPERF_SetRegion Direct3DCreate9 Direct3DCreate9Ex Direct3DShaderValidatorCreate9 DllMain"
      [ "$exports" = "$want" ] || { echo "exports: $exports"; echo "want:    $want"; exit 1; }
      touch $out
    '';

    # What a GitHub release carries: the DLL, a sample ini, README and
    # LICENSE, in a zip whose bytes depend on nothing else (fixed times,
    # sorted, no extra fields), so it can be checked against a nix build.
    inherit (craneLib.crateNameFromCargoToml {cargoToml = ./Cargo.toml;}) version;
    release = pkgs.runCommand "wow-font-d3d9-${version}.zip" {nativeBuildInputs = [pkgs.zip];} ''
      d=wow-font-d3d9-${version}
      mkdir $d
      cp ${dll}/d3d9.dll $d/
      cp ${./README.md} $d/README.md
      cp ${./LICENSE} $d/LICENSE
      sed 's/$/\r/' ${./dist/wow-font-d3d9.ini} > $d/wow-font-d3d9.ini
      find $d -exec touch -h -d @315532800 {} +
      find $d | LC_ALL=C sort | zip -X -q -@ $out
    '';
  in {
    packages.${system} = {
      default = dll;
      bench = benchExe;
      inherit release;
    };

    apps.${system} = {
      bench = {
        type = "app";
        program = "${benchApp}/bin/wow-font-bench";
        meta.description = "The decoder against an exact reference, under Wine";
      };
      gate = {
        type = "app";
        program = "${gateApp}/bin/wow-font-gate";
        meta.description = "The bench as a test: the decoder within its error limits on lavapipe";
      };
      shaders = {
        type = "app";
        program = "${shadersApp}/bin/wow-font-shaders";
        meta.description = "Compile shaders/*.hlsl into the committed bytecode";
      };
      asm = {
        type = "app";
        program = "${asmApp}/bin/wow-font-asm";
        meta.description = "Instructions, fetches and registers of each pixel shader";
      };
    };

    checks.${system} = {
      inherit dll tests shaders pe release clippy fmt;
      clippy-tests = clippyTests;
    };

    devShells.${system}.default = pkgs.mkShell (cross
      // {
        packages = [
          (rust.combine [
            toolchain
            rust.stable.clippy
            rust.stable.rustfmt
            rust.stable.rust-src
            rust.stable.rust-analyzer
          ])
          mingw
          pkgs.vkd3d
          pkgs.cargo-nextest
        ];
      });
  };
}
