// Copyright 2025-2025, dependabot[bot], shadow3, shadow3aaa
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
/// 说明：`frame-analyzer-ebpf` 并不作为 Rust 依赖出现在 `fas-rs` 的依赖图中，
/// 它只是被 `frame-analyzer` 在编译期通过 `include_bytes_aligned!(env!("FRAME_ANALYZER_EBPF_PATH"))`
/// 嵌入。因此不能通过 `cargo metadata` 的 packages 列表查找。
///
/// 我们改为从 `frame-analyzer` 的 manifest 路径反推其 git checkout 的仓库根目录，
/// 再在该目录下寻找 eBPF 程序包。
fn build_ebpf() -> Result<PathBuf> {
    let metadata = MetadataCommand::new()
        .exec()
        .context("执行 cargo metadata 失败")?;

    // 1. 通过 frame-analyzer 定位 git checkout 的仓库根目录
    let frame_pkg = metadata
        .packages
        .iter()
        .find(|p| p.name == "frame-analyzer")
        .context("在依赖图中找不到 `frame-analyzer` 包")?;

    let frame_manifest = frame_pkg.manifest_path.as_std_path();
    // 期望路径: <checkout>/<rev>/frame-analyzer/Cargo.toml
    // 上两级即为 <checkout>/<rev>/
    let repo_root = frame_manifest
        .parent() // <rev>/frame-analyzer/
        .and_then(|p| p.parent()) // <rev>/
        .context("无法从 frame-analyzer 的 manifest 推断 eBPF 仓库根目录")?;

    println!("eBPF repo root: {:?}", repo_root);

    // 2. 尝试若干可能的 eBPF 包目录名
    //    如果实际目录名不在其中，日志里会打印 repo_root，
    //    你照着加一个候选名即可。
    let candidates = [
        "frame-analyzer-ebpf",
        "frame-analyzer-ebpf-programs",
        "ebpf",
        "frame-analyzer-ebpf-user",
    ];
    let mut ebpf_manifest: Option<PathBuf> = None;
    for name in &candidates {
        let manifest = repo_root.join(name).join("Cargo.toml");
        if manifest.exists() {
            ebpf_manifest = Some(manifest);
            break;
        }
    }
    let ebpf_manifest = ebpf_manifest.with_context(|| {
        format!(
            "在 {:?} 下找不到 eBPF 包（已尝试: {:?}）",
            repo_root, candidates
        )
    })?;

    println!("eBPF manifest: {:?}", ebpf_manifest);

    // 3. 编译 eBPF 程序
    //    - bpfel-unknown-none 是 Tier 3 目标，rustup 无预编译 core，
    //      必须通过 `-Z build-std=core` 从 rust-src 现场构建。
    //    - CI 中不要使用 `rustup target add bpfel-unknown-none`，会失败。
    let target_dir = Path::new("target");
    let status = Command::new("cargo")
        .args([
            "build",
            "--manifest-path",
            ebpf_manifest.to_str().unwrap(),
            "--target",
            "bpfel-unknown-none",
            "-Z",
            "build-std=core",
            "--release",
            "--target-dir",
            target_dir.to_str().unwrap(),
        ])
        .status()
        .context("执行 cargo build (eBPF) 失败")?;

    if !status.success() {
        anyhow::bail!("构建 eBPF 程序失败，退出码: {:?}", status.code());
    }

    // 4. 在 release 目录下查找产物（兼容自定义 bin 名）
    let release_dir = target_dir.join("bpfel-unknown-none").join("release");
    let ebpf_path = find_ebpf_object(&release_dir)?;

    println!("eBPF object: {:?}", ebpf_path);

    Ok(ebpf_path)
}

/// 在指定目录下查找 eBPF 产物（跳过 .d/.rlib/.rmeta/隐藏文件）。
fn find_ebpf_object(dir: &Path) -> Result<PathBuf> {
    if !dir.exists() {
        anyhow::bail!("eBPF 产物目录不存在: {:?}", dir);
    }
    let mut fallback: Option<PathBuf> = None;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.starts_with('.')
            || name.ends_with(".d")
            || name.ends_with(".rlib")
            || name.ends_with(".rmeta")
        {
            continue;
        }
        // 优先返回名字里包含 "ebpf" 或 "frame" 的产物
        if name.contains("ebpf") || name.contains("frame") {
            return Ok(path);
        }
        if fallback.is_none() {
            fallback = Some(path);
        }
    }
    fallback.with_context(|| format!("在 {:?} 下找不到 eBPF 对象文件", dir))
}

fn build(release: bool, verbose: bool) -> Result<()> {
    // 1. 先构建 eBPF 程序（frame-analyzer 编译期依赖）
    let ebpf_path = build_ebpf()?;

    // 2. 准备打包临时目录
    let temp_dir = temp_dir(release);
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir)?;

    // 3. 构建 Android 目标，注入 FRAME_ANALYZER_EBPF_PATH
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
    // check 同样需要 eBPF 路径（frame-analyzer 是编译期依赖）
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
    // clippy 也会编译 frame-analyzer，同样需要 eBPF 路径
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
