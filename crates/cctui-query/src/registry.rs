use crate::ast::FilterOp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Id,
    Str,
    Enum,
    Bool,
}

pub struct FieldDef {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub ty: FieldType,
    pub default_op: FilterOp,
    pub enum_values: &'static [&'static str],
}

pub static FIELDS: &[FieldDef] = &[
    FieldDef {
        name: "machine",
        aliases: &["m"],
        ty: FieldType::Id,
        default_op: FilterOp::Eq,
        enum_values: &[],
    },
    FieldDef {
        name: "account",
        aliases: &["acct"],
        ty: FieldType::Str,
        default_op: FilterOp::Eq,
        enum_values: &[],
    },
    FieldDef {
        name: "tag",
        aliases: &["label"],
        ty: FieldType::Enum,
        default_op: FilterOp::Eq,
        enum_values: &[],
    },
    FieldDef {
        name: "title",
        aliases: &["name"],
        ty: FieldType::Str,
        default_op: FilterOp::Contains,
        enum_values: &[],
    },
    FieldDef {
        name: "status",
        aliases: &[],
        ty: FieldType::Enum,
        default_op: FilterOp::Eq,
        enum_values: &["new", "active", "inactive", "archived", "draft", "queued"],
    },
    FieldDef {
        name: "model",
        aliases: &[],
        ty: FieldType::Str,
        default_op: FilterOp::Contains,
        enum_values: &[],
    },
    FieldDef {
        name: "effort",
        aliases: &[],
        ty: FieldType::Enum,
        default_op: FilterOp::Eq,
        enum_values: &["low", "high"],
    },
    FieldDef {
        name: "adapter",
        aliases: &[],
        ty: FieldType::Enum,
        default_op: FilterOp::Eq,
        enum_values: &["claude-code", "codex"],
    },
    FieldDef {
        name: "pinned",
        aliases: &["starred"],
        ty: FieldType::Bool,
        default_op: FilterOp::Eq,
        enum_values: &[],
    },
    FieldDef {
        name: "dir",
        aliases: &["cwd"],
        ty: FieldType::Str,
        default_op: FilterOp::Contains,
        enum_values: &[],
    },
];

#[must_use]
pub fn resolve(token: &str) -> Option<&'static FieldDef> {
    let lower = token.to_lowercase();
    FIELDS.iter().find(|f| f.name == lower || f.aliases.iter().any(|a| *a == lower))
}
