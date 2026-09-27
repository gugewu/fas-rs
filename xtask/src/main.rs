// Copyright 2025-2025, dependabot[bot], shadow3aaa
//
// This file is part of fas-rs.
//
// fas-rs is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option)
// any later version.
//
// fas-rs is distributed in the hope that it will be useful, but WITHOUT ANY
// WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
// FOR A PARTICULAR PURPOSE. See the GNU General Public License for more
// details.
//
// You should have received a copy of the GNU General Public License along
// with fas-rs. If not, see <https://www.gnu.org/licenses/>.

mod zip_ext;

use std::{
    fs::{self},
    path::{Path, PathBuf},
    process::{self, Command},
};

use anyhow::{Context, Result};
use cargo_metadata::MetadataCommand;
use clap::{Parser, Subcommand};
use fs_extra::{dir, file};
use zip::{CompressionMethod, write::FileOptions};

use zip_ext::zip_create_from_directory_with_options;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Check the build of fas-rs
    Check {
        /// Build in release mode (default: false)
        #[clap(short, long, default_value = "false")]
        release: bool,

        /// Print detailed output (default: false)
        #[clap(short, long, default_value = "false")]
        verbose: bool,
    },

    /// Build fas-rs
    Build {
        /// Build in release mode (default: false)
        #[clap(short, long, default_value = "false")]
        release: bool,

        /// Print detailed output (default: false)
        #[clap(short, long, default_value = "false")]
        verbose: bool,
    },

    /// Clean build artifacts
    Clean,

    /// Format source code
    Format {
        /// Print detailed output (default: false)
        #[clap(short, long, default_value = "false")]
        verbose: bool,
    },

    /// Run the Clippy linter
    Lint {
        /// Automatically fix lint issues (default: false)
        #[clap(short, long, default_value = "false")]
        fix: bool,
    },

    /// Update project dependencies
    Update,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let Some(command) = cli.command else {
        eprintln!("No command specified. Use --help to see available commands.");
        process::exit(1);
    };

    match command {
        Commands::Check { release, verbose } => {
            check(release, verbose)?;
        }
        Commands::Build { release, verbose } => {
            build(release, verbose)?;
        }
        Commands::Clean => {
            clean()?;
        }
        Commands::Format { verbose } => {
            format(verbose)?;
        }
        Commands::Lint { fix } => {
            lint(fix)?;
        }
        Commands::Update => {
            update()?;
        }
    }

    Ok(())
}

/// 构建 eBPF 程序并返回生成的对象文件路径。
///
/// 该函数会：
/// 1. 通过 `cargo metadata` 查找 `frame-analyzer-ebpf` 包；
/// 2. 使用 `bpfel-unknown-none` 目标构建它；
/// 3. 返回生成的对象文件路径，供 `FRAME_ANALYZER_EBPF_PATH` 使用。
fn build_ebpf() -> Result<PathBuf> {
    let metadata = MetadataCommand::new()
        .exec()
        .context("执行 cargo metadata 失败")?;

    // 优先查找名叫 frame-analyzer-ebpf 的包
    // 如果实际包名不同，请在这里调整（可用 `cargo metadata | jq` 查看包名）
    let ebpf_pkg = metadata
        .packages
        .iter()
        .find(|p| p.name == "frame-analyzer-ebpf")
        .context("在工作区中找不到 `frame-analyzer-ebpf` 包")?;

    let manifest_path = ebpf_pkg.manifest_path.as_std_path().to_path_buf();

    // 使用与主构建相同的 target 目录，便于缓存共享
    let target_dir = Path::new("target");

    println!(
        "Building eBPF: {} (manifest: {:?})",
        ebpf_pkg.name, manifest_path
    );

    let status = Command::new("cargo")
        .args([
            "build",
            "--manifest-path",
            manifest_path.to_str().unwrap(),
            "--target",
            "bpfel-unknown-none",
            "--release",
            "--target-dir",
            target_dir.to_str().unwrap(),
        ])
        .status()
        .context("执行 cargo build (eBPF) 失败")?;

    if !status.success() {
        anyhow::bail!("构建 eBPF 程序失败，退出码: {:?}", status.code());
    }

    let ebpf_path = target_dir
        .join("bpfel-unknown-none")
        .join("release")
        .join("frame-analyzer-ebpf");

    if !ebpf_path.exists() {
        anyhow::bail!("eBPF 对象文件不存在: {:?}", ebpf_path);
    }

    println!("eBPF object: {:?}", ebpf_path);

    Ok(ebpf_path)
}

