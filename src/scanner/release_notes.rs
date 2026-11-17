use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use serde::{Deserialize, Serialize};

use super::model::read_json;
#[cfg(any(windows, test))]
use super::model::save_json;

pub(super) const REPOSITORY: Option<&str> = option_env!("SCANNER_WORKBENCH_UPDATE_REPOSITORY");
const BUNDLED: &str = include_str!("../../data/scanner/releases.json");
const CACHE: &str = "release-notes.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Release {
    pub version: String,
    #[serde(default)]
    pub published_at: String,
    pub notes: String,
}

impl Release {
    pub(super) fn date_label(&self) -> Option<String> {
        chrono::DateTime::parse_from_rfc3339(&self.published_at)
            .ok()
            .map(|date| date.format("%B %-d, %Y").to_string())
    }

    pub(super) fn url(&self) -> String {
        format!("{}/tag/workbench-v{}", releases_url(), self.version)
    }
}

pub(super) fn releases_url() -> String {
    format!(
        "https://github.com/{}/releases",
        REPOSITORY.unwrap_or("Supersam902/scanner-workbench")
    )
}

fn version_key(version: &str) -> Option<[u64; 3]> {
    let mut parts = version.split('.');
    let mut key = [0; 3];
    for number in &mut key {
        let part = parts.next()?;
        *number = part.parse().ok()?;
        if number.to_string() != part {
            return None;
        }
    }
    parts.next().is_none().then_some(key)
}

fn merge(entries: impl IntoIterator<Item = Release>) -> Vec<Release> {
    let mut versions = BTreeMap::new();
    for entry in entries {
        if let Some(version) = version_key(&entry.version)
            && !entry.notes.trim().is_empty()
        {
            versions.insert(version, entry);
        }
    }
    versions.into_values().rev().collect()
}

struct Loaded {
    entries: Vec<Release>,
    warning: Option<String>,
}

pub(super) struct History {
    entries: Vec<Release>,
    events: Option<Receiver<Result<Loaded, String>>>,
    #[cfg(any(windows, test))]
    cache: std::path::PathBuf,
    #[cfg(windows)]
    requested: bool,
    pub query: String,
    pub error: Option<String>,
}

impl History {
    pub(super) fn new(root: &Path) -> Self {
        let bundled: Vec<Release> = serde_json::from_str(BUNDLED).expect("bundled release notes");
        let cached: Vec<Release> = read_json(&root.join(CACHE)).unwrap_or_default();
        Self {
            entries: merge(bundled.into_iter().chain(cached)),
            events: None,
            #[cfg(any(windows, test))]
            cache: root.join(CACHE),
            #[cfg(windows)]
            requested: cfg!(test),
            query: String::new(),
            error: None,
        }
    }

    pub(super) fn entries(&self) -> &[Release] {
        &self.entries
    }

    #[cfg(windows)]
    pub(super) fn load_latest(&mut self) {
        if !self.requested {
            self.refresh();
        }
    }

    #[cfg(windows)]
    pub(super) fn refresh(&mut self) {
        self.requested = true;
        let Some(repository) = REPOSITORY else {
            self.error = Some("Showing saved patch notes. No release feed is configured.".into());
            return;
        };
        self.refresh_with(move || fetch_github(repository));
    }

    #[cfg(any(windows, test))]
    fn refresh_with(
        &mut self,
        fetch: impl FnOnce() -> Result<Vec<Release>, String> + Send + 'static,
    ) {
        if self.loading() {
            return;
        }
        self.error = None;
        let (tx, rx) = mpsc::channel();
        self.events = Some(rx);
        let cache = self.cache.clone();
        let saved = self.entries.clone();
        std::thread::spawn(move || {
            let result = fetch().map(|latest| {
                let entries = merge(saved.into_iter().chain(latest));
                let warning = save_json(&cache, &entries).err().map(|error| {
                    format!("Patch notes loaded but could not be saved for offline use. {error}")
                });
                Loaded { entries, warning }
            });
            let _ = tx.send(result);
        });
    }

    #[cfg(any(windows, test))]
    pub(super) fn loading(&self) -> bool {
        self.events.is_some()
    }

    pub(super) fn tick(&mut self) {
        let Some(events) = &self.events else {
            return;
        };
        let result = match events.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("Patch-note refresh ended early.".into()),
        };
        self.events = None;
        match result {
            Ok(Loaded { entries, warning }) => {
                self.entries = entries;
                self.error = warning;
            }
            Err(error) => self.error = Some(format!("Showing saved patch notes. {error}")),
        }
    }
}

#[cfg(any(windows, test))]
#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    body: Option<String>,
    published_at: Option<String>,
    draft: bool,
    prerelease: bool,
}

#[cfg(any(windows, test))]
fn fetch_pages(
    mut read_page: impl FnMut(u32) -> Result<String, String>,
) -> Result<Vec<Release>, String> {
    let mut releases = Vec::new();
    for page in 1..=100 {
        let json = read_page(page)?;
        let entries: Vec<GithubRelease> = serde_json::from_str(&json)
            .map_err(|error| format!("Could not read the release history. {error}"))?;
        let finished = entries.len() < 100;
        for entry in entries {
            if entry.draft || entry.prerelease {
                continue;
            }
            let Some(version) = entry.tag_name.strip_prefix("workbench-v") else {
                continue;
            };
            if version_key(version).is_none() {
                continue;
            }
            let notes = entry
                .body
                .filter(|body| !body.trim().is_empty())
                .unwrap_or_else(|| "No patch notes were published for this version.".into());
            releases.push(Release {
                version: version.to_owned(),
                published_at: entry.published_at.unwrap_or_default(),
                notes,
            });
        }
        if finished {
            return Ok(merge(releases));
        }
    }
    Err("The release history did not finish loading. Try refreshing again.".into())
}

