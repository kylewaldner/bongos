//! The session types bongos ships, embedded from `chat-types/`. `seed` writes a type that has no
//! directory yet; nothing here is ever written over an existing file.

/// One shipped type: the directory name and its files, each as a name and its text.
#[derive(Clone, Copy, Debug)]
pub struct ShippedType {
    pub name: &'static str,
    pub files: &'static [(&'static str, &'static str)],
}

macro_rules! shipped_type {
    ($name:literal) => {
        ShippedType {
            name: $name,
            files: &[
                (
                    "type.toml",
                    include_str!(concat!("../chat-types/", $name, "/type.toml")),
                ),
                (
                    "context.md",
                    include_str!(concat!("../chat-types/", $name, "/context.md")),
                ),
            ],
        }
    };
}

/// The five shipped types, in the order the picker offers them.
pub const SHIPPED_TYPES: &[ShippedType] = &[
    shipped_type!("plan"),
    shipped_type!("implement"),
    shipped_type!("review"),
    shipped_type!("thinking"),
    shipped_type!("free"),
];
