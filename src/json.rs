//! `--json`: `jlo list` and `jlo current` as one JSON document each. Machine
//! output, like the export lines - kept out of `ui`, which speaks to people.
//!
//! The wire structs are separate from the domain types on purpose: renaming a
//! domain type must not move the contract. The contract only grows: a key is
//! never removed, renamed or retyped, and every key is always present, `null`
//! when unknown.

use crate::adoptium::Catalogue;
use crate::conf::{Resolved, Source};
use crate::request::Request;
use crate::resolve::Active;
use crate::shellenv::path_str;
use crate::store::{InstalledJdk, JdkStore, NameGroup, Status, supersedes_every_install};
use anyhow::Context;
use serde::Serialize;
use std::path::Path;

/// Every JDK jlo manages is Temurin; stated, so a consumer need not assume it.
const DISTRIBUTION: &str = "temurin";

#[derive(Serialize)]
struct ListDoc<'a> {
    names: Vec<NameEntry<'a>>,
}

#[derive(Serialize)]
struct NameEntry<'a> {
    name: String,
    major: i64,
    distribution: &'static str,
    /// Newest first: the first is the build the name reports.
    installed: Vec<Build>,
    offered: Option<&'a str>,
    /// `None` when the name was not looked up: unknown, not "no".
    update: Option<bool>,
}

#[derive(Serialize)]
struct Build {
    version: String,
    path: String,
    active: bool,
    status: BuildStatus,
}

/// Separate from `active`: a shell still on a superseded build is both.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum BuildStatus {
    Installed,
    Superseded,
}

/// `jlo list --json`. `catalogue` is `None` under `--offline`. Names in the
/// listing's order, installed and offered alike - the table's split between
/// them is layout.
pub(crate) fn list(
    catalogue: Option<&Catalogue>,
    groups: &[NameGroup],
    active_version: Option<&str>,
    store: &JdkStore,
) -> anyhow::Result<String> {
    let offers = catalogue.map(|c| c.jdks.as_slice()).unwrap_or_default();
    let mut names: Vec<Request> = offers
        .iter()
        .map(|jdk| jdk.request)
        .chain(groups.iter().map(|group| group.request))
        .collect();
    names.sort_unstable_by_key(|name| name.listing_order());
    names.dedup();

    let names = names
        .into_iter()
        .map(|name| {
            let group = groups.iter().find(|group| group.request == name);
            let offered = offers
                .iter()
                .find(|jdk| jdk.request == name)
                .map(|jdk| jdk.version.as_str());
            let builds = group.map(NameGroup::builds).unwrap_or_default();
            // The one rule the table's `update` and `jlo update` use.
            let update = catalogue.filter(|c| c.asked.contains(&name)).map(|_| {
                !builds.is_empty() && offered.is_some_and(|v| supersedes_every_install(v, &builds))
            });
            Ok(NameEntry {
                name: name.to_string(),
                major: name.major,
                distribution: DISTRIBUTION,
                installed: installed(group, active_version, store)?,
                offered,
                update,
            })
        })
        .collect::<anyhow::Result<_>>()?;

    Ok(serde_json::to_string(&ListDoc { names })?)
}

fn installed(
    group: Option<&NameGroup>,
    active_version: Option<&str>,
    store: &JdkStore,
) -> anyhow::Result<Vec<Build>> {
    let Some(group) = group else {
        return Ok(Vec::new());
    };
    std::iter::once((group.head, Status::Installed))
        .chain(group.others.iter().copied())
        .map(|(jdk, status)| build(jdk, status, active_version, store))
        .collect()
}

/// A path jlo worked out (a store entry, a config file), as the document
/// promises it: absolute even under a relative `HOME`, which
/// `JdkStore::discover` accepts. Lexical, like the shell's `$PWD/...`: no
/// symlink is resolved. `current.path` is not one of these - it is reported
/// as the shell set it.
fn computed_path(path: &Path) -> anyhow::Result<String> {
    let absolute = std::path::absolute(path)
        .with_context(|| format!("could not make {} absolute", path.display()))?;
    Ok(path_str(&absolute)?.to_string())
}

fn build(
    jdk: &InstalledJdk,
    status: Status,
    active_version: Option<&str>,
    store: &JdkStore,
) -> anyhow::Result<Build> {
    Ok(Build {
        version: jdk.version.clone(),
        path: computed_path(&store.java_home(jdk))?,
        active: active_version == Some(jdk.version.as_str()),
        status: match status {
            Status::Installed => BuildStatus::Installed,
            Status::Superseded => BuildStatus::Superseded,
        },
    })
}

