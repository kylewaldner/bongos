use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, SecondsFormat, Utc};

/// Why a person approved a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Basis {
    Edit,
    Land,
    Approve,
    Grant,
    Adopt,
}

impl Basis {
    fn as_str(self) -> &'static str {
        match self {
            Basis::Edit => "edit",
            Basis::Land => "land",
            Basis::Approve => "approve",
            Basis::Grant => "grant",
            Basis::Adopt => "adopt",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "edit" => Basis::Edit,
            "land" => Basis::Land,
            "approve" => Basis::Approve,
            "grant" => Basis::Grant,
            "adopt" => Basis::Adopt,
            _ => return None,
        })
    }
}

/// How the signature was made.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    /// Signed on this machine with the key in the system keyring.
    Keyring,
    /// Signed on another machine that holds the key, after a pairing request made on
    /// `requested_on`.
    Paired { requested_on: String },
    /// Signed with the file key store of a debug build.
    TestStore,
}

/// What a person approved. Its only signed form is [`ApprovalStatement::signing_bytes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalStatement {
    pub project_id: String,
    pub change_id: String,
    pub author_patch_id: String,
    pub amend_patch_id: String,
    /// The branch tip approved (the adopted tip for [`Basis::Adopt`]).
    pub commit: String,
    /// For [`Basis::Adopt`] only: main's tip before the adoption.
    pub base: Option<String>,
    /// For [`Basis::Adopt`] only: every first-parent commit from `base` to the tip.
    pub adopted: Vec<String>,
    pub basis: Basis,
    pub approver: String,
    /// The first 16 hex digits of the SHA-256 of the signing public key (see [`crate::key_id`]).
    pub key_id: String,
    pub method: Method,
    pub at: DateTime<Utc>,
}

/// The first line of the signing bytes; it versions the format.
const HEADER: &str = "bongos-approval/1";

/// The fields of the signing bytes, in their fixed order.
const FIELDS: [&str; 12] = [
    "project_id",
    "change_id",
    "author_patch_id",
    "amend_patch_id",
    "commit",
    "base",
    "adopted",
    "basis",
    "approver",
    "key_id",
    "method",
    "at",
];

/// The prefix of a signed approval's line form.
const LINE_PREFIX: &str = "ba1";

impl ApprovalStatement {
    /// The bytes that are signed and verified: `bongos-approval/1\n`, then one `key=value\n` line
    /// per field in a fixed order.
    ///
    /// Every free-text value is percent-encoded (`%`, `=`, `,` and control characters become
    /// `%XX`), so no value can contain a newline or `=` and no value can add or change a field.
    /// `base` is `-` when absent and `+<value>` when present; `adopted` is the element count
    /// followed by `,<value>` per element; `method` is `keyring`, `test-store` or
    /// `paired,<requested_on>`; `at` is RFC 3339 in UTC.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let base = match &self.base {
            None => "-".to_string(),
            Some(base) => format!("+{}", encode(base)),
        };
        let mut adopted = self.adopted.len().to_string();
        for commit in &self.adopted {
            adopted.push(',');
            adopted.push_str(&encode(commit));
        }
        let method = match &self.method {
            Method::Keyring => "keyring".to_string(),
            Method::TestStore => "test-store".to_string(),
            Method::Paired { requested_on } => format!("paired,{}", encode(requested_on)),
        };
        let values = [
            encode(&self.project_id),
            encode(&self.change_id),
            encode(&self.author_patch_id),
            encode(&self.amend_patch_id),
            encode(&self.commit),
            base,
            adopted,
            self.basis.as_str().to_string(),
            encode(&self.approver),
            encode(&self.key_id),
            method,
            self.at.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        ];
        let mut out = String::new();
        out.push_str(HEADER);
        out.push('\n');
        for (key, value) in FIELDS.iter().zip(values) {
            out.push_str(key);
            out.push('=');
            out.push_str(&value);
            out.push('\n');
        }
        out.into_bytes()
    }

    /// Parses signing bytes back into a statement. Only the canonical form is accepted: the bytes
    /// must equal the parsed statement's own [`Self::signing_bytes`].
    pub fn from_signing_bytes(bytes: &[u8]) -> Result<Self, LineError> {
        let text = std::str::from_utf8(bytes).map_err(|_| LineError::NotUtf8)?;
        let body = text
            .strip_prefix(HEADER)
            .and_then(|rest| rest.strip_prefix('\n'))
            .ok_or(LineError::BadHeader)?;
        let body = body
            .strip_suffix('\n')
            .ok_or_else(|| LineError::BadField("the last line has no newline".into()))?;
        let lines: Vec<&str> = body.split('\n').collect();
        if lines.len() != FIELDS.len() {
            return Err(LineError::BadField(format!(
                "expected {} fields, found {}",
                FIELDS.len(),
                lines.len()
            )));
        }
        let mut values = Vec::with_capacity(FIELDS.len());
        for (line, key) in lines.iter().zip(FIELDS) {
            let value = line
                .strip_prefix(key)
                .and_then(|rest| rest.strip_prefix('='))
                .ok_or_else(|| LineError::BadField(format!("expected the field {key}")))?;
            if value.contains('=') {
                return Err(LineError::BadField(format!("the field {key} contains =")));
            }
            values.push(value);
        }
        let field = |index: usize| decode(values[index], FIELDS[index]);

        let base = match values[5] {
            "-" => None,
            other => match other.strip_prefix('+') {
                Some(base) => Some(decode(base, "base")?),
                None => return Err(LineError::BadField("bad base".into())),
            },
        };
        let mut parts = values[6].split(',');
        let count: usize = parts
            .next()
            .and_then(|count| count.parse().ok())
            .ok_or_else(|| LineError::BadField("bad adopted count".into()))?;
        let adopted = parts
            .map(|commit| decode(commit, "adopted"))
            .collect::<Result<Vec<_>, _>>()?;
        if adopted.len() != count {
            return Err(LineError::BadField(
                "the adopted count does not match".into(),
            ));
        }
        let basis =
            Basis::parse(values[7]).ok_or_else(|| LineError::BadField("unknown basis".into()))?;
        let method = match values[10] {
            "keyring" => Method::Keyring,
            "test-store" => Method::TestStore,
            other => match other.strip_prefix("paired,") {
                Some(requested_on) => Method::Paired {
                    requested_on: decode(requested_on, "method")?,
                },
                None => return Err(LineError::BadField("unknown method".into())),
            },
        };
        let at = DateTime::parse_from_rfc3339(values[11])
            .map_err(|_| LineError::BadField("bad time".into()))?
            .with_timezone(&Utc);

        let statement = ApprovalStatement {
            project_id: field(0)?,
            change_id: field(1)?,
            author_patch_id: field(2)?,
            amend_patch_id: field(3)?,
            commit: field(4)?,
            base,
            adopted,
            basis,
            approver: field(8)?,
            key_id: field(9)?,
            method,
            at,
        };
        if statement.signing_bytes() != bytes {
            return Err(LineError::NotCanonical);
        }
        Ok(statement)
    }
}

