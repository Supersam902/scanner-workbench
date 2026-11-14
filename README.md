# Scanner Workbench

Scanner Workbench is a generic OBD-II scan tool for Windows that works with a wide range of adapters. Connect with an ELM327-compatible USB, Bluetooth serial, or Wi-Fi/TCP adapter, or with a compatible J2534 pass-through interface. Read trouble codes, emissions readiness, freeze frames, VIN, and live data.

**[Download the Windows installer](https://github.com/Supersam902/scanner-workbench/releases/latest/download/ScannerWorkbenchApp-win-x86-Setup.exe)** · [Portable ZIP and release notes](https://github.com/Supersam902/scanner-workbench/releases/latest)

## Adapter compatibility

| Adapter type | What to look for |
| --- | --- |
| ELM327-compatible USB | A Windows COM port when plugged in |
| ELM327-compatible Bluetooth serial (SPP) | A Windows COM port after pairing |
| ELM327-compatible Wi-Fi | A TCP host and port in the adapter's instructions |
| J2534 pass-through | A vendor-supplied 32-bit Windows J2534 04.04 driver |

Open **Connection → Adapter** to choose the adapter type. J2534 uses the vendor's **32-bit Windows 04.04 driver** and detects supported cables automatically. For ELM serial, choose the adapter's COM port. For Wi-Fi/TCP, enter the host and port specified by the adapter. Workbench saves your choice for **Connect** and the DLC voltage readout. It verifies an OBD-II reply before showing the vehicle as connected.

Use **Copy connection report** under **Connection → Adapter** to share the adapter settings, voltage at Connect, and protocol results when troubleshooting.

Check that the adapter supports your vehicle's protocol. Workbench supports ISO 15765 CAN (11-bit and 29-bit), ISO 9141-2, ISO 14230 KWP2000, and SAE J1850 VPW/PWM. ELM327-compatible adapters need automatic OBD-II protocol detection and ECU response headers. Available readings depend on the adapter and the ECUs that respond.

## Scan and save results

- **Overview** runs an emissions scan with stored, pending and permanent codes, readiness since clear and this drive cycle, VIN, and freeze frame.
- **Live data** shows supported PIDs with graphs and recordings. Save named PID selections from **Presets** and review readings before and after a marked event in a saved recording. The header displays DLC voltage and a click-to-copy VIN.
- **Trouble codes** can save a before-clear scan, confirm an emissions clear and save a follow-up scan. Scan history can compare scans that match the same vehicle.
- **Reports** opens a print-ready scan report. Add a work order and technician notes from **Scan review → Notes**. Recordings also export to CSV.

The installer checks the public GitHub release feed. Open **Connection → Updates** to check for an update or restart into a staged version. Vehicle data stays in `%LOCALAPPDATA%/ScannerWorkbench`.

Use `--demo` for a practice session and `--data-dir PATH` for separate local storage. See the [Workbench guide](docs/scanner-workbench.txt) and [release documentation](docs/scanner-releases.md).

## Build from source

Clone this repository and use a Windows x86 Rust target with a matching MinGW toolchain.

```powershell
cargo build --locked --release --target i686-pc-windows-gnu --bin scanner-workstation
cargo test --locked --target i686-pc-windows-gnu --all-targets
```

### Classic dashboard

Build the classic desktop app with `cargo build --locked --release --bin obd-dashboard`. Select **Serial** for an ELM-compatible adapter that appears as a serial port, or **TCP** and enter the adapter's host and port. For the browser build, install the `wasm32-unknown-unknown` target and run `trunk serve` from the checkout. Open it in a browser with Web Serial. See [adapter implementation notes](docs/adapters.md).

The application uses the [MIT license](LICENSE). Generic OBDb data is licensed under CC BY-SA 4.0. See the [data attribution](data/obdb/ATTRIBUTION.txt).