#[derive(Serialize)]
struct CurrentDoc {
    name: Option<String>,
    major: Option<i64>,
    distribution: Option<&'static str>,
    version: Option<String>,
    /// `$JAVA_HOME` as the shell spells it: not normalized.
    path: String,
    source: Option<SourceDoc>,
    pinned_elsewhere: Option<Pinned>,
}

/// The variant tags are `conf::Source`'s, committed as wire names.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SourceDoc {
    ProjectConfig { path: String },
    DefaultConfig { path: String },
    NewestInstalled,
    Foreign,
}

#[derive(Serialize)]
struct Pinned {
    name: String,
    major: i64,
    distribution: &'static str,
    source: SourceDoc,
}

/// `jlo current --json`.
pub(crate) fn current(active: &Active) -> anyhow::Result<String> {
    let doc = CurrentDoc {
        name: active.request.map(|request| request.to_string()),
        major: active.request.map(|request| request.major),
        distribution: active.request.map(|_| DISTRIBUTION),
        version: active.version.clone(),
        path: path_str(&active.path)?.to_string(),
        source: active.source.as_ref().map(source).transpose()?,
        pinned_elsewhere: active.pinned_elsewhere.as_ref().map(pinned).transpose()?,
    };
    Ok(serde_json::to_string(&doc)?)
}

fn source(source: &Source) -> anyhow::Result<SourceDoc> {
    Ok(match source {
        Source::ProjectConfig(path) => SourceDoc::ProjectConfig {
            path: computed_path(path)?,
        },
        Source::DefaultConfig(path) => SourceDoc::DefaultConfig {
            path: computed_path(path)?,
        },
        Source::NewestInstalled => SourceDoc::NewestInstalled,
        Source::Foreign => SourceDoc::Foreign,
    })
}

