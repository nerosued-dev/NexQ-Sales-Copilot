fn main() {
    // Fix CRT linking: ort_sys and whisper_rs_sys are compiled with /MD (dynamic CRT)
    // but Rust uses /MT (static CRT). We need to swap static CRT for dynamic CRT.
    #[cfg(target_os = "windows")]
    {
        if let Err(error) = build_windows_test_manifest_resource() {
            panic!("failed to build the Windows test manifest resource: {error}");
        }

        // Remove static CRT, add dynamic CRT to resolve __imp_* symbols
        println!("cargo:rustc-link-arg=/NODEFAULTLIB:libucrt.lib");
        println!("cargo:rustc-link-arg=/DEFAULTLIB:ucrt.lib");
    }
    tauri_build::build()
}

#[cfg(target_os = "windows")]
fn build_windows_test_manifest_resource() -> Result<(), Box<dyn std::error::Error>> {
    use std::{env, fs, path::PathBuf, process::Command};

    const RESOURCE_SYMBOL: &str = "NEXQ_WINDOWS_TEST_MANIFEST_RESOURCE";

    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR must be set by Cargo")?,
    );
    let manifest = manifest_dir.join("windows-test-manifest.xml");
    if !manifest.is_file() {
        return Err(format!("test manifest not found at {}", manifest.display()).into());
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR must be set by Cargo")?);
    let rc_source = out_dir.join("windows-test-manifest.rc");
    let compiled_resource = out_dir.join("windows-test-manifest.res");
    let resource_object = out_dir.join("windows-test-manifest.obj");
    let resource_library = out_dir.join("windows-test-manifest.lib");

    let target_arch = env::var("CARGO_CFG_TARGET_ARCH")?;
    let (tool_arch, machine) = match target_arch.as_str() {
        "x86_64" => ("x64", "X64"),
        "x86" => ("x86", "X86"),
        "aarch64" => ("arm64", "ARM64"),
        unsupported => {
            return Err(format!("unsupported Windows target architecture: {unsupported}").into())
        }
    };

    let rc = find_windows_sdk_tool("rc.exe", tool_arch)?;
    let msvc_bin = find_msvc_bin_dir()?;
    let cvtres = msvc_bin.join("cvtres.exe");
    let librarian = msvc_bin.join("lib.exe");

    let escaped_manifest = manifest
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\"\"");
    fs::write(
        &rc_source,
        format!("#pragma code_page(65001)\n1 24 \"{escaped_manifest}\"\n"),
    )?;

    run_tool(
        Command::new(rc)
            .arg("/nologo")
            .arg("/fo")
            .arg(&compiled_resource)
            .arg(&rc_source),
    )?;
    run_tool(
        Command::new(cvtres)
            .arg("/NOLOGO")
            .arg(format!("/MACHINE:{machine}"))
            .arg(format!("/DEFINE:{RESOURCE_SYMBOL}"))
            .arg(format!("/OUT:{}", resource_object.display()))
            .arg(&compiled_resource),
    )?;
    run_tool(
        Command::new(librarian)
            .arg("/NOLOGO")
            .arg(format!("/MACHINE:{machine}"))
            .arg(format!("/OUT:{}", resource_library.display()))
            .arg(&resource_object),
    )?;

    println!("cargo:rerun-if-changed={}", manifest.display());
    println!(
        "cargo:rustc-env=NEXQ_WINDOWS_TEST_RESOURCE_LIBRARY={}",
        resource_library.display()
    );

    Ok(())
}

#[cfg(target_os = "windows")]
fn find_windows_sdk_tool(
    tool: &str,
    target_arch: &str,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    use std::{env, path::PathBuf};

    let program_files =
        PathBuf::from(env::var_os("ProgramFiles(x86)").ok_or("ProgramFiles(x86) is not set")?);
    let sdk_bin = program_files.join("Windows Kits").join("10").join("bin");
    let version_dir = latest_version_dir(&sdk_bin)?;
    let tool_path = version_dir.join(target_arch).join(tool);
    if !tool_path.is_file() {
        return Err(format!("Windows SDK tool not found at {}", tool_path.display()).into());
    }

    Ok(tool_path)
}

#[cfg(target_os = "windows")]
fn find_msvc_bin_dir() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    use std::{env, path::PathBuf, process::Command};

    let program_files =
        PathBuf::from(env::var_os("ProgramFiles(x86)").ok_or("ProgramFiles(x86) is not set")?);
    let vswhere = program_files
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    let output = Command::new(&vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!("{} failed with {}", vswhere.display(), output.status).into());
    }

    let installation = String::from_utf8(output.stdout)?;
    let tools_dir = PathBuf::from(installation.trim())
        .join("VC")
        .join("Tools")
        .join("MSVC");
    let version_dir = latest_version_dir(&tools_dir)?;
    let bin_dir = version_dir.join("bin").join("Hostx64").join("x64");
    if !bin_dir.join("cvtres.exe").is_file() || !bin_dir.join("lib.exe").is_file() {
        return Err(format!("MSVC resource tools not found in {}", bin_dir.display()).into());
    }

    Ok(bin_dir)
}

#[cfg(target_os = "windows")]
fn latest_version_dir(
    parent: &std::path::Path,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let mut candidates = std::fs::read_dir(parent)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .split('.')
            .map(|part| part.parse::<u32>().unwrap_or_default())
            .collect::<Vec<_>>()
    });

    candidates
        .pop()
        .map(|entry| entry.path())
        .ok_or_else(|| format!("no versioned tool directory found in {}", parent.display()).into())
}

#[cfg(target_os = "windows")]
fn run_tool(command: &mut std::process::Command) -> Result<(), Box<dyn std::error::Error>> {
    let description = format!("{command:?}");
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "{description} failed with {}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    Ok(())
}