fn build(release: bool, verbose: bool) -> Result<()> {
    // 1. 先构建 eBPF 程序（必须）
    let ebpf_path = build_ebpf()?;

    // 2. 准备临时打包目录
    let temp_dir = temp_dir(release);
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir)?;

    // 3. 构建 Android 目标，并注入 FRAME_ANALYZER_EBPF_PATH
    let mut cargo = cargo_ndk();
    cargo.env("FRAME_ANALYZER_EBPF_PATH", &ebpf_path);
    cargo.args([
        "build",
        "--target",
        "aarch64-linux-android",
        "-Z",
        "build-std",
        "-Z",
        "trim-paths",
    ]);

    if release {
        cargo.arg("--release");
    }
    if verbose {
        cargo.arg("--verbose");
    }

    let status = cargo.spawn().context("启动 cargo ndk build 失败")?.wait()?;
    if !status.success() {
        anyhow::bail!("Android 目标构建失败，退出码: {:?}", status.code());
    }

    // 4. 打包 module 目录
    let module_dir = module_dir();
    dir::copy(
        &module_dir,
        &temp_dir,
        &dir::CopyOptions::new().overwrite(true).content_only(true),
    )?;
    let _ = fs::remove_file(temp_dir.join(".gitignore"));
    file::copy(
        bin_path(release),
        temp_dir.join("fas-rs"),
        &file::CopyOptions::new().overwrite(true),
    )?;

    // 5. 构建 webui 并复制产物
    build_webui()?;
    dir::copy(
        webroot_dir(),
        &temp_dir,
        &dir::CopyOptions::new().overwrite(true),
    )?;

    // 6. 打包 zip
    let build_type = if release { "release" } else { "debug" };
    let package_path = Path::new("output").join(format!("fas-rs({build_type}).zip"));

    let options: FileOptions<'_, ()> = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(9));
    zip_create_from_directory_with_options(&package_path, &temp_dir, |_| options)?;

    println!("fas-rs built successfully: {:?}", package_path);

    Ok(())
}

fn check(release: bool, verbose: bool) -> Result<()> {
    // check 也需要 eBPF 路径，因为 frame-analyzer 是编译期依赖
    let ebpf_path = build_ebpf()?;

    let mut cargo = cargo_ndk();
    cargo.env("FRAME_ANALYZER_EBPF_PATH", &ebpf_path);
    cargo.args([
        "check",
        "--target",
        "aarch64-linux-android",
        "-Z",
        "build-std",
        "-Z",
        "trim-paths",
    ]);
    cargo.env("RUSTFLAGS", "-C default-linker-libraries");

    if release {
        cargo.arg("--release");
    }
    if verbose {
        cargo.arg("--verbose");
    }

    let status = cargo.spawn().context("启动 cargo ndk check 失败")?.wait()?;
    if !status.success() {
        anyhow::bail!("cargo check 失败，退出码: {:?}", status.code());
    }

    Ok(())
}

fn clean() -> Result<()> {
    let temp_dir = temp_dir(false);
    let _ = fs::remove_dir_all(&temp_dir);

    let status = Command::new("cargo").arg("clean").spawn()?.wait()?;
    if !status.success() {
        anyhow::bail!("cargo clean 失败");
    }

    Ok(())
}

fn format(verbose: bool) -> Result<()> {
    let mut command = Command::new("cargo");
    command.args(["fmt", "--all"]);
    if verbose {
        command.arg("--verbose");
    }
    let status = command.spawn()?.wait()?;
    if !status.success() {
        anyhow::bail!("cargo fmt 失败");
    }
    Ok(())
}

fn lint(fix: bool) -> Result<()> {
    // clippy 也会编译 frame-analyzer，所以同样需要 eBPF 路径
    let ebpf_path = build_ebpf()?;

    let command_builder = |fix: bool| {
        let mut command = cargo_ndk();
        command.env("FRAME_ANALYZER_EBPF_PATH", &ebpf_path);
        command.arg("clippy");
        if fix {
            command.args(["--fix", "--allow-dirty", "--allow-staged", "--all"]);
        }
        command.args(["--target", "aarch64-linux-android"]);
        command
    };

    let status = command_builder(fix).spawn()?.wait()?;
    if !status.success() {
        anyhow::bail!("cargo clippy 失败");
    }

    let status = command_builder(fix).arg("--release").spawn()?.wait()?;
    if !status.success() {
        anyhow::bail!("cargo clippy (release) 失败");
    }

    Ok(())
}

fn update() -> Result<()> {
    let status = Command::new("cargo")
        .args(["update", "--recursive"])
        .spawn()?
        .wait()?;
    if !status.success() {
        anyhow::bail!("cargo update 失败");
    }

    let status = Command::new("cargo")
        .current_dir("xtask")
        .args(["update", "--recursive"])
        .spawn()?
        .wait()?;
    if !status.success() {
        anyhow::bail!("xtask cargo update 失败");
    }

    Ok(())
}

fn module_dir() -> PathBuf {
    Path::new("module").to_path_buf()
}

fn temp_dir(release: bool) -> PathBuf {
    Path::new("output")
        .join(".temp")
        .join(if release { "release" } else { "debug" })
}

fn bin_path(release: bool) -> PathBuf {
    Path::new("target")
        .join("aarch64-linux-android")
        .join(if release { "release" } else { "debug" })
        .join("fas-rs")
}

fn cargo_ndk() -> Command {
    let mut command = Command::new("cargo");
    command
        .args(["+nightly", "ndk", "--platform", "31", "-t", "arm64-v8a"])
        .env("RUSTFLAGS", "-C default-linker-libraries")
        .env("CARGO_CFG_BPF_TARGET_ARCH", "aarch64");
    command
}

fn webroot_dir() -> PathBuf {
    Path::new("webui").join("webroot")
}

fn build_webui() -> Result<()> {
    let npm = || {
        let mut command = Command::new("npm");
        command.current_dir("webui");
        command
    };

    let status = npm().arg("install").spawn()?.wait()?;
    if !status.success() {
        anyhow::bail!("npm install 失败");
    }

    let status = npm().args(["run", "build"]).spawn()?.wait()?;
    if !status.success() {
        anyhow::bail!("npm run build 失败");
    }

    Ok(())
}
