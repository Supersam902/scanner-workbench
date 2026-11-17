use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use velopack::bundle::Manifest;
use velopack::download::{download_url_as_string, download_url_to_file};
use velopack::locator::{LocationContext, VelopackLocator, auto_locate_app_manifest};
#[cfg(any(test, feature = "scanner-update-smoke"))]
use velopack::sources::FileSource;
use velopack::sources::UpdateSource;
use velopack::{
    Error, HttpOptions, UpdateCheck, UpdateInfo, UpdateManager, VelopackAsset, VelopackAssetFeed,
};

const PACKAGE_ID: &str = "ScannerWorkbenchApp";
const CHANNEL: &str = "win-x86";
const EXECUTABLE: &str = "scanner-workbench.exe";
const PENDING_UPDATE: &str = "scanner-workbench-pending.json";

#[derive(Default)]
pub(super) struct StartupUpdate {
    deadline: Option<Instant>,
    dismissed: bool,
}

impl StartupUpdate {
    pub(super) fn remaining(&mut self, ready: bool, now: Instant) -> Option<u64> {
        if self.dismissed || !ready {
            return None;
        }
        let deadline = self.deadline.get_or_insert(now + Duration::from_secs(5));
        let remaining = deadline.saturating_duration_since(now);
        Some(remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0))
    }

    pub(super) fn dismiss(&mut self) {
        self.dismissed = true;
        self.deadline = None;
    }
}

pub const REPOSITORY: Option<&str> = option_env!("SCANNER_WORKBENCH_UPDATE_REPOSITORY");

pub fn initialize() {
    velopack::VelopackApp::build()
        .set_auto_apply_on_startup(false)
        .run();
}

fn github_source(status: Option<Sender<State>>) -> Option<PublicGithubRelease> {
    REPOSITORY.map(|repository| PublicGithubRelease {
        releases: format!("https://github.com/{repository}/releases"),
        status,
    })
}

struct PublicGithubRelease {
    releases: String,
    status: Option<Sender<State>>,
}

impl UpdateSource for PublicGithubRelease {
    fn get_release_feed(
        &self,
        channel: &str,
        _app: &Manifest,
        _staged_user_id: &str,
    ) -> Result<VelopackAssetFeed, Error> {
        let url = format!("{}/latest/download/releases.{channel}.json", self.releases);
        let options = HttpOptions {
            TimeoutMilliseconds: 15_000,
            ..Default::default()
        };
        let json = download_url_as_string(&url, Some(&options))?;
        Ok(serde_json::from_str(&json)?)
    }

    fn download_release_entry(
        &self,
        asset: &VelopackAsset,
        local_file: &Path,
        progress: Option<Sender<i16>>,
    ) -> Result<(), Error> {
        // Use the release named in the validated feed if Latest changes during the download.
        let url = format!(
            "{}/download/workbench-v{}/{}",
            self.releases, asset.Version, asset.FileName
        );
        let options = HttpOptions {
            TimeoutMilliseconds: 180_000,
            ..Default::default()
        };
        let report = |percent: i16| {
            if let Some(status) = &self.status {
                let _ = status.send(State::Downloading {
                    version: asset.Version.clone(),
                    percent: percent.clamp(0, 100) as u8,
                });
            }
            if let Some(progress) = &progress {
                let _ = progress.send(percent);
            }
        };
        report(0);
        download_url_to_file(&url, local_file, Some(&options), report)
    }
}

fn manager(source: impl UpdateSource + 'static) -> Result<(UpdateManager, VelopackLocator), Error> {
    let locator = auto_locate_app_manifest(LocationContext::FromCurrentExe)?;
    let manifest = locator.get_manifest();
    if !workbench_package(&manifest) || manifest.version.to_string() != super::VERSION {
        return Err(Error::NotInstalled(
            "Invalid Scanner Workbench installation".into(),
        ));
    }
    let manager = UpdateManager::new(WorkbenchSource(source), None, None)?;
    Ok((manager, locator))
}

fn workbench_package(manifest: &Manifest) -> bool {
    manifest.id == PACKAGE_ID
        && manifest.channel == CHANNEL
        && manifest.machine_architecture == "x86"
        && manifest.os == "win"
        && manifest.main_exe == EXECUTABLE
}

