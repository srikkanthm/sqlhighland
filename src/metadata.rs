//! Per-connection dictionary cache for autocomplete, hover, and the schema
//! browser.
//!
//! GUI-free: blocking fetchers take `&mut dyn DbClient` (a real session on the
//! background executor, fakes in tests) and are reached through the
//! engine-agnostic [`MetadataProvider`] seam ([`provider_for`]). The view owns
//! `HashMap<connection_id, Arc<Mutex<MetadataCache>>>` beside the session
//! pool; the completion provider only ever clones an `Arc` snapshot. The
//! Oracle dictionary SQL lives in the `*_blocking` fetchers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::complete::{ForeignKey, SYSTEM_SCHEMAS};
use crate::db::DbClient;
use crate::schema::DbEngine;

/// Cap per dictionary query (protects huge schemas; highland-size DBs never
/// come close).
pub const DICT_MAX_ROWS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TableKind {
    Table,
    View,
    Sequence,
    /// A synonym: queryable like a table; columns resolve through
    /// [`MetadataCache::synonyms`].
    Synonym,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableId {
    pub owner: String,
    pub name: String,
    pub kind: TableKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    pub data_type: String,
    /// `ALL_COL_COMMENTS` text (may be empty). Shown in popup detail.
    pub comments: String,
}

/// One synonym: `owner.name` resolves to `[table_owner.]table_name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Synonym {
    pub owner: String,
    pub name: String,
    pub table_owner: Option<String>,
    pub table_name: String,
}

/// One callable member of a package (`pkg.member`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageMember {
    pub owner: String,
    pub package: String,
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct MetadataCache {
    pub tables: Vec<TableId>,
    pub columns: HashMap<(String, String), Vec<ColumnMeta>>,
    pub sequences: Vec<TableId>,
    /// Referential constraints for JOIN … ON suggestions.
    pub fks: Vec<ForeignKey>,
    /// Synonym resolution: `(owner_upper, synonym_upper)` → `(table_owner,
    /// table_name)`. Used to serve columns for a synonym name.
    pub synonyms: HashMap<(String, String), (Option<String>, String)>,
    /// Package members: `(owner_upper, package_upper)` → member names.
    pub package_members: HashMap<(String, String), Vec<String>>,
    pub fetched_at: Option<Instant>,
    pub loading: bool,
    /// Whether an on-disk cache has already been consulted this session, so a
    /// miss is not retried on every `ensure_meta` call. Runtime-only.
    pub disk_loaded: bool,
}

impl MetadataCache {
    /// True when the cache should be refreshed: never fetched, or older than
    /// `ttl`. `None` means it never expires by time (only a reconnect or a
    /// manual refresh clears it).
    pub fn is_stale(&self, ttl: Option<std::time::Duration>) -> bool {
        match self.fetched_at {
            None => true,
            Some(t) => match ttl {
                None => false,
                Some(ttl) => t.elapsed() > ttl,
            },
        }
    }

    /// Uppercase sequence-name lookup for `seq.|` detection.
    pub fn is_sequence(&self, upper_name: &str) -> bool {
        self.sequences
            .iter()
            .any(|s| s.name.to_ascii_uppercase() == upper_name)
    }

    /// Columns for `(owner, table)` (both compared uppercase). `None` owner
    /// matches any owner with that table name (merged). A name with no direct
    /// columns is retried through the synonym map.
    pub fn columns_for(&self, owner: Option<&str>, table: &str) -> Vec<ColumnMeta> {
        let direct = self.direct_columns(owner, table);
        if !direct.is_empty() {
            return direct;
        }
        let t = table.to_ascii_uppercase();
        let resolved = match owner {
            Some(o) => self.synonyms.get(&(o.to_ascii_uppercase(), t)),
            None => self
                .synonyms
                .iter()
                .find(|((_, syn), _)| *syn == t)
                .map(|(_, v)| v),
        };
        match resolved {
            Some((to, tn)) => self.direct_columns(to.as_deref(), tn),
            None => direct,
        }
    }

    fn direct_columns(&self, owner: Option<&str>, table: &str) -> Vec<ColumnMeta> {
        let t = table.to_ascii_uppercase();
        match owner {
            Some(o) => {
                let o = o.to_ascii_uppercase();
                self.columns.get(&(o, t)).cloned().unwrap_or_default()
            }
            None => {
                let mut out = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for ((_, tbl), cols) in &self.columns {
                    if *tbl == t {
                        for c in cols {
                            if seen.insert(c.name.clone()) {
                                out.push(c.clone());
                            }
                        }
                    }
                }
                out.sort_by(|a, b| a.name.cmp(&b.name));
                out
            }
        }
    }

    /// True when `upper_name` names a package (any owner).
    pub fn is_package_name(&self, upper_name: &str) -> bool {
        self.package_members.keys().any(|(_, p)| p == upper_name)
    }

