{
  description = "qr_cpp_compiler - GCC vs Clang vs Intel oneAPI on an equity-vol quant pipeline";

  # Tarball input (not github:) so restricted networks that block the GitHub
  # API but allow channels.nixos.org/releases.nixos.org still resolve it.
  inputs.nixpkgs.url = "https://channels.nixos.org/nixos-24.11/nixexprs.tar.xz";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      # scipy is only used by the data generator (exact erf for ground-truth
      # pricing), never by timed code.
      python = pkgs.python312.withPackages (ps: with ps; [ numpy nanobind scipy ]);
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          gcc13
          clang_18
          cmake
          ninja
          python
          git
        ];

        # Intel oneAPI (icpx) is not packaged in nixpkgs
        # (https://github.com/NixOS/nixpkgs/issues/367722). It is installed
        # outside nix via scripts/install_oneapi.sh; pick it up if present.
        shellHook = ''
          if [ -f /opt/intel/oneapi/setvars.sh ]; then
            set +u
            source /opt/intel/oneapi/setvars.sh --force >/dev/null 2>&1
            set -u
          fi
          if command -v icpx >/dev/null 2>&1; then
            export QR_HAVE_ICPX=1
            echo "icpx available: $(icpx --version | head -1)"
          else
            echo "icpx NOT available - run scripts/install_oneapi.sh for the oneAPI leg"
          fi
          # scripts/build_all.sh derives QR_GCC_TOOLCHAIN / QR_LIBSTDCXX_DIR
          # from this shell's g++ so icpx links the same libstdc++.
        '';
      };
    };
}