fn valid_asset(asset: &VelopackAsset) -> bool {
    let stable = asset.Version.split('.').count() == 3
        && asset.Version.split('.').all(|part| {
            part.parse::<u16>()
                .is_ok_and(|number| number.to_string() == part)
        });
    asset.PackageId == PACKAGE_ID
        && asset.Type.eq_ignore_ascii_case("full")
        && stable
        && asset.FileName == format!("{PACKAGE_ID}-{}-{CHANNEL}-full.nupkg", asset.Version)
        && asset.SHA256.len() == 64
        && asset.SHA256.bytes().all(|b| b.is_ascii_hexdigit())
        && (1..=128 * 1024 * 1024).contains(&asset.Size)
}

struct WorkbenchSource<S>(S);

impl<S: UpdateSource> UpdateSource for WorkbenchSource<S> {
    fn get_release_feed(
        &self,
        channel: &str,
        app: &Manifest,
        staged_user_id: &str,
    ) -> Result<VelopackAssetFeed, Error> {
        let mut feed = self.0.get_release_feed(channel, app, staged_user_id)?;
        feed.Assets.retain(valid_asset);
        Ok(feed)
    }

    fn download_release_entry(
        &self,
        asset: &VelopackAsset,
        local_file: &Path,
        progress: Option<Sender<i16>>,
    ) -> Result<(), Error> {
        if !valid_asset(asset) {
            return Err(Error::InvalidPackage("Unexpected Workbench asset".into()));
        }
        self.0.download_release_entry(asset, local_file, progress)?;
        verify_package(local_file, asset)
    }
}

fn verify_package(path: &Path, asset: &VelopackAsset) -> Result<(), Error> {
    let length = path.metadata()?.len();
    if length != asset.Size {
        return Err(Error::SizeInvalid(path.into(), asset.Size, length));
    }
    let (_, hash) = velopack::misc::calculate_sha1_sha256(path)?;
    if !hash.eq_ignore_ascii_case(&asset.SHA256) {
        return Err(Error::ChecksumInvalid(
            path.into(),
            asset.SHA256.clone(),
            hash,
        ));
    }
    let manifest = velopack::bundle::load_bundle_from_file(path)?.read_manifest()?;
    if !workbench_package(&manifest) || manifest.version.to_string() != asset.Version {
        return Err(Error::InvalidPackage(
            "Unexpected Workbench package manifest".into(),
        ));
    }
    Ok(())
}

pub(super) enum State {
    Idle,
    Checking,
    Downloading { version: String, percent: u8 },
    Current,
    Ready(ReadyUpdate),
    Failed(String),
}

pub(super) struct Updates {
    state: State,
    events: Option<Receiver<State>>,
}

impl Updates {
    pub(super) fn new() -> Self {
        Self {
            state: State::Idle,
            events: None,
        }
    }

    #[cfg(test)]
    pub(super) fn test_ready(root: &Path) -> Self {
        let paths = velopack::locator::create_config_from_root_dir(root);
        std::fs::create_dir_all(&paths.CurrentBinaryDir).unwrap();
        std::fs::create_dir_all(&paths.PackagesDir).unwrap();
        std::fs::write(&paths.UpdateExePath, b"test fixture, never executed").unwrap();
        std::fs::write(&paths.ManifestPath, format!(
            "<package><metadata><id>{PACKAGE_ID}</id><version>0.1.0</version><channel>{CHANNEL}</channel><mainExe>{EXECUTABLE}</mainExe><machineArchitecture>x86</machineArchitecture><os>win</os></metadata></package>"
        )).unwrap();
        let manager = UpdateManager::new(FileSource::new(root), None, Some(paths.clone())).unwrap();
        let asset = VelopackAsset {
            PackageId: PACKAGE_ID.into(),
            Version: "1.2.3".into(),
            Type: "Full".into(),
            FileName: format!("{PACKAGE_ID}-1.2.3-{CHANNEL}-full.nupkg"),
            SHA256: "0".repeat(64),
            Size: 1,
            ..Default::default()
        };
        let path = paths.PackagesDir.join(&asset.FileName);
        Self {
            state: State::Ready(ReadyUpdate {
                manager,
                asset,
                path,
            }),
            events: None,
        }
    }

    pub(super) fn state(&self) -> &State {
        &self.state
    }

    pub(super) fn fail(&mut self, message: String) {
        self.state = State::Failed(message);
    }