    /// Members of `[owner.]package` (case-insensitive); a `None` owner matches
    /// any owner. Deduped, document order.
    pub fn package_member_names(&self, owner: Option<&str>, package: &str) -> Vec<String> {
        let p = package.to_ascii_uppercase();
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for ((o, pkg), members) in &self.package_members {
            if *pkg != p || owner.is_some_and(|want| !o.eq_ignore_ascii_case(want)) {
                continue;
            }
            for m in members {
                if seen.insert(m.to_ascii_uppercase()) {
                    out.push(m.clone());
                }
            }
        }
        out
    }
}

/// Snapshot shared with the completion provider (cheap `Arc` clone).
pub type SharedCache = Arc<Mutex<MetadataCache>>;

// -- On-disk persistence -----------------------------------------------------

/// Format version for the persisted cache. Bump on any shape change so an old
/// file is ignored (and refetched) instead of mis-parsed.
pub const METADATA_CACHE_VERSION: u32 = 1;

/// Identity of the connection + filter a cache was fetched for. A persisted
/// cache is only reused when this matches the current connection and
/// `show_system`, so editing a connection (host/user/role/…) or toggling
/// system schemas invalidates the file instead of serving the wrong schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheFingerprint {
    pub engine: String,
    pub host: String,
    pub port: u16,
    pub service_name: String,
    pub service_kind: String,
    pub ssl: bool,
    pub user: String,
    pub role: String,
    pub include_system: bool,
}

impl CacheFingerprint {
    /// Fingerprint the connection params + system-schema filter.
    pub fn of(cfg: &crate::model::ConnectionConfig, include_system: bool) -> Self {
        Self {
            engine: cfg.engine.label().to_string(),
            host: cfg.host.clone(),
            port: cfg.port,
            service_name: cfg.service_name.clone(),
            service_kind: cfg.service_kind.label().to_string(),
            ssl: cfg.ssl,
            user: cfg.user.clone(),
            role: cfg.role.label().to_string(),
            include_system,
        }
    }
}

/// Serializable mirror of [`MetadataCache`]. `fetched_at` is unix millis
/// (not `Instant`, which has no stable serialization) and `loading` is
/// runtime-only, so neither round-trips. The tuple-keyed maps become entry
/// vectors: JSON object keys must be strings, and `(owner, name)` is not.
pub type SynonymEntry = ((String, String), (Option<String>, String));

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataCacheDisk {
    pub version: u32,
    pub fingerprint: CacheFingerprint,
    /// Unix epoch millis when the data was fetched, or 0 when unknown.
    pub fetched_at_ms: u64,
    pub tables: Vec<TableId>,
    pub columns: Vec<((String, String), Vec<ColumnMeta>)>,
    pub sequences: Vec<TableId>,
    pub fks: Vec<ForeignKey>,
    pub synonyms: Vec<SynonymEntry>,
    pub package_members: Vec<((String, String), Vec<String>)>,
}

