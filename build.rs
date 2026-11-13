fn main() {
    println!("cargo:rerun-if-changed=assets/icon.png");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    // The vendored UnRAR C++ sources use a handful of Win32 APIs. Their import
    // libraries are not added by unrar-sys for the windows-gnu target, so link
    // them explicitly. This is harmless for MSVC builds as well.
    #[cfg(windows)]
    {
        for library in ["advapi32", "shell32", "userenv", "ole32", "oleaut32"] {
            println!("cargo:rustc-link-lib={library}");
        }
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
            for library in ["advapi32", "shell32", "userenv", "ole32", "oleaut32"] {
                println!("cargo:rustc-link-arg=-l{library}");
            }
        }

        // Embed the .ico into the compiled executable when provided. The window
        // icon itself is embedded from assets/icon.png in src/app.rs.
        if std::path::Path::new("assets/icon.ico").exists() {
            let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "1.0.1".into());
            let mut resource = winresource::WindowsResource::new();
            resource.set_icon("assets/icon.ico");
            resource.set("ProductName", "ComicCrate");
            resource.set(
                "FileDescription",
                "ComicCrate - comic archive converter and reader",
            );
            resource.set("ProductVersion", &version);
            resource.set("FileVersion", &version);
            resource.set(
                "LegalCopyright",
                "Copyright (c) 2026 ComicCrate contributors",
            );

            // winresource passes -I<manifest dir> to windres without quoting,
            // which breaks projects living under a path containing spaces.
            // For Windows GNU in that case, generate the .rc through
            // winresource and invoke windres ourselves with the project as the
            // working directory.
            let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
            let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
            let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
            let result = if target_env == "gnu" && manifest_dir.contains(' ') {
                compile_gnu_resource(&resource, &manifest_dir, &out_dir)
            } else {
                resource.compile()
            };
            if let Err(error) = result {
                if target_env == "msvc" {
                    panic!("Could not embed assets/icon.ico into the executable: {error}");
                }
                println!("cargo:warning=Could not embed assets/icon.ico: {error}");
            }
        }
    }
}

#[cfg(windows)]
fn compile_gnu_resource(
    resource: &winresource::WindowsResource,
    manifest_dir: &str,
    out_dir: &str,
) -> std::io::Result<()> {
    use std::path::Path;
    use std::process::Command;

    let rc_path = Path::new(out_dir).join("comiccrate_resource.rc");
    resource.write_resource_file(&rc_path)?;

    let object_path = Path::new(out_dir).join("comiccrate_resource.o");
    let mut command = Command::new("windres");
    command.current_dir(manifest_dir);
    match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => {
            command.args(["--target", "pe-x86-64"]);
        }
        Ok("x86") => {
            command.args(["--target", "pe-i386"]);
        }
        _ => {}
    }
    let status = command.arg(&rc_path).arg("-o").arg(&object_path).status()?;
    if !status.success() {
        return Err(std::io::Error::other(
            "windres failed to compile the resource file",
        ));
    }

    let library_path = Path::new(out_dir).join("libcomiccrate_resource.a");
    let status = Command::new("ar")
        .current_dir(manifest_dir)
        .arg("rsc")
        .arg(&library_path)
        .arg(&object_path)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(
            "ar failed to create the resource library",
        ));
    }

    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-arg={}", object_path.display());
    Ok(())
}
