# Scanner Workbench

Scanner Workbench is a generic OBD-II diagnostic app for Windows with support for a wide range of J2534 adapters. Read emissions trouble codes, readiness monitors, freeze frames, VIN and live data.

**[Download the Windows installer](https://github.com/Supersam902/scanner-workbench/releases/latest/download/ScannerWorkbenchApp-win-x86-Setup.exe)** · [Portable ZIP and release notes](https://github.com/Supersam902/scanner-workbench/releases/latest)

## Adapter compatibility

| Adapter connection | Where it works |
| --- | --- |
| J2534 pass-through with a 32-bit Windows 04.04 driver | Scanner Workbench |
| ELM-compatible USB or Bluetooth that appears as a serial port | Classic desktop dashboard |
| ELM-compatible Wi-Fi with a TCP connection | Classic desktop dashboard |
| ELM-compatible serial available through Web Serial | Classic browser dashboard |

Install your adapter's **32-bit Windows J2534 04.04 driver**. Plug in the adapter, turn on the ignition and click **Connect**. Workbench verifies an OBD-II reply before showing a connection. Choose an installed driver under **Connection → Adapter** to use a particular cable. Workbench saves that choice and uses it for the DLC voltage readout as well.

Workbench checks ISO 15765 CAN (11-bit and 29-bit), ISO 9141-2, ISO 14230 KWP2000, and SAE J1850 VPW/PWM. Available readings depend on the adapter and the ECUs that respond.

## Scan and save results

- **Overview** runs an emissions scan with stored, pending and permanent codes, readiness since clear and this drive cycle, VIN, and freeze frame.
- **Live data** shows supported PIDs with graphs and recordings. The header displays DLC voltage and a click-to-copy VIN.
- **Trouble codes** can save a before-clear scan, confirm an emissions clear and save a follow-up scan. Scan history can compare scans that match the same vehicle.
- **Reports** opens a print-ready scan report. Recordings also export to CSV.

The installer checks the public GitHub release feed. Open **Connection → Updates** to check for an update or restart into a staged version. Vehicle data stays in `%LOCALAPPDATA%/ScannerWorkbench`.

Use `--demo` for a practice session and `--data-dir PATH` for separate local storage. See the [Workbench guide](docs/scanner-workbench.txt) and [release documentation](docs/scanner-releases.md).

## Build from source

Clone this repository and use a Windows x86 Rust target with a matching MinGW toolchain.

```powershell
cargo build --locked --release --target i686-pc-windows-gnu --bin scanner-workstation
cargo test --locked --target i686-pc-windows-gnu --all-targets
```

The classic `obd-dashboard` target supports generic OBD-II through ELM-compatible serial or TCP adapters and native J2534. The browser build uses Web Serial. See [adapter implementation notes](docs/adapters.md).

The application uses the [MIT license](LICENSE). Generic OBDb data is licensed under CC BY-SA 4.0. See the [data attribution](data/obdb/ATTRIBUTION.txt).
