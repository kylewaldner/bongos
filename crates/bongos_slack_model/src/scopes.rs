//! The scopes the Slack app asks for, and the request text for a workspace admin.

/// Every scope bongos needs: read history and names of every conversation kind, and post.
/// No admin scope is asked for.
pub const SCOPES: &[&str] = &[
    "channels:history",
    "groups:history",
    "im:history",
    "mpim:history",
    "channels:read",
    "groups:read",
    "im:read",
    "mpim:read",
    "users:read",
    "chat:write",
];

/// The scopes of [`SCOPES`] that `granted` lacks, in [`SCOPES`] order.
pub fn missing_scopes<S: AsRef<str>>(granted: &[S]) -> Vec<&'static str> {
    SCOPES
        .iter()
        .copied()
        .filter(|scope| !granted.iter().any(|granted| granted.as_ref() == *scope))
        .collect()
}

/// The text to send a workspace admin who must approve the app.
pub fn admin_request_text() -> String {
    format!(
        "Please approve the bongos Slack app for this workspace. bongos is a code editor; the app \
         lets me read and post Slack messages from it as myself. It asks to read history, read \
         channel and user names, and post messages ({}). It asks for no admin scope.",
        SCOPES.join(", ")
    )
}
