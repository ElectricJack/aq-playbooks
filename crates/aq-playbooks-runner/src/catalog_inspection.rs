//! Bounded metadata for storage-owned current catalog inspection. Pure
//! projection helpers do not themselves prove current ownership or freshness.
use crate::{State, StorageError};
use aq_playbooks_model::hash_bytes;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
mod projection;
pub use projection::project;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum CatalogError {
    #[error("bounded current catalog inspection unavailable")]
    Unsupported,
    #[error("current catalog namespace ownership unavailable or lost")]
    Ownership,
    #[error("current catalog context changed")]
    Conflict,
    #[error("catalog backend: {0}")]
    Backend(String),
}
impl From<StorageError> for CatalogError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Ownership => Self::Ownership,
            StorageError::Conflict => Self::Conflict,
            StorageError::Backend(reason) => Self::Backend(reason),
        }
    }
}

pub const CATALOG_VERSION: &str = "playbooks-catalog-1";
pub const MAX_CATALOG_PAGE: usize = 25;
pub const MAX_CATALOG_BYTES: usize = 262_144;
pub const MAX_CATALOG_ROW_BYTES: usize = 131_072;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogKind {
    Definitions,
    Activations,
}
impl CatalogKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Definitions => "definitions",
            Self::Activations => "activations",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogStamp {
    pub namespace: String,
    pub kind: CatalogKind,
    #[serde(with = "decimal")]
    pub incarnation: u64,
    #[serde(with = "decimal")]
    pub metadata_revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogQuery {
    pub kind: CatalogKind,
    /// An opaque item address from a previous page, never a mutation permit.
    pub exact: Option<String>,
    pub after: Option<String>,
    pub limit: usize,
    pub expected: Option<CatalogStamp>,
}
impl CatalogQuery {
    pub fn first(kind: CatalogKind, limit: usize) -> Self {
        Self {
            kind,
            exact: None,
            after: None,
            limit,
            expected: None,
        }
    }
    pub fn validate(&self) -> Result<(), CatalogError> {
        let valid_bounds = (1..=MAX_CATALOG_PAGE).contains(&self.limit) && self.valid_cursor();
        if !valid_bounds || !self.valid_stamp() {
            return Err(CatalogError::Backend(
                "invalid catalog query bounds or context".into(),
            ));
        }
        Ok(())
    }

    fn valid_cursor(&self) -> bool {
        let valid_keys = self
            .exact
            .iter()
            .chain(&self.after)
            .all(|key| valid_digest(key));
        let paginated = self.after.is_none() || (self.expected.is_some() && self.exact.is_none());
        valid_keys && paginated
    }

    fn matches(&self, item: &CatalogItem, previous: Option<&str>) -> bool {
        let ordered = previous.is_none_or(|key| item.key.as_str() > key);
        let selected = self.exact.as_ref().is_none_or(|key| key == &item.key);
        item.kind() == self.kind && ordered && selected
    }

    fn valid_stamp(&self) -> bool {
        self.expected
            .as_ref()
            .is_none_or(|stamp| stamp.valid_for(self.kind))
    }
}

impl CatalogStamp {
    fn check_page(&self, query: &CatalogQuery, state_revision: u64) -> Result<(), CatalogError> {
        if !self.valid_for(query.kind) || self.metadata_revision > state_revision {
            return Err(CatalogError::Backend(
                "invalid catalog projection stamp".into(),
            ));
        }
        if query
            .expected
            .as_ref()
            .is_some_and(|expected| expected != self)
        {
            return Err(CatalogError::Conflict);
        }
        Ok(())
    }

    fn valid_for(&self, kind: CatalogKind) -> bool {
        let initialized = self.incarnation > 0 && self.metadata_revision > 0;
        initialized && self.kind == kind && valid_context(&self.namespace)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSelection {
    Exact,
    Catalog,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogDependency {
    pub binding: String,
    pub reference_artifact: String,
    pub selection: CatalogSelection,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionMetadata {
    pub artifact: String,
    pub executable_fingerprint: String,
    pub source_digest: String,
    pub name: String,
    pub scope: String,
    pub compiler: String,
    pub purpose: String,
    pub purpose_truncated: bool,
    pub entries: Vec<String>,
    pub dependencies: BTreeMap<String, CatalogDependency>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogControls {
    pub calls_enabled: bool,
    pub event_starts_enabled: bool,
    pub execution_enabled: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationMetadata {
    pub binding: String,
    pub artifact: String,
    #[serde(with = "decimal")]
    pub revision: u64,
    pub controls: CatalogControls,
    pub artifact_execution_denied: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "metadata",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CatalogRecord {
    Definition(DefinitionMetadata),
    Activation(ActivationMetadata),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogItem {
    pub key: String,
    pub record: CatalogRecord,
}
impl CatalogItem {
    pub fn kind(&self) -> CatalogKind {
        match self.record {
            CatalogRecord::Definition(_) => CatalogKind::Definitions,
            CatalogRecord::Activation(_) => CatalogKind::Activations,
        }
    }
    pub fn validate(&self) -> Result<(), CatalogError> {
        self.validated_size().map(|_| ())
    }

    fn validated_size(&self) -> Result<usize, CatalogError> {
        let valid = match &self.record {
            CatalogRecord::Definition(d) => self.key == d.artifact && d.valid(),
            CatalogRecord::Activation(a) => self.key == activation_key(&a.binding) && a.valid(),
        };
        if !valid {
            return Err(CatalogError::Unsupported);
        }
        let bytes = encoded_length(self)?;
        if bytes > MAX_CATALOG_ROW_BYTES {
            return Err(CatalogError::Unsupported);
        }
        Ok(bytes)
    }
}

impl DefinitionMetadata {
    fn valid(&self) -> bool {
        let digests = [
            &self.artifact,
            &self.executable_fingerprint,
            &self.source_digest,
        ];
        let named = valid_name(&self.name) && valid_name(&self.scope);
        let compiler = (1..=256).contains(&self.compiler.len());
        let fields = named && compiler && self.purpose.len() <= 4096;
        fields
            && digests.iter().all(|digest| valid_digest(digest))
            && self.valid_entries()
            && self.valid_dependencies()
    }

    fn valid_entries(&self) -> bool {
        (1..=128).contains(&self.entries.len())
            && self.entries.iter().all(|name| valid_name(name))
            && self.entries.windows(2).all(|pair| pair[0] < pair[1])
    }

    fn valid_dependencies(&self) -> bool {
        self.dependencies.len() <= 128
            && self.dependencies.iter().all(|(name, dependency)| {
                valid_name(name)
                    && valid_name(&dependency.binding)
                    && valid_digest(&dependency.reference_artifact)
            })
    }
}

impl ActivationMetadata {
    fn valid(&self) -> bool {
        (1..=512).contains(&self.binding.len()) && valid_digest(&self.artifact) && self.revision > 0
    }
}

fn encoded_length(value: &impl Serialize) -> Result<usize, CatalogError> {
    serde_json::to_vec(value)
        .map(|encoded| encoded.len())
        .map_err(|_| CatalogError::Unsupported)
}

/// Activation addresses are opaque stable identifiers. They are not secrets,
/// authentication tokens or permission to inspect a different namespace.
pub fn activation_key(binding: &str) -> String {
    hash_bytes(
        &[
            b"aq-playbooks.catalog.binding.v1\0".as_slice(),
            binding.as_bytes(),
        ]
        .concat(),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogPage {
    pub api_version: String,
    #[serde(with = "decimal")]
    pub state_revision: u64,
    pub stamp: CatalogStamp,
    pub items: Vec<CatalogItem>,
    pub next_cursor: Option<String>,
}

/// Derived writer state only. A current read must go through the Storage method
/// that checks its actual lease and projected state revision.
pub type CatalogProjection = BTreeMap<(CatalogKind, String), CatalogItem>;
/// Pack already bounded stored rows into one response. No snapshot lookup occurs.
pub fn make_page(
    query: &CatalogQuery,
    stamp: CatalogStamp,
    state_revision: u64,
    rows: impl IntoIterator<Item = CatalogItem>,
    more: bool,
) -> Result<CatalogPage, CatalogError> {
    query.validate()?;
    stamp.check_page(query, state_revision)?;
    let mut page = CatalogPage {
        api_version: CATALOG_VERSION.into(),
        state_revision,
        stamp,
        items: vec![],
        next_cursor: None,
    };
    let truncated = page.append_rows(query, rows)?;
    if more || truncated {
        page.next_cursor = page.items.last().map(|last| last.key.clone());
    }
    if encoded_length(&page)? > MAX_CATALOG_BYTES {
        return Err(CatalogError::Unsupported);
    }
    Ok(page)
}

impl CatalogPage {
    fn append_rows(
        &mut self,
        query: &CatalogQuery,
        rows: impl IntoIterator<Item = CatalogItem>,
    ) -> Result<bool, CatalogError> {
        // Reserve more than the final cursor framing without assuming JSON character
        // width. Each single row fits the remaining response by construction.
        let mut bytes = encoded_length(self)? + 256;
        let mut previous = query.after.clone();
        for item in rows {
            let size = item.validated_size()? + 1;
            if !query.matches(&item, previous.as_deref()) {
                return Err(CatalogError::Backend("catalog row context mismatch".into()));
            }
            if bytes + size > MAX_CATALOG_BYTES || self.items.len() == query.limit {
                return Ok(true);
            }
            bytes += size;
            previous = Some(item.key.clone());
            self.items.push(item);
        }
        Ok(false)
    }
}

pub fn valid_context(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.contains('\0')
}
fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

mod decimal {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let value = String::deserialize(deserializer)?;
        let canonical = value == "0" || !value.starts_with('0');
        let digits = (1..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit());
        if !canonical || !digits {
            return Err(D::Error::custom("invalid unsigned decimal revision"));
        }
        value
            .parse()
            .map_err(|_| D::Error::custom("unsigned decimal revision overflow"))
    }
}
