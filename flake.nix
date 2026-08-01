{
  description = "qr_cpp_compiler - GCC vs Clang vs Intel oneAPI on an equity-vol quant pipeline";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      python = pkgs.python312.withPackages (ps: with ps; [ numpy nanobind ]);
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
          # icpx builds link against this gcc's libstdc++ so all variants
          # share one C++ runtime (see CMakeLists QR_GCC_TOOLCHAIN).
          export QR_GCC_TOOLCHAIN="$(dirname $(dirname $(command -v g++)))"
        '';
      };
    };
}