#[cfg(windows)]
fn fetch_github(repository: &str) -> Result<Vec<Release>, String> {
    use velopack::download::{HttpHeader, HttpOptions, download_url_as_string};
    let options = HttpOptions {
        Headers: vec![
            HttpHeader {
                Name: "User-Agent".into(),
                Value: format!("Scanner-Workbench/{}", super::VERSION),
            },
            HttpHeader {
                Name: "Accept".into(),
                Value: "application/vnd.github+json".into(),
            },
            HttpHeader {
                Name: "X-GitHub-Api-Version".into(),
                Value: "2022-11-28".into(),
            },
        ],
        TimeoutMilliseconds: 15_000,
    };
    fetch_pages(|page| {
        let url =
            format!("https://api.github.com/repos/{repository}/releases?per_page=100&page={page}");
        download_url_as_string(&url, Some(&options))
            .map_err(|error| format!("Could not refresh patch notes. {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(version: &str, notes: &str) -> Release {
        Release {
            version: version.into(),
            published_at: String::new(),
            notes: notes.into(),
        }
    }

    #[test]
    fn cached_history_keeps_bundled_versions_and_sorts_numerically() {
        let folder = tempfile::tempdir().unwrap();
        save_json(
            &folder.path().join(CACHE),
            &vec![
                release("0.1.9", "Earlier build"),
                release("0.1.100", "Later build"),
                release("0.1.21", "Corrected release notes"),
                release("../0.1.1", "Invalid tag"),
            ],
        )
        .unwrap();
        let history = History::new(folder.path());
        assert_eq!(history.entries[0].version, "0.1.100");
        assert!(history.entries.iter().any(|entry| entry.version == "0.1.1"));
        assert!(
            history
                .entries
                .iter()
                .any(|entry| entry.version == "0.1.21" && entry.notes == "Corrected release notes")
        );
        assert_eq!(
            history
                .entries
                .iter()
                .filter(|entry| entry.version == "0.1.21")
                .count(),
            1
        );
        assert!(
            history
                .entries
                .iter()
                .all(|entry| !entry.version.contains('/'))
        );
        std::fs::write(folder.path().join(CACHE), b"interrupted cache").unwrap();
        assert!(!History::new(folder.path()).entries.is_empty());
    }

    #[test]
    fn release_history_follows_all_pages_and_excludes_other_apps_and_unpublished_builds() {
        let entry = |tag: String, body: Option<&str>, draft: bool, prerelease: bool| {
            serde_json::json!({
                "tag_name": tag, "body": body, "draft": draft, "prerelease": prerelease, "published_at": "2026-11-17T19:40:20Z"
            })
        };
        let first: Vec<_> = (0..100)
            .map(|patch| {
                entry(
                    format!("workbench-v1.0.{patch}"),
                    Some("Changes"),
                    false,
                    false,
                )
            })
            .collect();
        let second = vec![
            entry("workbench-v0.1.1".into(), None, false, false),
            entry("v9.0.0".into(), Some("Another app"), false, false),
            entry("workbench-v9.0.0".into(), Some("Draft"), true, false),
            entry("workbench-v8.0.0".into(), Some("Preview"), false, true),
        ];
        let mut pages = Vec::new();
        let releases = fetch_pages(|page| {
            pages.push(page);
            Ok(serde_json::to_string(if page == 1 { &first } else { &second }).unwrap())
        })
        .unwrap();
        assert_eq!(pages, vec![1, 2]);
        assert_eq!(releases.len(), 101);
        assert_eq!(releases[0].version, "1.0.99");
        assert_eq!(
            releases.last().unwrap().notes,
            "No patch notes were published for this version."
        );
        assert_eq!(
            releases[0].date_label().as_deref(),
            Some("November 17, 2026")
        );
    }

    fn finish(history: &mut History) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while history.loading() && std::time::Instant::now() < deadline {
            history.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!history.loading());
    }

    #[test]
    fn refresh_saves_new_notes_and_preserves_history_on_network_and_cache_errors() {
        let folder = tempfile::tempdir().unwrap();
        let mut history = History::new(folder.path());
        history.refresh_with(|| Ok(vec![release("1.0.0", "New changes")]));
        history.refresh_with(|| panic!("a refresh is already running"));
        finish(&mut history);
        assert!(history.error.is_none());
        assert_eq!(History::new(folder.path()).entries[0].version, "1.0.0");
        history.refresh_with(|| Err("Offline".into()));
        finish(&mut history);
        assert_eq!(history.entries[0].version, "1.0.0");
        assert!(
            history
                .error
                .as_deref()
                .unwrap()
                .contains("Showing saved patch notes. Offline")
        );
        history.cache = folder.path().join("blocked/cache.json");
        std::fs::write(folder.path().join("blocked"), b"not a directory").unwrap();
        history.refresh_with(|| Ok(vec![release("1.1.0", "Still readable")]));
        finish(&mut history);
        assert_eq!(history.entries[0].version, "1.1.0");
        assert!(
            history
                .error
                .as_deref()
                .unwrap()
                .contains("could not be saved for offline use")
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "contacts the public GitHub release feed"]
    fn public_github_history_loads_through_the_production_http_client() {
        let releases = fetch_github("Supersam902/scanner-workbench").unwrap();
        assert!(releases.iter().any(|entry| entry.version == "0.1.1"));
        assert!(
            releases
                .iter()
                .any(|entry| entry.version == "0.1.21" && entry.notes.contains("Save report"))
        );
    }
}
