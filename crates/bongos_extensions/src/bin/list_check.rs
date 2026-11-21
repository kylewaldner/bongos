//! `bongos-extension-list-check [--scratch <dir>] [<list>]`: fetches, preflights and builds every
//! entry of the language extension list and checks its extension API version against the Stable
//! range. `<list>` is a path or `file://` URL of a list file; the embedded list by default. It
//! reaches the network, so a developer runs it by hand before a release
//! (`script/bongos/check-language-list`). Prints one line per entry and exits 1 when any fails.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use bongos_extensions::git::RealGit;
use bongos_extensions::host::{ExtensionStoreHost, HostApi as _};
use bongos_extensions::installer::RealTools;
use bongos_extensions::list_check::{
    CheckContext, RealListBuilder, check_list, load_list, stable_api_range,
};
use extension::extension_builder::ExtensionBuilder;
use fs::RealFs;

fn main() -> ExitCode {
    let mut scratch =
        std::env::temp_dir().join(format!("bongos-list-check-{}", std::process::id()));
    let mut list_location = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--scratch" => match args.next() {
                Some(dir) => scratch = PathBuf::from(dir),
                None => return usage(),
            },
            "-h" | "--help" => return usage(),
            _ if arg.starts_with('-') || list_location.is_some() => return usage(),
            _ => list_location = Some(arg),
        }
    }
    let list = match load_list(list_location.as_deref()) {
        Ok(list) => list,
        Err(err) => {
            eprintln!("bongos-extension-list-check: {err:#}");
            return ExitCode::from(2);
        }
    };
    if let Err(err) = std::fs::create_dir_all(&scratch) {
        eprintln!(
            "bongos-extension-list-check: could not create {}: {err}",
            scratch.display()
        );
        return ExitCode::from(2);
    }
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let result = failed.clone();
    gpui_platform::headless().run(move |cx| {
        let executor = cx.background_executor().clone();
        let fs = Arc::new(RealFs::new(None, executor.clone()));
        let http = Arc::new(reqwest_client::ReqwestClient::new());
        let builder = RealListBuilder {
            builder: ExtensionBuilder::new(http, scratch.join("build-cache")),
            fs,
        };
        cx.spawn(async move |cx| {
            let git = RealGit::new();
            let tools = RealTools(executor);
            let context = CheckContext {
                git: &git,
                tools: &tools,
                builder: &builder,
                scratch: &scratch,
                api_range: stable_api_range(),
                schema_version_range: ExtensionStoreHost.schema_version_range(),
                is_blocked_host: bongos_net::is_blocked_host,
                is_suppressed: extension_host::is_suppressed_extension,
            };
            for report in check_list(&list, &context).await {
                println!("{report}");
                if !report.passed() {
                    result.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            std::fs::remove_dir_all(&scratch).ok();
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    if failed.load(std::sync::atomic::Ordering::SeqCst) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: bongos-extension-list-check [--scratch <dir>] [<list file or file:// URL>]");
    ExitCode::from(2)
}
