#![cfg(any(target_os = "linux", all(target_os = "windows", target_arch = "x86")))]
use obd_dashboard::adapter::{DiagnosticAdapter, DiagnosticTarget};
use obd_dashboard::elm327::block_on;
use obd_dashboard::j2534::{CanProtocol, J2534};

#[test]
fn native_ecu_filters_and_legacy_initialization_and_keepalive_cleanup() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let folder = root.join("target/extended-fixture");
    std::fs::create_dir_all(&folder).unwrap();
    #[cfg(target_os = "windows")]
    let (compiler, library) = (
        std::env::var_os("CC_i686_pc_windows_gnu")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "i686-w64-mingw32-gcc.exe".into()),
        folder.join("fixture.dll"),
    );
    #[cfg(target_os = "linux")]
    let (compiler, library) = (std::path::PathBuf::from("cc"), folder.join("fixture.so"));
    let mut command = std::process::Command::new(compiler);
    command.args(["-shared", "-Wall", "-Wextra", "-Werror"]);
    #[cfg(target_os = "windows")]
    command.arg("-Wl,--kill-at");
    #[cfg(target_os = "linux")]
    command.arg("-fPIC");
    assert!(
        command
            .arg(root.join("tests/fixtures/j2534_extended.c"))
            .arg("-o")
            .arg(&library)
            .status()
            .unwrap()
            .success()
    );
    let control = unsafe { libloading::Library::new(&library).unwrap() };
    let counter = unsafe {
        control
            .get::<unsafe extern "system" fn(u32) -> u32>(b"MockCounter\0")
            .unwrap()
    };
    let scenario = unsafe {
        control
            .get::<unsafe extern "system" fn(u32)>(b"MockScenario\0")
            .unwrap()
    };
    {
        let mut adapter = J2534::connect(&library, CanProtocol::Can11Bit500K).unwrap();
        assert_eq!(unsafe { counter(0) }, 8);
        let replies = block_on(adapter.request_to(
            DiagnosticTarget {
                request: 0x7b0,
                response: 0x7b8,
            },
            &[0x21, 0x82],
            1000,
        ))
        .unwrap();
        assert_eq!(replies[0].source, Some(0x7b8));
        assert_eq!(replies[0].payload, [0x61, 0x82, 0x40, 0, 0x45, 0]);
        assert_eq!(unsafe { counter(0) }, 8);
        assert_eq!(unsafe { counter(1) }, 1);
        unsafe { scenario(1) };
        let replies = block_on(adapter.request_to(
            DiagnosticTarget {
                request: 0x7b0,
                response: 0x7b8,
            },
            &[0x21, 0x82],
            1000,
        ))
        .unwrap();
        assert_eq!(replies[0].payload, [0x7f, 0x21, 0x22]);
        assert_eq!(unsafe { counter(0) }, 8);
        unsafe { scenario(0) };
        for (request, expected) in [
            (vec![0x22, 0x20, 0x17], vec![0x62, 0x20, 0x17, 0, 80]),
            (
                vec![0x19, 2, 0xff],
                vec![0x59, 2, 0xff, 0x12, 0x34, 0x56, 9],
            ),
        ] {
            let replies = block_on(adapter.request_to(
                DiagnosticTarget {
                    request: 0x7e0,
                    response: 0x7e8,
                },
                &request,
                1000,
            ))
            .unwrap();
            assert_eq!(replies[0].source, Some(0x7e8));
            assert_eq!(replies[0].payload, expected);
            assert_eq!(unsafe { counter(0) }, 8);
        }
    }
    for protocol in [
        CanProtocol::Iso9141,
        CanProtocol::KwpFast,
        CanProtocol::KwpSlow,
    ] {
        let before = unsafe { counter(4) };
        {
            let mut adapter = J2534::connect(&library, protocol).unwrap();
            assert_eq!(unsafe { counter(2) }, 1);
            assert_eq!(unsafe { counter(0) }, 1);
            let replies = block_on(adapter.request_to(
                DiagnosticTarget {
                    request: 0x10,
                    response: 0x10,
                },
                &[0x21, 0x82],
                1000,
            ))
            .unwrap();
            assert_eq!(replies[0].source, Some(0x10));
            assert_eq!(replies[0].payload, [0x61, 0x82, 0x40, 0, 0x45, 0]);
            assert!(
                block_on(adapter.request_to(
                    DiagnosticTarget {
                        request: 0x7e0,
                        response: 0x7e8
                    },
                    &[0x21, 0x82],
                    1000
                ))
                .is_err()
            );
        }
        assert_eq!(unsafe { counter(4) }, before + 1);
    }
    assert_eq!(unsafe { counter(5) }, 4);
}