    pub(super) fn take_ready(&mut self) -> Option<ReadyUpdate> {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Ready(update) => Some(update),
            previous => {
                self.state = previous;
                None
            }
        }
    }

    pub(super) fn available() -> bool {
        REPOSITORY.is_some()
            && auto_locate_app_manifest(LocationContext::FromCurrentExe).is_ok_and(|locator| {
                let manifest = locator.get_manifest();
                workbench_package(&manifest) && manifest.version.to_string() == super::VERSION
            })
    }

    pub(super) fn check(&mut self) {
        if self.events.is_some() || matches!(self.state, State::Ready(_)) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let Some(source) = github_source(Some(tx.clone())) else {
            return;
        };
        self.state = State::Checking;
        self.events = Some(rx);
        std::thread::spawn(move || {
            let state = match restore_pending()
                .and_then(|cached| cached.map_or_else(|| stage(source), |update| Ok(Some(update))))
            {
                Ok(None) => State::Current,
                Ok(Some(ready)) => State::Ready(ready),
                Err(err) => {
                    tracing::warn!(%err, "Software update check failed");
                    State::Failed(err.to_string())
                }
            };
            let _ = tx.send(state);
        });
    }

    pub(super) fn tick(&mut self) {
        while let Some(rx) = &self.events {
            self.state = match rx.try_recv() {
                Ok(state) => state,
                Err(mpsc::TryRecvError::Disconnected) => {
                    State::Failed("Update check ended early.".into())
                }
                Err(mpsc::TryRecvError::Empty) => return,
            };
            if !matches!(self.state, State::Downloading { .. }) {
                self.events = None;
            }
        }
    }
}

pub(super) struct ReadyUpdate {
    manager: UpdateManager,
    asset: VelopackAsset,
    path: PathBuf,
}

impl ReadyUpdate {
    pub(super) fn version(&self) -> &str {
        &self.asset.Version
    }

    pub(super) fn apply(
        &self,
        restart_args: impl IntoIterator<Item = OsString>,
    ) -> Result<(), Error> {
        verify_package(&self.path, &self.asset)?;
        self.manager
            .wait_exit_then_apply_updates(&self.asset, true, true, restart_args)
    }
}

fn stage(source: impl UpdateSource + 'static) -> Result<Option<ReadyUpdate>, Error> {
    let (manager, locator) = manager(source)?;
    let asset = match manager.check_for_updates()? {
        UpdateCheck::UpdateAvailable(info) => info.TargetFullRelease,
        UpdateCheck::NoUpdateAvailable => return Ok(None),
        UpdateCheck::RemoteIsEmpty => {
            return Err(Error::Other(
                "The feed has no compatible Workbench release.".into(),
            ));
        }
    };
    let path = locator.get_packages_dir().join(&asset.FileName);
    {
        // Velopack reuses an existing final package without checking it again.
        let _lock = locator.try_get_exclusive_lock()?;
        match verify_package(&path, &asset) {
            Ok(()) => {
                remember_pending(&locator, &asset);
                return Ok(Some(ReadyUpdate {
                    manager,
                    asset,
                    path,
                }));
            }
            Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(Error::SizeInvalid(..) | Error::ChecksumInvalid(..)) => {
                std::fs::remove_file(&path)?;
            }
            Err(err) => return Err(err),
        }
    }
    manager.download_updates(
        &UpdateInfo {
            TargetFullRelease: asset.clone(),
            ..Default::default()
        },
        None,
    )?;
    verify_package(&path, &asset)?;
    remember_pending(&locator, &asset);
    Ok(Some(ReadyUpdate {
        manager,
        asset,
        path,
    }))
}

fn remember_pending(locator: &VelopackLocator, asset: &VelopackAsset) {
    let path = locator.get_packages_dir().join(PENDING_UPDATE);
    if let Err(error) = super::model::save_json(&path, asset) {
        tracing::warn!(%error, "Could not remember the staged update");
    }
}

fn restore_pending() -> Result<Option<ReadyUpdate>, Error> {
    let Some(source) = github_source(None) else {
        return Ok(None);
    };
    let (manager, locator) = manager(source)?;
    Ok(read_pending(manager, &locator))
}

