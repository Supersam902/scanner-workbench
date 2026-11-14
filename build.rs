use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=SCANNER_WORKBENCH_VERSION");
    let version = env::var("SCANNER_WORKBENCH_VERSION").unwrap_or_else(|_| "development".into());
    let [major, minor, patch] = version_numbers(&version);
    println!("cargo:rustc-env=SCANNER_WORKBENCH_VERSION={version}");
    println!("cargo:rerun-if-env-changed=SCANNER_WORKBENCH_UPDATE_REPOSITORY");
    if let Ok(repo) = env::var("SCANNER_WORKBENCH_UPDATE_REPOSITORY") {
        let valid_part = |part: &str| {
            !part.is_empty()
                && !matches!(part, "." | "..")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        };
        if !repo.is_empty() {
            assert!(
                repo.split_once('/')
                    .is_some_and(|(owner, name)| valid_part(owner) && valid_part(name)),
                "SCANNER_WORKBENCH_UPDATE_REPOSITORY must be a GitHub owner/repository"
            );
            println!("cargo:rustc-env=SCANNER_WORKBENCH_UPDATE_REPOSITORY={repo}");
        }
    }

    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "windows" {
        return;
    }
    let gnu = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
    if gnu {
        // MinGW otherwise tries to auto-export Rust internals from the cdylib.
        println!("cargo:rustc-link-arg-cdylib=-Wl,--exclude-all-symbols");
    }
    let icon = "assets/scanner/workbench-icon.ico";
    println!("cargo:rerun-if-changed={icon}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=WINDRES");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo build output directory"));
    let icon_path = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join(icon);
    let resource = out.join("workbench.rc");
    fs::write(
        &resource,
        format!(
            r#"1 ICON "{}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "FileDescription", "Scanner Workbench"
            VALUE "FileVersion", "{version}"
            VALUE "ProductName", "Scanner Workbench"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#,
            icon_path.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("write Windows icon resource");
    let compiled = if gnu {
        let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let prefix = if arch == "x86" { "i686" } else { &arch };
        let tool = env::var_os("WINDRES")
            .unwrap_or_else(|| format!("{prefix}-w64-mingw32-windres").into());
        let output = out.join("workbench.o");
        Command::new(tool)
            .args(["--input-format=rc", "--output-format=coff"])
            .arg(&resource)
            .arg(&output)
            .status()
            .map(|status| (status, output))
    } else {
        let output = out.join("workbench.res");
        Command::new("rc.exe")
            .arg("/nologo")
            .arg("/fo")
            .arg(&output)
            .arg(&resource)
            .status()
            .map(|status| (status, output))
    };
    match compiled {
        Ok((status, output)) if status.success() => {
            println!(
                "cargo:rustc-link-arg-bin=scanner-workstation={}",
                output.display()
            );
        }
        result => println!("cargo:warning=Windows Workbench resources unavailable: {result:?}"),
    }
}

fn version_numbers(version: &str) -> [u16; 3] {
    if version == "development" {
        return [0, 0, 0];
    }
    let numbers = version
        .split('.')
        .map(str::parse::<u16>)
        .collect::<Result<Vec<_>, _>>();
    if let Ok(numbers) = numbers {
        if let [major, minor, patch] = numbers.as_slice() {
            if format!("{major}.{minor}.{patch}") == version {
                return [*major, *minor, *patch];
            }
        }
    }
    panic!(
        "SCANNER_WORKBENCH_VERSION must be development or MAJOR.MINOR.PATCH (each 0 through 65535)"
    );
}
