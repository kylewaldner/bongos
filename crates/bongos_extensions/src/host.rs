//! What the installer needs from the extension host: compiling a source folder, linking a built
//! folder, the installed ids and the newest extension API this build loads. [`ExtensionStoreHost`]
//! goes through the global `ExtensionStore`; tests substitute [`FakeHost`].

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use bongos_extensions_model::ApiVersion;
use extension::ExtensionManifest;
use extension::extension_builder::CompileExtensionOptions;
use extension_host::ExtensionStore;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::{App, Task};

/// An extension the host has in its index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledExtension {
    pub id: String,
    /// Whether it is linked from a source folder (dev extensions and git installs).
    pub dev: bool,
}

/// The extension host as the installer sees it.
pub trait HostApi: 'static {
    /// The extensions in the host's index.
    fn installed(&self, cx: &App) -> Vec<InstalledExtension>;
    /// The newest extension API version this build loads.
    fn api_max(&self, cx: &App) -> ApiVersion;
    /// The manifest schema versions this build loads.
    fn schema_version_range(&self) -> RangeInclusive<u32>;
    /// Whether the id must never be installed.
    fn is_suppressed(&self, id: &str) -> bool;
    /// Whether the WASI SDK the builder needs for grammars is already downloaded.
    fn wasi_sdk_present(&self, cx: &App) -> bool;
    /// Compiles the extension in `dir`. The future does the work and is meant for the background
    /// executor. It yields the extension API version of the built wasm, or `None` when the
    /// extension has no Rust part.
    fn compile(&self, dir: PathBuf, cx: &App) -> BoxFuture<'static, Result<Option<ApiVersion>>>;
    /// Points `installed/<id>` at the built folder `dir` and reloads.
    fn link(&self, id: &str, dir: PathBuf, cx: &mut App) -> Task<Result<()>>;
    /// Removes `installed/<id>` (only the link, never the folder it points at) and reloads.
    fn unlink(&self, id: &str, cx: &mut App) -> Task<Result<()>>;
}

/// The API version of a semver version.
pub fn api_version(version: &semver::Version) -> ApiVersion {
    ApiVersion::new(
        u32::try_from(version.major).unwrap_or(u32::MAX),
        u32::try_from(version.minor).unwrap_or(u32::MAX),
        u32::try_from(version.patch).unwrap_or(u32::MAX),
    )
}

/// Reads the extension API version from `<dir>/extension.wasm`, or `None` when there is no wasm.
pub fn built_api_version(id: &str, dir: &Path) -> Result<Option<ApiVersion>> {
    let wasm = dir.join("extension.wasm");
    let bytes = match std::fs::read(&wasm) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("could not read {}", wasm.display())),
    };
    let version = extension::parse_wasm_extension_version(id, &bytes)
        .with_context(|| format!("could not read the extension API version of {id}"))?;
    Ok(Some(api_version(&version)))
}

/// The real host: the global `ExtensionStore`.
pub struct ExtensionStoreHost;

impl HostApi for ExtensionStoreHost {
    fn installed(&self, cx: &App) -> Vec<InstalledExtension> {
        ExtensionStore::global(cx)
            .read(cx)
            .installed_extensions()
            .iter()
            .map(|(id, entry)| InstalledExtension {
                id: id.to_string(),
                dev: entry.dev,
            })
            .collect()
    }

    fn api_max(&self, cx: &App) -> ApiVersion {
        api_version(&extension_host::supported_api_max(cx))
    }

    fn schema_version_range(&self) -> RangeInclusive<u32> {
        let range = extension_host::schema_version_range();
        let start = u32::try_from(range.start().0).unwrap_or(0);
        let end = u32::try_from(range.end().0).unwrap_or(0);
        start..=end
    }

    fn is_suppressed(&self, id: &str) -> bool {
        extension_host::is_suppressed_extension(id)
    }

    fn wasi_sdk_present(&self, cx: &App) -> bool {
        let store = ExtensionStore::global(cx);
        let Some(root) = store.read(cx).installed_dir.parent().map(Path::to_path_buf) else {
            return false;
        };
        root.join("build").join("wasi-sdk").join("bin").is_dir()
    }