fn pinned(pinned: &Resolved) -> anyhow::Result<Pinned> {
    Ok(Pinned {
        name: pinned.request.to_string(),
        major: pinned.request.major,
        distribution: DISTRIBUTION,
        source: source(&pinned.source)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adoptium::RemoteJdk;
    use crate::conf::{Resolved, Source};
    use crate::request::request;
    use crate::resolve::Active;
    use crate::store::{entry_name, group_by_name};
    use serde_json::{Value, json};
    use std::path::Path;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn store_holding(base: &Path, versions: &[&str]) -> JdkStore {
        for version in versions {
            let entry = base.join(entry_name(version));
            std::fs::create_dir_all(entry.join("bin")).unwrap();
            std::fs::write(entry.join("bin").join("java"), "").unwrap();
        }
        JdkStore::at(base)
    }

    fn home(base: &Path, version: &str) -> String {
        base.join(entry_name(version)).to_str().unwrap().to_string()
    }

    fn offer(version: &str, name: &str) -> RemoteJdk {
        RemoteJdk {
            version: version.to_string(),
            request: request(name),
        }
    }

    fn list_doc(catalogue: Option<&Catalogue>, store: &JdkStore, active: Option<&str>) -> Value {
        let installed = store.list().unwrap();
        let groups = group_by_name(&installed);
        serde_json::from_str(&list(catalogue, &groups, active, store).unwrap()).unwrap()
    }

    /// Every row kind a consumer must tell apart, against literal keys, so a
    /// renamed wire field fails here.
    #[test]
    fn list_tells_offered_unknown_and_absent_apart() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        let store = store_holding(
            base,
            &[
                "25.0.1+8",
                "25.0.0+36",
                "25.0.2-beta+3",
                "21.0.9+10",
                "16.0.2+7",
            ],
        );
        let catalogue = Catalogue {
            jdks: vec![
                offer("28.0.0-beta+5", "28-ea"),
                offer("25.0.2+10", "25"),
                offer("21.0.8+9", "21"),
            ],
            // 25-ea is not asked: its major has shipped. 17 is asked and has
            // no build, and is not installed, so it is not listed.
            asked: vec![
                request("28-ea"),
                request("25"),
                request("21"),
                request("17"),
                request("16"),
            ],
            failed: vec![],
        };

        let doc = list_doc(Some(&catalogue), &store, Some("25.0.0+36"));

        assert_eq!(
            doc,
            json!({"names": [
                {"name": "28-ea", "major": 28, "distribution": "temurin",
                 "installed": [], "offered": "28.0.0-beta+5", "update": false},
                {"name": "25", "major": 25, "distribution": "temurin",
                 "installed": [
                    {"version": "25.0.1+8", "path": home(base, "25.0.1+8"),
                     "active": false, "status": "installed"},
                    // The shell is still on the old build: both facts at once.
                    {"version": "25.0.0+36", "path": home(base, "25.0.0+36"),
                     "active": true, "status": "superseded"}],
                 "offered": "25.0.2+10", "update": true},
                // Not covered by the catalogue: unknown, not "no".
                {"name": "25-ea", "major": 25, "distribution": "temurin",
                 "installed": [
                    {"version": "25.0.2-beta+3", "path": home(base, "25.0.2-beta+3"),
                     "active": false, "status": "installed"}],
                 "offered": null, "update": null},
                // The catalogue behind the store: offered, never an update.
                {"name": "21", "major": 21, "distribution": "temurin",
                 "installed": [
                    {"version": "21.0.9+10", "path": home(base, "21.0.9+10"),
                     "active": false, "status": "installed"}],
                 "offered": "21.0.8+9", "update": false},
                // Asked, no build for this platform.
                {"name": "16", "major": 16, "distribution": "temurin",
                 "installed": [
                    {"version": "16.0.2+7", "path": home(base, "16.0.2+7"),
                     "active": false, "status": "installed"}],
                 "offered": null, "update": false}
            ]})
        );
    }

    /// A macOS bundle's `JAVA_HOME` is `Contents/Home`, not the entry: the
    /// path must be one a consumer can export as is.
    #[test]
    fn list_path_is_the_java_home_inside_a_bundle() {
        let dir = tempdir().unwrap();
        let entry = dir.path().join(entry_name("21.0.9+10"));
        let bundled = entry.join("Contents").join("Home");
        std::fs::create_dir_all(bundled.join("bin")).unwrap();
        std::fs::write(bundled.join("bin").join("java"), "").unwrap();
        let store = JdkStore::at(dir.path());

        let doc = list_doc(None, &store, None);

        assert_eq!(
            doc["names"][0]["installed"][0]["path"],
            bundled.to_str().unwrap()
        );
    }

    #[test]
    fn list_offline_knows_no_offer() {
        let dir = tempdir().unwrap();
        let store = store_holding(dir.path(), &["21.0.9+10"]);

        let doc = list_doc(None, &store, None);

        assert_eq!(doc["names"][0]["offered"], Value::Null);
        assert_eq!(doc["names"][0]["update"], Value::Null);
    }

    fn current_doc(active: &Active) -> Value {
        serde_json::from_str(&current(active).unwrap()).unwrap()
    }

    fn jlos(source: Option<Source>, pinned: Option<Resolved>) -> Active {
        Active {
            path: PathBuf::from("/s/jlo-temurin-21.0.5+11"),
            version: Some("21.0.5+11".to_string()),
            request: Some(request("21")),
            source,
            pinned_elsewhere: pinned,
        }
    }

    /// `source.path` exists for the two config kinds and for no other.
    #[test]
    fn current_names_each_source_kind() {
        let cases = [
            (
                Source::ProjectConfig(PathBuf::from("/p/.jlorc")),
                json!({"kind": "project_config", "path": "/p/.jlorc"}),
            ),
            (
                Source::DefaultConfig(PathBuf::from("/h/.jlo/default.jlorc")),
                json!({"kind": "default_config", "path": "/h/.jlo/default.jlorc"}),
            ),
            (Source::NewestInstalled, json!({"kind": "newest_installed"})),
        ];
        for (source, expected) in cases {
            assert_eq!(current_doc(&jlos(Some(source), None))["source"], expected);
        }
    }

    #[test]
    fn current_reports_a_jlo_jdk_and_the_pin_it_disagrees_with() {
        let pinned = Resolved {
            request: request("25"),
            source: Source::ProjectConfig(PathBuf::from("/p/.jlorc")),
        };
        assert_eq!(
            current_doc(&jlos(None, Some(pinned))),
            json!({
                "name": "21", "major": 21, "distribution": "temurin",
                "version": "21.0.5+11", "path": "/s/jlo-temurin-21.0.5+11",
                "source": null,
                "pinned_elsewhere": {
                    "name": "25", "major": 25, "distribution": "temurin",
                    "source": {"kind": "project_config", "path": "/p/.jlorc"}
                }
            })
        );
    }

    /// Not jlo's: what jlo cannot know is `null`, and the path is reported
    /// exactly as the shell set it.
    #[test]
    fn current_reports_a_foreign_jdk_verbatim() {
        let active = Active {
            path: PathBuf::from("jdk/../jdk"),
            version: None,
            request: None,
            source: Some(Source::Foreign),
            pinned_elsewhere: None,
        };
        assert_eq!(
            current_doc(&active),
            json!({
                "name": null, "major": null, "distribution": null,
                "version": null, "path": "jdk/../jdk",
                "source": {"kind": "foreign"}, "pinned_elsewhere": null
            })
        );
    }
}
