# Scanner Workbench releases on GitHub

Scanner Workbench releases are hosted at [Supersam902/scanner-workbench](https://github.com/Supersam902/scanner-workbench/releases). A Workbench release uses Windows x86 to load the 32-bit Mongoose MFC3 J2534 driver. The separate OBD-II Dashboard release workflow builds the original app for several platforms.

## Package from the checkout

Build the locked Rust project for `i686-pc-windows-gnu` using an x86 MinGW toolchain and run the test suite with the matching C compiler. From the repository root, package the result in PowerShell 7.

```powershell
./scripts/package-scanner.ps1 -Binary ./target/i686-pc-windows-gnu/release/scanner-workstation.exe
```

The script writes `dist/scanner-workbench-mfc3-windows-x86.zip` and its `.sha256` file. The archive contains the app, code definitions, and license notices. The driver is installed separately on the vehicle computer. The packager reads only the build and source assets, so it runs without a connected cable or access to local scan data.

## Publish the first build

1. [Scanner Workbench Windows x86](../.github/workflows/scanner-release.yml) runs for changes to `main` and for pull requests into `main`. You can also run it from the Actions tab. It uses Rust 1.99.1 and WinLibs GCC 16.2.0 for i686, runs the x86 tests, and uploads the ZIP and checksum to the run. Enable **Run an isolated Windows upgrade test** on a manual run to check installation and restart in a scratch directory.
2. Push an annotated tag such as `workbench-v0.1.0` on the commit to release. The workflow embeds the version from the tag in the executable, runs the isolated upgrade test and creates a draft release with the ZIP, installer and feed. The About panel under Connection shows the build version.
3. Verify the draft on a clean Windows computer with the Mongoose driver installed. Publish the draft as the latest stable release to activate the shared [portable ZIP download](https://github.com/Supersam902/scanner-workbench/releases/latest/download/scanner-workbench-mfc3-windows-x86.zip). Use a dedicated repository for this feed if other apps have their own release tags. [Linking to GitHub releases](https://docs.github.com/en/repositories/releasing-projects-on-github/linking-to-releases)

## Enable the installer and update feed

The Workbench workflow embeds `Supersam902/scanner-workbench` in the scanner on that repository. For a separate repository, set the Actions variable `SCANNER_WORKBENCH_UPDATE_REPOSITORY` to its `owner/name`. On a release tag it uses Velopack 1.2.161 to attach `ScannerWorkbenchApp-win-x86-Setup.exe`, `releases.win-x86.json`, and the matching full `.nupkg` alongside the ZIP. Publish all assets together to activate the [installer download](https://github.com/Supersam902/scanner-workbench/releases/latest/download/ScannerWorkbenchApp-win-x86-Setup.exe). The feed needs public access for automatic checks without a token. [Velopack update sources](https://docs.velopack.io/integrating/update-sources)

Install from Setup to use updates. On startup the installed build checks for a newer stable Windows x86 release and stages it in the background. The Software updates card under Connection can retry a failed check. When a download is ready, **Restart to update** becomes available after disconnect and after pending scans and recordings are saved. Applying the update stops the diagnostic worker first. The installation lives under `ScannerWorkbenchApp`, separately from vehicle data in `%LOCALAPPDATA%\ScannerWorkbench`.

The current workflow produces unsigned Windows binaries. Before a public rollout, configure publisher signing and verify a full update on a clean Windows system. The scanner validates the feed's package ID, version, SHA-256 digest, and the package's Windows x86 manifest. [Velopack code signing](https://docs.velopack.io/packaging/signing)

## Verify an upgrade locally

Build two successive Windows x86 release binaries using `SCANNER_WORKBENCH_VERSION` and `--features scanner-update-smoke`. For each version, use `package-scanner.ps1` with a fresh output directory and the matching `-ExpectedVersion`. Run `package-scanner-installer.ps1 -ForLocalTest` against each staged `Scanner-Workbench` directory, with the matching `-Version` and an installed `vpk` 1.2.161. This produces an updater-enabled portable ZIP for isolation.

Run `scripts/test-scanner-update.ps1` with `-OldPortableZip` pointing to the older Velopack ZIP, `-NewFeed` pointing to the newer installer output directory, and `-OutputDirectory` pointing to a new scratch directory. The check runs the scanner's update code without a vehicle connection, rejects a truncated package, recovers from a corrupt cached download, restarts on the newer version, and verifies that saved settings survived. This complements a test of the published GitHub feed and the regular Setup installer.
