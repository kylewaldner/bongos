//! Static provenance for bongos: its own version and the Zed release it was forked from.
//! Nothing here is compared with anything remote.

/// The bongos version. The app's `AppVersion` stays at the Zed base version on purpose.
pub const VERSION: &str = "0.1.0";
/// The Zed tag bongos was forked from.
pub const ZED_TAG: &str = "v1.9.0";
/// The full Zed commit bongos was forked from.
pub const ZED_COMMIT: &str = "ced90fc636c4ede05402befc38a63bae7fd741bd";

/// The first seven characters of [`ZED_COMMIT`].
pub fn zed_commit_short() -> &'static str {
    &ZED_COMMIT[..7]
}

/// The line printed by `bongos --version`.
pub fn version_line() -> String {
    format!(
        "bongos {VERSION} (forked from Zed {ZED_TAG}, {})",
        zed_commit_short()
    )
}

/// The HTTP user agent, `bongos/<version> (<os>; <arch>)`.
pub fn user_agent(os: &str, arch: &str) -> String {
    format!("bongos/{VERSION} ({os}; {arch})")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin_field(origin: &str, key: &str) -> String {
        origin
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap_or_else(|| panic!("ORIGIN.md has no `{key}` line"))
            .trim()
            .to_string()
    }

    #[test]
    fn version_line_format() {
        assert_eq!(
            version_line(),
            "bongos 0.1.0 (forked from Zed v1.9.0, ced90fc)"
        );
    }

    #[test]
    fn user_agent_format() {
        assert_eq!(
            user_agent("linux", "x86_64"),
            "bongos/0.1.0 (linux; x86_64)"
        );
    }

    #[test]
    fn constants_match_origin_md() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ORIGIN.md");
        let origin = std::fs::read_to_string(path).expect("ORIGIN.md should be readable");
        assert_eq!(origin_field(&origin, "Zed tag:"), ZED_TAG);
        assert_eq!(origin_field(&origin, "Zed commit:"), ZED_COMMIT);
    }
}
