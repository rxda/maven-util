{
  description = "Rust Stable: Musl + Windows (Standard GCC Linker)";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, fenix, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        # 定义 Windows 交叉编译包集
        mingwPkgs = pkgs.pkgsCross.mingwW64;

        # 1. 定义 Rust 工具链
        rustToolchain = fenix.packages.${system}.combine [
          fenix.packages.${system}.stable.toolchain
          fenix.packages.${system}.targets.x86_64-unknown-linux-musl.stable.rust-std
          fenix.packages.${system}.targets.x86_64-pc-windows-gnu.stable.rust-std
        ];

        # 2. 获取编译器
        muslCc = pkgs.pkgsStatic.stdenv.cc;
        mingwCc = mingwPkgs.stdenv.cc;

      in
      {
        devShells.default = pkgs.mkShell {
          name = "rust-std-env";

          packages = [
            rustToolchain
            pkgs.pkg-config
            muslCc
            mingwCc
          ];

          # --- Target: x86_64-unknown-linux-musl ---
          CC_x86_64_unknown_linux_musl = "${muslCc}/bin/${muslCc.targetPrefix}cc";
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = "${muslCc}/bin/${muslCc.targetPrefix}cc";

          # --- Target: x86_64-pc-windows-gnu ---
          CC_x86_64_pc_windows_gnu = "${mingwCc}/bin/${mingwCc.targetPrefix}cc";
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${mingwCc}/bin/${mingwCc.targetPrefix}cc";

          # 【关键修复：增加搜索路径】
          # 这里告诉 Rust 在链接时去哪里找 windows 版本的静态库 (pthreads, crt2.o 等)
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = builtins.concatStringsSep " " [
            "-L native=${mingwPkgs.windows.pthreads}/lib"
            "-L native=${mingwPkgs.windows.mcfgthreads}/lib" # 部分版本可能需要这个
            "-L native=${mingwPkgs.stdenv.cc.cc}/x86_64-w64-mingw32/lib" # 寻找 crt2.o
          ];

          shellHook = ''
            echo "🛡️  Rust Standard Environment"
            # 导出路径以便 build.rs 查找
            export CROSS_COMPILE="${mingwCc.targetPrefix}"
            export WINDOWS_PTHREAD_LIB="${mingwPkgs.windows.pthreads}/lib"
          '';
        };
      }
    );
}