/// A statement with its Ed25519 signature over [`ApprovalStatement::signing_bytes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedApproval {
    pub statement: ApprovalStatement,
    pub signature: Vec<u8>,
}

impl SignedApproval {
    /// The line form stored in rows, evidence and trailers and sent over the host socket:
    /// `ba1.<base64url(signing bytes)>.<base64url(signature)>`, unpadded.
    pub fn to_line(&self) -> String {
        format!(
            "{LINE_PREFIX}.{}.{}",
            URL_SAFE_NO_PAD.encode(self.statement.signing_bytes()),
            URL_SAFE_NO_PAD.encode(&self.signature)
        )
    }

    /// Parses [`Self::to_line`]'s form. It checks the format only; [`crate::verify`] checks the
    /// signature.
    pub fn from_line(line: &str) -> Result<Self, LineError> {
        let parts: Vec<&str> = line.split('.').collect();
        let [prefix, statement, signature] = parts.as_slice() else {
            return Err(LineError::BadShape);
        };
        if *prefix != LINE_PREFIX {
            return Err(LineError::BadPrefix);
        }
        let statement = URL_SAFE_NO_PAD
            .decode(statement)
            .map_err(|_| LineError::BadBase64)?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| LineError::BadBase64)?;
        Ok(SignedApproval {
            statement: ApprovalStatement::from_signing_bytes(&statement)?,
            signature,
        })
    }
}

/// Why a signed approval line or signing bytes could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LineError {
    #[error("a signed approval line has three dot-separated parts")]
    BadShape,
    #[error("a signed approval line starts with {LINE_PREFIX}")]
    BadPrefix,
    #[error("a signed approval line is not valid unpadded base64url")]
    BadBase64,
    #[error("an approval statement is not valid UTF-8")]
    NotUtf8,
    #[error("an approval statement starts with {HEADER}")]
    BadHeader,
    #[error("an approval statement has a bad field: {0}")]
    BadField(String),
    #[error("an approval statement is not in its canonical form")]
    NotCanonical,
}

fn needs_encoding(byte: u8) -> bool {
    matches!(byte, b'%' | b'=' | b',') || byte < 0x20 || byte == 0x7f
}

fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii() && needs_encoding(ch as u8) {
            out.push_str(&format!("%{:02X}", ch as u8));
        } else {
            out.push(ch);
        }
    }
    out
}

fn decode(value: &str, field: &str) -> Result<String, LineError> {
    let bad = || LineError::BadField(format!("bad percent-encoding in {field}"));
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let hex = bytes.get(index + 1..index + 3).ok_or_else(bad)?;
            let hex = std::str::from_utf8(hex).map_err(|_| bad())?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| bad())?);
            index += 3;
        } else if needs_encoding(byte) {
            return Err(bad());
        } else {
            out.push(byte);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| LineError::NotUtf8)
}