impl MetadataCacheDisk {
    /// Capture a cache for `fingerprint`. `fetched_at` is preserved as millis
    /// relative to now so a loaded cache keeps its original age.
    pub fn capture(cache: &MetadataCache, fingerprint: CacheFingerprint) -> Self {
        let fetched_at_ms = cache
            .fetched_at
            .map(|t| t.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
            .map(|age| now_unix_millis().saturating_sub(age))
            .unwrap_or(0);
        Self {
            version: METADATA_CACHE_VERSION,
            fingerprint,
            fetched_at_ms,
            tables: cache.tables.clone(),
            columns: cache
                .columns
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            sequences: cache.sequences.clone(),
            fks: cache.fks.clone(),
            synonyms: cache
                .synonyms
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            package_members: cache
                .package_members
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    /// Rebuild a runtime cache. `fetched_at` is reconstructed from the saved
    /// timestamp; a timestamp in the future (clock moved back) reads as now.
    pub fn into_cache(self) -> MetadataCache {
        let fetched_at = (self.fetched_at_ms > 0).then(|| {
            let age = now_unix_millis().saturating_sub(self.fetched_at_ms);
            Instant::now()
                .checked_sub(std::time::Duration::from_millis(age))
                .unwrap_or_else(Instant::now)
        });
        MetadataCache {
            tables: self.tables,
            columns: self.columns.into_iter().collect(),
            sequences: self.sequences,
            fks: self.fks,
            synonyms: self.synonyms.into_iter().collect(),
            package_members: self.package_members.into_iter().collect(),
            fetched_at,
            loading: false,
            disk_loaded: true,
        }
    }
}

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Path of a connection's persisted cache file.
pub fn cache_path(conn_id: &str) -> anyhow::Result<std::path::PathBuf> {
    anyhow::ensure!(
        !conn_id.is_empty()
            && !conn_id.contains(['/', '\\', ':', '\0'])
            && conn_id != "."
            && conn_id != "..",
        "unsafe connection id {conn_id:?}"
    );
    Ok(crate::config::metadata_cache_dir()?.join(format!("{conn_id}.json.gz")))
}

/// Load and decompress a persisted cache. Returns `None` for a missing file,
/// an unreadable/corrupt one, a version mismatch, or a fingerprint mismatch
/// (each treated as a miss → refetch). Never panics.
pub fn load_cache(conn_id: &str, fingerprint: &CacheFingerprint) -> Option<MetadataCache> {
    let path = cache_path(conn_id).ok()?;
    let bytes = std::fs::read(&path).ok()?;
    let disk: MetadataCacheDisk = decode_cache(&bytes)?;
    if disk.version != METADATA_CACHE_VERSION || disk.fingerprint != *fingerprint {
        return None;
    }
    Some(disk.into_cache())
}

/// Delete a connection's persisted cache (best-effort; a missing file is fine).
pub fn delete_cache(conn_id: &str) {
    if let Ok(path) = cache_path(conn_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Serialize + gzip a cache and write it atomically. Returns the uncompressed
/// size so the caller can report a cap skip. `cap` (bytes) is the ceiling on
/// the uncompressed payload; `None` is uncapped. A cache over the cap is not
/// written and `Ok(None)` is returned.
pub fn save_cache(
    conn_id: &str,
    disk: &MetadataCacheDisk,
    cap: Option<u64>,
) -> anyhow::Result<Option<u64>> {
    let json = serde_json::to_vec(disk).context("encoding metadata cache")?;
    let size = json.len() as u64;
    if cap.is_some_and(|cap| size > cap) {
        return Ok(None);
    }
    let gz = encode_cache(&json)?;
    let path = cache_path(conn_id)?;
    write_bytes_atomic(&path, &gz)?;
    Ok(Some(size))
}

/// Gzip `json`.
fn encode_cache(json: &[u8]) -> anyhow::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write as _;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(json).context("gzip metadata cache")?;
    enc.finish().context("finishing gzip metadata cache")
}

/// Gunzip + parse.
fn decode_cache(bytes: &[u8]) -> Option<MetadataCacheDisk> {
    use flate2::read::GzDecoder;
    use std::io::Read as _;
    let mut dec = GzDecoder::new(bytes);
    let mut json = Vec::new();
    dec.read_to_end(&mut json).ok()?;
    serde_json::from_slice(&json).ok()
}

/// Atomic write of raw bytes (the text-based [`crate::fsutil::write_atomic`]
/// takes a `&str`; a gzip payload is binary).
fn write_bytes_atomic(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let _ = crate::fsutil::restrict(parent, 0o700);
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all().ok();
    }
    let _ = crate::fsutil::restrict(&tmp, 0o600);
    std::fs::rename(&tmp, path).with_context(|| format!("moving {}", path.display()))?;
    Ok(())
}

/// System-schema predicate on `owner_col` (dictionary owners are stored
/// uppercase). Empty when `include_system`. `own_schema` (connected user)
/// is always exempt. Composes with AND: callers add their own WHERE.
pub fn system_predicate(owner_col: &str, include_system: bool, own_schema: &str) -> String {
    if include_system {
        return String::new();
    }
    let mut s = format!("{owner_col} NOT IN (");
    for (i, schema) in SYSTEM_SCHEMAS.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('\'');
        s.push_str(schema);
        s.push('\'');
    }
    s.push_str(&format!(
        ") AND {owner_col} NOT LIKE 'APEX\\_%' ESCAPE '\\' AND {owner_col} NOT LIKE 'FLOWS\\_%' ESCAPE '\\'"
    ));
    let own = own_schema.to_ascii_uppercase().replace('\'', "''");
    if own.is_empty() {
        s
    } else {
        // Parenthesized: AND binds tighter than OR, but spell it out.
        format!("({s} OR {owner_col} = '{own}')")
    }
}

/// Back-compat wrapper: full WHERE clause for queries without one.
pub fn system_filter_sql(owner_col: &str, include_system: bool, own_schema: &str) -> String {
    let p = system_predicate(owner_col, include_system, own_schema);
    if p.is_empty() {
        String::new()
    } else {
        format!(" WHERE {p}")
    }
}

/// Fetch tables+views+materialized views as `(owner, name, kind)` triples.
/// `own_schema` (connected user) is always exempt from the system filter.
pub fn fetch_tables_blocking(
    db: &mut dyn DbClient,
    include_system: bool,
    own_schema: &str,
) -> Result<Vec<TableId>, crate::db::DbError> {
    let filter = system_filter_sql("owner", include_system, own_schema);
    let r = db.run_query(
        &format!(
            "SELECT owner, table_name, 'TABLE' FROM all_tables{filter} \
             UNION ALL SELECT owner, view_name, 'VIEW' FROM all_views{filter} \
             UNION ALL SELECT owner, mview_name, 'VIEW' FROM all_mviews{filter}"
        ),
        DICT_MAX_ROWS,
        &[],
    )?;
    Ok(r.rows
        .iter()
        .filter_map(|row| match row.as_slice() {
            [Some(o), Some(t), Some(k)] => {
                let kind = if k.eq_ignore_ascii_case("VIEW") {
                    TableKind::View
                } else {
                    TableKind::Table
                };
                Some(TableId {
                    owner: o.clone(),
                    name: t.clone(),
                    kind,
                })
            }
            _ => None,
        })
        .collect())
}

/// Fetch columns grouped by `(OWNER, TABLE)` (uppercased keys), left-joined
/// to `ALL_COL_COMMENTS` for popup detail text.
pub fn fetch_columns_blocking(
    db: &mut dyn DbClient,
    include_system: bool,
    own_schema: &str,
) -> Result<HashMap<(String, String), Vec<ColumnMeta>>, crate::db::DbError> {
    let filter = system_filter_sql("t.owner", include_system, own_schema);
    let r = db.run_query(
        &format!(
            "SELECT t.owner, t.table_name, t.column_name, t.data_type, c.comments \
               FROM all_tab_columns t \
               LEFT JOIN all_col_comments c \
                 ON c.owner = t.owner AND c.table_name = t.table_name \
                AND c.column_name = t.column_name{filter}"
        ),
        DICT_MAX_ROWS,
        &[],
    )?;
    let mut map: HashMap<(String, String), Vec<ColumnMeta>> = HashMap::new();
    for row in &r.rows {
        if let [Some(o), Some(t), Some(c), dt, cm] = row.as_slice() {
            map.entry((o.to_ascii_uppercase(), t.to_ascii_uppercase()))
                .or_default()
                .push(ColumnMeta {
                    name: c.clone(),
                    data_type: dt.clone().unwrap_or_default(),
                    comments: cm.clone().unwrap_or_default(),
                });
        }
    }
    Ok(map)
}

/// Fetch referential constraints, grouped per constraint (composite keys
/// stay aligned by position). Filtered on the constrained table's owner.
pub fn fetch_fks_blocking(
    db: &mut dyn DbClient,
    include_system: bool,
    own_schema: &str,
) -> Result<Vec<ForeignKey>, crate::db::DbError> {
    let pred = system_predicate("c.owner", include_system, own_schema);
    let scope = if pred.is_empty() {
        String::new()
    } else {
        format!(" AND ({pred})")
    };
    let r = db.run_query(
        &format!(
            "SELECT c.owner, c.constraint_name, a.table_name, a.column_name, \
                c.r_owner, c2.table_name, c2.column_name \
             FROM all_constraints c \
             JOIN all_cons_columns a \
               ON a.owner = c.owner AND a.constraint_name = c.constraint_name \
             JOIN all_cons_columns c2 \
               ON c2.owner = c.r_owner AND c2.constraint_name = c.r_constraint_name \
              AND c2.position = a.position \
            WHERE c.constraint_type = 'R'{scope} \
            ORDER BY c.owner, c.constraint_name, a.position"
        ),
        DICT_MAX_ROWS,
        &[],
    )?;
    // Group rows by (owner, constraint); ORDER BY keeps positions aligned.
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<(String, String), ForeignKey> = HashMap::new();
    for row in &r.rows {
        if let [Some(o), Some(name), Some(ft), Some(fc), Some(ro), Some(tt), Some(tc)] =
            row.as_slice()
        {
            let key = (o.clone(), name.clone());
            let entry = groups.entry(key.clone()).or_insert_with(|| {
                order.push(key);
                ForeignKey {
                    name: name.clone(),
                    from_owner: Some(o.clone()),
                    from_table: ft.clone(),
                    from_cols: Vec::new(),
                    to_owner: Some(ro.clone()),
                    to_table: tt.clone(),
                    to_cols: Vec::new(),
                }
            });
            entry.from_cols.push(fc.clone());
            entry.to_cols.push(tc.clone());
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .collect())
}

/// Fetch sequences as `(owner, name)` pairs.
pub fn fetch_sequences_blocking(
    db: &mut dyn DbClient,
    include_system: bool,
    own_schema: &str,
) -> Result<Vec<TableId>, crate::db::DbError> {
    let filter = system_filter_sql("sequence_owner", include_system, own_schema);
    let r = db.run_query(
        &format!("SELECT sequence_owner, sequence_name FROM all_sequences{filter}"),
        DICT_MAX_ROWS,
        &[],
    )?;
    Ok(r.rows
        .iter()
        .filter_map(|row| match row.as_slice() {
            [Some(o), Some(t)] => Some(TableId {
                owner: o.clone(),
                name: t.clone(),
                kind: TableKind::Sequence,
            }),
            _ => None,
        })
        .collect())
}

/// Fetch synonyms as `owner.name → [table_owner.]table_name`.
pub fn fetch_synonyms_blocking(
    db: &mut dyn DbClient,
    include_system: bool,
    own_schema: &str,
) -> Result<Vec<Synonym>, crate::db::DbError> {
    let filter = system_filter_sql("owner", include_system, own_schema);
    let r = db.run_query(
        &format!("SELECT owner, synonym_name, table_owner, table_name FROM all_synonyms{filter}"),
        DICT_MAX_ROWS,
        &[],
    )?;
    Ok(r.rows
        .iter()
        .filter_map(|row| match row.as_slice() {
            [Some(o), Some(n), to, Some(t)] => Some(Synonym {
                owner: o.clone(),
                name: n.clone(),
                table_owner: to.clone(),
                table_name: t.clone(),
            }),
            _ => None,
        })
        .collect())
}

/// Fetch package members (`pkg.member`) from `ALL_PROCEDURES`.
pub fn fetch_package_members_blocking(
    db: &mut dyn DbClient,
    include_system: bool,
    own_schema: &str,
) -> Result<Vec<PackageMember>, crate::db::DbError> {
    let pred = system_predicate("owner", include_system, own_schema);
    let scope = if pred.is_empty() {
        String::new()
    } else {
        format!(" AND ({pred})")
    };
    let r = db.run_query(
        &format!(
            "SELECT owner, object_name, procedure_name FROM all_procedures \
             WHERE procedure_name IS NOT NULL{scope}"
        ),
        DICT_MAX_ROWS,
        &[],
    )?;
    Ok(r.rows
        .iter()
        .filter_map(|row| match row.as_slice() {
            [Some(o), Some(p), Some(m)] => Some(PackageMember {
                owner: o.clone(),
                package: p.clone(),
                name: m.clone(),
            }),
            _ => None,
        })
        .collect())
}

/// Engine-specific dictionary fetching for autocomplete, hover, and the
/// schema browser. The Oracle provider is the only implementation today; a
/// second engine supplies its own and a [`DbEngine`] arm in [`provider_for`],
/// so `browser.rs` never names a specific engine.
pub trait MetadataProvider: Send + Sync {
    fn fetch_tables(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<TableId>, crate::db::DbError>;
    fn fetch_columns(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<HashMap<(String, String), Vec<ColumnMeta>>, crate::db::DbError>;
    fn fetch_fks(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<ForeignKey>, crate::db::DbError>;
    fn fetch_sequences(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<TableId>, crate::db::DbError>;
    fn fetch_synonyms(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<Synonym>, crate::db::DbError>;
    fn fetch_package_members(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<PackageMember>, crate::db::DbError>;
}

/// Oracle dictionary provider: delegates to the `*_blocking` fetchers below.
pub struct OracleMetadata;

impl MetadataProvider for OracleMetadata {
    fn fetch_tables(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<TableId>, crate::db::DbError> {
        fetch_tables_blocking(db, include_system, own_schema)
    }

    fn fetch_columns(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<HashMap<(String, String), Vec<ColumnMeta>>, crate::db::DbError> {
        fetch_columns_blocking(db, include_system, own_schema)
    }

    fn fetch_fks(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<ForeignKey>, crate::db::DbError> {
        fetch_fks_blocking(db, include_system, own_schema)
    }

    fn fetch_sequences(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<TableId>, crate::db::DbError> {
        fetch_sequences_blocking(db, include_system, own_schema)
    }

    fn fetch_synonyms(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<Synonym>, crate::db::DbError> {
        fetch_synonyms_blocking(db, include_system, own_schema)
    }

    fn fetch_package_members(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<PackageMember>, crate::db::DbError> {
        fetch_package_members_blocking(db, include_system, own_schema)
    }
}

/// The dictionary provider for an engine. Add a match arm per [`DbEngine`].
pub fn provider_for(engine: DbEngine) -> &'static dyn MetadataProvider {
    match engine {
        DbEngine::Oracle => &OracleMetadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{BindParam, DbError};
    use crate::model::ColumnInfo;
    use crate::model::QueryResult;

    struct FakeDb {
        tables: QueryResult,
        columns: QueryResult,
        sequences: QueryResult,
        constraints: QueryResult,
        synonyms: QueryResult,
        packages: QueryResult,
    }

    fn qr(cols: &[&str], rows: &[Vec<Option<&str>>]) -> QueryResult {
        QueryResult {
            columns: cols
                .iter()
                .map(|c| ColumnInfo {
                    name: c.to_string(),
                    db_type: "DB_TYPE_VARCHAR2".to_string(),
                })
                .collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|c| c.map(str::to_string)).collect())
                .collect(),
            elapsed_ms: 0,
            truncated: false,
        }
    }

    impl DbClient for FakeDb {
        fn connect(&mut self, _: &crate::model::ConnectionConfig) -> Result<(), DbError> {
            Ok(())
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn disconnect(&mut self) {}
        fn run_query(
            &mut self,
            sql: &str,
            _max: usize,
            _binds: &[BindParam],
        ) -> Result<QueryResult, DbError> {
            if sql.contains("all_tab_columns") {
                Ok(std::mem::replace(&mut self.columns, qr(&[], &[])))
            } else if sql.contains("all_sequences") {
                Ok(std::mem::replace(&mut self.sequences, qr(&[], &[])))
            } else if sql.contains("all_synonyms") {
                Ok(std::mem::replace(&mut self.synonyms, qr(&[], &[])))
            } else if sql.contains("all_procedures") {
                Ok(std::mem::replace(&mut self.packages, qr(&[], &[])))
            } else if sql.contains("all_constraints") {
                Ok(std::mem::replace(&mut self.constraints, qr(&[], &[])))
            } else {
                Ok(std::mem::replace(&mut self.tables, qr(&[], &[])))
            }
        }
        fn exec(&mut self, _: &str, _: &[BindParam]) -> Result<(u64, u128), DbError> {
            Ok((0, 0))
        }
        fn commit(&mut self) -> Result<(), DbError> {
            Ok(())
        }
        fn rollback(&mut self) -> Result<(), DbError> {
            Ok(())
        }
    }

    fn fake() -> FakeDb {
        FakeDb {
            tables: qr(
                &["OWNER", "TABLE_NAME", "KIND"],
                &[
                    vec![Some("SCOTT"), Some("EMP"), Some("TABLE")],
                    vec![Some("SCOTT"), Some("DEPT"), Some("TABLE")],
                    vec![Some("SCOTT"), Some("EMPVW"), Some("VIEW")],
                    vec![None, Some("GHOST"), Some("TABLE")],
                ],
            ),
            columns: qr(
                &[
                    "OWNER",
                    "TABLE_NAME",
                    "COLUMN_NAME",
                    "DATA_TYPE",
                    "COMMENTS",
                ],
                &[
                    vec![
                        Some("SCOTT"),
                        Some("EMP"),
                        Some("EMPNO"),
                        Some("NUMBER"),
                        Some("employee id"),
                    ],
                    vec![
                        Some("SCOTT"),
                        Some("EMP"),
                        Some("ENAME"),
                        Some("VARCHAR2"),
                        None,
                    ],
                ],
            ),
            sequences: qr(
                &["SEQUENCE_OWNER", "SEQUENCE_NAME"],
                &[vec![Some("SCOTT"), Some("EMP_SEQ")]],
            ),
            constraints: qr(
                &[
                    "OWNER",
                    "CONSTRAINT_NAME",
                    "TABLE_NAME",
                    "COLUMN_NAME",
                    "R_OWNER",
                    "R_TABLE_NAME",
                    "R_COLUMN_NAME",
                ],
                &[
                    vec![
                        Some("SCOTT"),
                        Some("EMP_DEPT_FK"),
                        Some("EMP"),
                        Some("DEPTNO"),
                        Some("SCOTT"),
                        Some("DEPT"),
                        Some("DEPTNO"),
                    ],
                    // Composite key: two rows, one constraint.
                    vec![
                        Some("SCOTT"),
                        Some("COMP_FK"),
                        Some("A"),
                        Some("X"),
                        Some("SCOTT"),
                        Some("B"),
                        Some("X"),
                    ],
                    vec![
                        Some("SCOTT"),
                        Some("COMP_FK"),
                        Some("A"),
                        Some("Y"),
                        Some("SCOTT"),
                        Some("B"),
                        Some("Y"),
                    ],
                ],
            ),
            synonyms: qr(
                &["OWNER", "SYNONYM_NAME", "TABLE_OWNER", "TABLE_NAME"],
                &[
                    vec![Some("SCOTT"), Some("EMP_SYN"), Some("SCOTT"), Some("EMP")],
                    // No table target → dropped.
                    vec![Some("SCOTT"), Some("BAD_SYN"), None, None],
                ],
            ),
            packages: qr(
                &["OWNER", "OBJECT_NAME", "PROCEDURE_NAME"],
                &[
                    vec![Some("SCOTT"), Some("UTIL"), Some("ADD_ONE")],
                    vec![Some("SCOTT"), Some("UTIL"), Some("TO_UPPER")],
                ],
            ),
        }
    }

    #[test]
    fn tables_skip_null_owner() {
        let mut db = fake();
        let t = fetch_tables_blocking(&mut db, true, "").unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].name, "EMP");
        assert_eq!(t[0].kind, TableKind::Table);
        assert_eq!(t[2].name, "EMPVW");
        assert_eq!(t[2].kind, TableKind::View);
    }

    #[test]
    fn synonyms_fetch_and_column_indirection() {
        let mut db = fake();
        let syns = fetch_synonyms_blocking(&mut db, true, "").unwrap();
        assert_eq!(syns.len(), 1, "rows with no table target are dropped");
        assert_eq!(syns[0].name, "EMP_SYN");
        assert_eq!(syns[0].table_name, "EMP");

        let mut cache = MetadataCache {
            columns: fetch_columns_blocking(&mut db, true, "").unwrap(),
            ..Default::default()
        };
        cache.synonyms.insert(
            ("SCOTT".to_string(), "EMP_SYN".to_string()),
            (Some("SCOTT".to_string()), "EMP".to_string()),
        );
        // A synonym name resolves to the underlying table's columns.
        assert_eq!(cache.columns_for(Some("SCOTT"), "EMP_SYN").len(), 2);
        assert_eq!(cache.columns_for(None, "emp_syn").len(), 2);
        // An unknown name still returns nothing.
        assert!(cache.columns_for(Some("SCOTT"), "NOPE").is_empty());
    }

    #[test]
    fn package_members_fetch_and_lookup() {
        let mut db = fake();
        let members = fetch_package_members_blocking(&mut db, true, "").unwrap();
        assert_eq!(members.len(), 2);
        let mut cache = MetadataCache::default();
        for m in members {
            cache
                .package_members
                .entry((m.owner.to_ascii_uppercase(), m.package.to_ascii_uppercase()))
                .or_default()
                .push(m.name);
        }
        assert!(cache.is_package_name("UTIL"));
        assert!(!cache.is_package_name("EMP"));
        assert_eq!(
            cache.package_member_names(None, "util"),
            ["ADD_ONE", "TO_UPPER"]
        );
        assert_eq!(cache.package_member_names(Some("SCOTT"), "UTIL").len(), 2);
        assert!(cache.package_member_names(Some("OTHER"), "UTIL").is_empty());
    }

    #[test]
    fn provider_for_dispatches_by_engine() {
        let mut db = fake();
        let provider = provider_for(DbEngine::Oracle);
        assert_eq!(provider.fetch_tables(&mut db, true, "").unwrap().len(), 3);
        assert_eq!(
            provider.fetch_sequences(&mut db, true, "").unwrap().len(),
            1
        );
    }

    #[test]
    fn columns_group_by_upper_key() {
        let mut db = fake();
        let m = fetch_columns_blocking(&mut db, true, "").unwrap();
        let cols = m.get(&("SCOTT".to_string(), "EMP".to_string())).unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].data_type, "NUMBER");
        assert_eq!(cols[0].comments, "employee id");
        assert_eq!(cols[1].comments, "");
    }

    #[test]
    fn sequences_fetch() {
        let mut db = fake();
        let s = fetch_sequences_blocking(&mut db, true, "").unwrap();
        assert_eq!(s[0].name, "EMP_SEQ");
        assert_eq!(s[0].kind, TableKind::Sequence);
    }

    #[test]
    fn fks_group_composite_keys() {
        let mut db = fake();
        let fks = fetch_fks_blocking(&mut db, true, "").unwrap();
        assert_eq!(fks.len(), 2);
        assert_eq!(fks[0].name, "EMP_DEPT_FK");
        assert_eq!(fks[0].from_cols, vec!["DEPTNO"]);
        assert_eq!(fks[0].to_cols, vec!["DEPTNO"]);
        assert_eq!(fks[1].from_cols, vec!["X", "Y"]);
        assert_eq!(fks[1].to_cols, vec!["X", "Y"]);
    }

    #[test]
    fn system_filter_sql_builds() {
        let f = system_filter_sql("owner", false, "");
        assert!(f.contains("WHERE owner NOT IN ('SYS', 'SYSTEM'"), "{f}");
        assert!(f.contains("NOT LIKE 'APEX\\_%'"), "{f}");
        assert_eq!(system_filter_sql("owner", true, ""), "");
        // sequence_owner variant.
        assert!(system_filter_sql("sequence_owner", false, "").starts_with(" WHERE sequence_owner"));
        // Own schema is exempt even when system.
        let f = system_filter_sql("owner", false, "system");
        assert!(f.contains("OR owner = 'SYSTEM'"), "{f}");
    }

    #[test]
    fn cache_columns_for_merges_owners() {
        let mut db = fake();
        let mut cache = MetadataCache {
            columns: fetch_columns_blocking(&mut db, true, "").unwrap(),
            ..Default::default()
        };
        assert_eq!(cache.columns_for(Some("scott"), "emp").len(), 2);
        assert_eq!(cache.columns_for(None, "emp").len(), 2);
        assert!(cache.columns_for(None, "nope").is_empty());
        assert!(cache.is_stale(None));
        assert!(cache.is_stale(Some(std::time::Duration::from_secs(60))));
        cache.fetched_at = Some(Instant::now());
        // A fresh cache is never stale: `None` never expires by time, and a
        // finite TTL only trips once elapsed exceeds it.
        assert!(!cache.is_stale(None));
        assert!(!cache.is_stale(Some(std::time::Duration::from_secs(3600))));
    }

    /// Process-global env (the config dir) → serialize these tests.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::testenv::config_dir_lock()
    }

    fn staged_cache_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sqlhighland-metacache-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir) };
        dir
    }

    fn sample_fingerprint() -> CacheFingerprint {
        CacheFingerprint {
            engine: "Oracle".to_string(),
            host: "db.example".to_string(),
            port: 1521,
            service_name: "pdb1".to_string(),
            service_kind: "Service".to_string(),
            ssl: false,
            user: "SCOTT".to_string(),
            role: "SYSDEFAULT".to_string(),
            include_system: false,
        }
    }

    #[test]
    fn cache_round_trips_through_disk() {
        let _guard = env_lock();
        let dir = staged_cache_dir("roundtrip");
        let mut db = fake();
        let cache = MetadataCache {
            tables: fetch_tables_blocking(&mut db, true, "").unwrap(),
            columns: fetch_columns_blocking(&mut db, true, "").unwrap(),
            sequences: fetch_sequences_blocking(&mut db, true, "").unwrap(),
            fks: fetch_fks_blocking(&mut db, true, "").unwrap(),
            fetched_at: Some(Instant::now()),
            ..Default::default()
        };
        let fp = sample_fingerprint();
        let disk = MetadataCacheDisk::capture(&cache, fp.clone());
        save_cache("conn-1", &disk, None).unwrap();

        let loaded = load_cache("conn-1", &fp).expect("cache loads back");
        assert_eq!(loaded.tables.len(), cache.tables.len());
        assert_eq!(loaded.columns.len(), cache.columns.len());
        assert_eq!(loaded.sequences.len(), cache.sequences.len());
        assert_eq!(loaded.fks.len(), cache.fks.len());
        // Fresh data survives: a just-fetched cache is not stale under any TTL.
        assert!(!loaded.is_stale(Some(std::time::Duration::from_secs(3600))));
        // A loaded cache is marked as already consulted.
        assert!(loaded.disk_loaded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_fingerprint_mismatch_is_a_miss() {
        let _guard = env_lock();
        let dir = staged_cache_dir("fingerprint");
        let cache = MetadataCache {
            tables: vec![TableId {
                owner: "SCOTT".to_string(),
                name: "EMP".to_string(),
                kind: TableKind::Table,
            }],
            fetched_at: Some(Instant::now()),
            ..Default::default()
        };
        let fp = sample_fingerprint();
        save_cache(
            "conn-1",
            &MetadataCacheDisk::capture(&cache, fp.clone()),
            None,
        )
        .unwrap();
        // Same fingerprint hits; a changed host/user/filter misses.
        assert!(load_cache("conn-1", &fp).is_some());
        let mut other = fp.clone();
        other.host = "other.example".to_string();
        assert!(load_cache("conn-1", &other).is_none());
        let mut other = fp.clone();
        other.include_system = true;
        assert!(load_cache("conn-1", &other).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_corrupt_file_is_ignored() {
        let _guard = env_lock();
        let dir = staged_cache_dir("corrupt");
        let path = cache_path("conn-1").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not gzip").unwrap();
        assert!(load_cache("conn-1", &sample_fingerprint()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_size_cap_skips_write() {
        let _guard = env_lock();
        let dir = staged_cache_dir("cap");
        let cache = MetadataCache {
            tables: vec![TableId {
                owner: "SCOTT".to_string(),
                name: "EMP".to_string(),
                kind: TableKind::Table,
            }],
            ..Default::default()
        };
        let disk = MetadataCacheDisk::capture(&cache, sample_fingerprint());
        // A 1-byte cap is exceeded → nothing written.
        let written = save_cache("conn-1", &disk, Some(1)).unwrap();
        assert_eq!(written, None);
        assert!(!cache_path("conn-1").unwrap().exists());
        // Uncapped writes and reports the uncompressed size.
        let written = save_cache("conn-1", &disk, None).unwrap();
        assert!(written.unwrap() > 0);
        assert!(cache_path("conn-1").unwrap().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_delete_removes_file() {
        let _guard = env_lock();
        let dir = staged_cache_dir("delete");
        let disk = MetadataCacheDisk::capture(&MetadataCache::default(), sample_fingerprint());
        save_cache("conn-1", &disk, None).unwrap();
        assert!(cache_path("conn-1").unwrap().exists());
        delete_cache("conn-1");
        assert!(!cache_path("conn-1").unwrap().exists());
        // Deleting a missing file is a no-op.
        delete_cache("conn-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_path_rejects_traversal() {
        assert!(cache_path("../escape").is_err());
        assert!(cache_path("a/b").is_err());
        assert!(cache_path("").is_err());
        assert!(cache_path("ok-id").is_ok());
    }
}
