use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use velopack::bundle::Manifest;
use velopack::locator::{LocationContext, VelopackLocator, auto_locate_app_manifest};
#[cfg(feature = "scanner-update-smoke")]
use velopack::sources::FileSource;
use velopack::sources::{GithubSource, UpdateSource};
use velopack::{Error, UpdateCheck, UpdateInfo, UpdateManager, VelopackAsset, VelopackAssetFeed};

const PACKAGE_ID: &str = "ScannerWorkbenchApp";
const CHANNEL: &str = "win-x86";
const EXECUTABLE: &str = "scanner-workbench.exe";

pub const REPOSITORY: Option<&str> = option_env!("SCANNER_WORKBENCH_UPDATE_REPOSITORY");

pub fn initialize() {
    velopack::VelopackApp::build()
        .set_auto_apply_on_startup(false)
        .run();
}

fn github_source() -> Option<GithubSource> {
    REPOSITORY.map(|repository| {
        GithubSource::new(&format!("https://github.com/{repository}"), None, false)
    })
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
    Current,
    Ready(ReadyUpdate),
    Failed(String),
}

pub(super) struct Updates {
    state: State,
    result: Option<Receiver<Result<Option<ReadyUpdate>, String>>>,
}

impl Updates {
    pub(super) fn new() -> Self {
        Self {
            state: State::Idle,
            result: None,
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
            && auto_locate_app_manifest(LocationContext::FromCurrentExe)
                .is_ok_and(|locator| workbench_package(&locator.get_manifest()))
    }

    pub(super) fn check(&mut self) {
        let Some(source) = github_source() else {
            return;
        };
        if matches!(self.state, State::Checking | State::Ready(_)) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.state = State::Checking;
        self.result = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(stage(source).map_err(|err| err.to_string()));
        });
    }

    pub(super) fn tick(&mut self) {
        let Some(rx) = &self.result else { return };
        self.state = match rx.try_recv() {
            Ok(Ok(None)) => State::Current,
            Ok(Ok(Some(ready))) => State::Ready(ready),
            Ok(Err(err)) => State::Failed(err),
            Err(mpsc::TryRecvError::Disconnected) => {
                State::Failed("Update check ended early.".into())
            }
            Err(mpsc::TryRecvError::Empty) => return,
        };
        self.result = None;
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
    Ok(Some(ReadyUpdate {
        manager,
        asset,
        path,
    }))
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
            github_source()
                .ok_or_else(|| Error::Other("No GitHub repository configured.".into()))?,
        )?,
    };
    let Some(ready) = ready else {
        std::fs::write(output, super::VERSION)?;
        return Ok(());
    };
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