    fn compile(&self, dir: PathBuf, cx: &App) -> BoxFuture<'static, Result<Option<ApiVersion>>> {
        let store = ExtensionStore::global(cx).read(cx);
        let builder = store.builder.clone();
        let fs = store.fs.clone();
        async move {
            let mut manifest = ExtensionManifest::load(fs.clone(), &dir).await?;
            let id = manifest.id.clone();
            builder
                .compile_extension(&dir, &mut manifest, CompileExtensionOptions::dev(), fs)
                .await?;
            built_api_version(&id, &dir)
        }
        .boxed()
    }

    fn link(&self, id: &str, dir: PathBuf, cx: &mut App) -> Task<Result<()>> {
        let id: Arc<str> = id.into();
        ExtensionStore::global(cx).update(cx, |store, cx| store.link_extension(id, dir, cx))
    }

    fn unlink(&self, id: &str, cx: &mut App) -> Task<Result<()>> {
        let id: Arc<str> = id.into();
        ExtensionStore::global(cx).update(cx, |store, cx| store.uninstall_extension(id, cx))
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use fake::{FakeBuild, FakeHost};

#[cfg(any(test, feature = "test-support"))]
mod fake {
    use std::collections::HashMap;
    use std::ops::RangeInclusive;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{Result, anyhow};
    use bongos_extensions_model::{ApiVersion, Layout};
    use futures::FutureExt as _;
    use futures::channel::oneshot;
    use futures::future::BoxFuture;
    use gpui::{App, Task};
    use parking_lot::Mutex;

    use super::{HostApi, InstalledExtension};

    /// What one fake build does.
    pub enum FakeBuild {
        /// Succeeds with this API version (`None`: no Rust part).
        Ok(Option<ApiVersion>),
        /// Fails with this message.
        Fail(String),
        /// Waits for the sender to send the outcome.
        Pending(oneshot::Receiver<Result<Option<ApiVersion>, String>>),
    }

    #[derive(Default)]
    struct State {
        installed: Vec<InstalledExtension>,
        builds: Vec<FakeBuild>,
        compiled: Vec<PathBuf>,
        linked: Vec<(String, PathBuf)>,
        link_failures: HashMap<String, String>,
        wasi_sdk_present: bool,
    }

    /// A host that never compiles: builds follow a script and links are real symlinks under the
    /// layout's `installed/`.
    pub struct FakeHost {
        layout: Layout,
        api_max: ApiVersion,
        state: Mutex<State>,
        running: Arc<AtomicUsize>,
        max_running: Arc<AtomicUsize>,
    }

    impl FakeHost {
        pub fn new(layout: Layout, api_max: ApiVersion) -> Self {
            Self {
                layout,
                api_max,
                state: Mutex::default(),
                running: Arc::default(),
                max_running: Arc::default(),
            }
        }

        /// Queues the outcome of the next build. With nothing queued a build succeeds with no API
        /// version.
        pub fn push_build(&self, build: FakeBuild) {
            self.state.lock().builds.push(build);
        }

        /// Makes linking `id` fail with `message`.
        pub fn fail_link(&self, id: &str, message: &str) {
            self.state
                .lock()
                .link_failures
                .insert(id.to_string(), message.to_string());
        }

        pub fn add_installed(&self, id: &str, dev: bool) {
            self.state.lock().installed.push(InstalledExtension {
                id: id.to_string(),
                dev,
            });
        }

        pub fn set_wasi_sdk_present(&self, present: bool) {
            self.state.lock().wasi_sdk_present = present;
        }

        /// The folders compiled so far.
        pub fn compiled(&self) -> Vec<PathBuf> {
            self.state.lock().compiled.clone()
        }

        /// The links made so far.
        pub fn linked(&self) -> Vec<(String, PathBuf)> {
            self.state.lock().linked.clone()
        }

        /// The most builds that ran at once.
        pub fn max_concurrent_builds(&self) -> usize {
            self.max_running.load(Ordering::SeqCst)
        }
    }

    struct Running(Arc<AtomicUsize>);

    impl Drop for Running {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl HostApi for FakeHost {
        fn installed(&self, _: &App) -> Vec<InstalledExtension> {
            self.state.lock().installed.clone()
        }

        fn api_max(&self, _: &App) -> ApiVersion {
            self.api_max
        }

        fn schema_version_range(&self) -> RangeInclusive<u32> {
            0..=1
        }

        fn is_suppressed(&self, id: &str) -> bool {
            id == "snippets"
        }

        fn wasi_sdk_present(&self, _: &App) -> bool {
            self.state.lock().wasi_sdk_present
        }

        fn compile(&self, dir: PathBuf, _: &App) -> BoxFuture<'static, Result<Option<ApiVersion>>> {
            let build = {
                let mut state = self.state.lock();
                state.compiled.push(dir);
                if state.builds.is_empty() {
                    FakeBuild::Ok(None)
                } else {
                    state.builds.remove(0)
                }
            };
            let running = self.running.clone();
            let max_running = self.max_running.clone();
            async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                max_running.fetch_max(now, Ordering::SeqCst);
                let _running = Running(running);
                match build {
                    FakeBuild::Ok(version) => Ok(version),
                    FakeBuild::Fail(message) => Err(anyhow!(message)),
                    FakeBuild::Pending(receiver) => match receiver.await {
                        Ok(outcome) => outcome.map_err(|message| anyhow!(message)),
                        Err(_) => Err(anyhow!("the build was dropped")),
                    },
                }
            }
            .boxed()
        }

        fn link(&self, id: &str, dir: PathBuf, _: &mut App) -> Task<Result<()>> {
            let mut state = self.state.lock();
            if let Some(message) = state.link_failures.get(id) {
                return Task::ready(Err(anyhow!(message.clone())));
            }
            let link = self.layout.symlink_path(id);
            let result = (|| {
                std::fs::create_dir_all(self.layout.installed_dir())?;
                if std::fs::symlink_metadata(&link).is_ok() {
                    std::fs::remove_file(&link)?;
                }
                std::os::unix::fs::symlink(&dir, &link)
            })();
            if let Err(err) = result {
                return Task::ready(Err(err.into()));
            }
            state.linked.push((id.to_string(), dir));
            if !state.installed.iter().any(|e| e.id == id) {
                state.installed.push(InstalledExtension {
                    id: id.to_string(),
                    dev: true,
                });
            }
            Task::ready(Ok(()))
        }

        fn unlink(&self, id: &str, _: &mut App) -> Task<Result<()>> {
            let link = self.layout.symlink_path(id);
            std::fs::remove_file(&link).ok();
            self.state.lock().installed.retain(|e| e.id != id);
            Task::ready(Ok(()))
        }
    }
}