fn read_pending(manager: UpdateManager, locator: &VelopackLocator) -> Option<ReadyUpdate> {
    let record = locator.get_packages_dir().join(PENDING_UPDATE);
    let Ok(asset) = super::model::read_json::<VelopackAsset>(&record) else {
        return None;
    };
    if !valid_asset(&asset) {
        return None;
    }
    let installed = manager.get_current_version();
    let newer = asset
        .Version
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .cmp([installed.major, installed.minor, installed.patch])
        .is_gt();
    if !newer {
        return None;
    }
    let path = locator.get_packages_dir().join(&asset.FileName);
    if let Err(error) = verify_package(&path, &asset) {
        tracing::warn!(%error, "Staged update needs to be downloaded again");
        return None;
    }
    Some(ReadyUpdate {
        manager,
        asset,
        path,
    })
}

/// Exercises staging from a local or GitHub feed, with restart for local smoke tests.
#[cfg(feature = "scanner-update-smoke")]
#[doc(hidden)]
pub fn verify_update(
    feed: Option<&Path>,
    output: &Path,
    data: &Path,
    apply: bool,
) -> Result<(), Error> {
    // Exercise the same settings read used by the desktop app after the restart.
    super::model::Settings::load(data).map_err(Error::Other)?;
    let ready = match feed {
        Some(feed) => stage(FileSource::new(feed))?,
        None => stage(
            github_source(None)
                .ok_or_else(|| Error::Other("No GitHub repository configured.".into()))?,
        )?,
    };
    let Some(ready) = ready else {
        std::fs::write(output, super::VERSION)?;
        return Ok(());
    };
    let locator = auto_locate_app_manifest(LocationContext::FromCurrentExe)?;
    let ready = read_pending(ready.manager.clone(), &locator)
        .ok_or_else(|| Error::Other("The staged update could not be restored.".into()))?;
    if apply && let Some(feed) = feed {
        ready.apply([
            OsString::from("--verify-local-update"),
            feed.as_os_str().to_owned(),
            output.as_os_str().to_owned(),
            data.as_os_str().to_owned(),
        ])?;
    } else {
        std::fs::write(output, ready.version())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_countdown_starts_when_ready_and_dismisses_for_the_session() {
        let start = Instant::now();
        let mut startup = StartupUpdate::default();
        assert_eq!(startup.remaining(false, start), None);
        let ready = start + Duration::from_secs(20);
        assert_eq!(startup.remaining(true, ready), Some(5));
        assert_eq!(
            startup.remaining(true, ready + Duration::from_millis(999)),
            Some(5)
        );
        assert_eq!(
            startup.remaining(true, ready + Duration::from_secs(1)),
            Some(4)
        );
        assert_eq!(
            startup.remaining(true, ready + Duration::from_millis(4999)),
            Some(1)
        );
        assert_eq!(
            startup.remaining(true, ready + Duration::from_micros(4_999_999)),
            Some(1)
        );
        assert_eq!(
            startup.remaining(true, ready + Duration::from_secs(5)),
            Some(0)
        );
        startup.dismiss();
        assert_eq!(
            startup.remaining(true, ready + Duration::from_secs(10)),
            None
        );
    }

    #[test]
    fn cached_update_rejects_invalid_metadata_and_changed_package_bytes() {
        let root = tempfile::tempdir().unwrap();
        let updates = Updates::test_ready(root.path());
        let State::Ready(ready) = updates.state else {
            panic!("expected staged update")
        };
        let paths = velopack::locator::create_config_from_root_dir(root.path());
        let locator = VelopackLocator::new(&paths).unwrap();
        remember_pending(&locator, &ready.asset);
        std::fs::write(&ready.path, b"x").unwrap();
        assert!(read_pending(ready.manager.clone(), &locator).is_none());
        let mut invalid = ready.asset;
        invalid.FileName = "../scanner.exe".into();
        remember_pending(&locator, &invalid);
        assert!(read_pending(ready.manager, &locator).is_none());
    }

    #[test]
    fn download_progress_keeps_the_check_active_until_its_result() {
        let (tx, rx) = mpsc::channel();
        let mut updates = Updates {
            state: State::Checking,
            events: Some(rx),
        };
        for percent in [0, 25, 75] {
            tx.send(State::Downloading {
                version: "1.2.3".into(),
                percent,
            })
            .unwrap();
        }
        updates.tick();
        updates.check();
        assert!(matches!(
            updates.state(),
            State::Downloading { version, percent: 75 } if version == "1.2.3"
        ));

        tx.send(State::Failed("Download interrupted".into()))
            .unwrap();
        updates.tick();
        assert!(matches!(updates.state(), State::Failed(error) if error == "Download interrupted"));
        assert!(updates.events.is_none());
    }
}
