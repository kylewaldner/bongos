//! Contains helper functions for constructing URLs to Zed's public docs and blog.
//!
//! None of these read the configured server URL, so no link can open the
//! placeholder host.

use gpui::App;
use release_channel::ReleaseChannel;

fn docs_url(cx: &App) -> String {
    let site = "https://zed.dev";
    match ReleaseChannel::try_global(cx).unwrap_or_default() {
        ReleaseChannel::Stable => {
            format!("{site}/docs")
        }
        ReleaseChannel::Preview => {
            format!("{site}/docs/preview")
        }
        ReleaseChannel::Dev | ReleaseChannel::Nightly => {
            format!("{site}/docs/nightly")
        }
    }
}

pub fn skills_docs(cx: &App) -> String {
    format!("{docs_url}/ai/skills", docs_url = docs_url(cx))
}

/// Returns the URL to Zed's ACP registry blog post.
pub fn acp_registry_blog(_cx: &App) -> String {
    "https://zed.dev/blog/acp-registry".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn test_doc_links_point_at_the_public_site(cx: &mut TestAppContext) {
        cx.update(|cx| {
            for url in [skills_docs(cx), acp_registry_blog(cx)] {
                assert!(url.starts_with("https://zed.dev/"), "{url}");
            }
        });
    }
}
