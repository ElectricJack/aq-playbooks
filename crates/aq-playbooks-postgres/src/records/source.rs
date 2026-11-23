use super::*;
use postgres::Statement;
use std::panic::Location;
use std::sync::Arc;

/// Prepared once per connection, outside any transaction.
#[derive(Clone)]
pub(crate) struct Statements {
    get: Statement,
    first: Statement,
    after: Statement,
    scan: Statement,
}
impl Statements {
    pub(crate) fn prepare(client: &mut postgres::Client) -> Result<Self, StorageError> {
        let mut p = |sql: &str| client.prepare(sql).map_err(backend);
        Ok(Self {
            get: p(
                "SELECT value FROM aq_playbooks.state_records WHERE namespace=$1 AND field=$2 AND sort_key=$3",
            )?,
            first: p(
                "SELECT sort_key,value FROM aq_playbooks.state_records WHERE namespace=$1 AND field=$2 ORDER BY sort_key LIMIT $3",
            )?,
            after: p(
                "SELECT sort_key,value FROM aq_playbooks.state_records WHERE namespace=$1 AND field=$2 AND sort_key>$3 ORDER BY sort_key LIMIT $4",
            )?,
            scan: p(
                "SELECT sort_key,value FROM aq_playbooks.state_records WHERE namespace=$1 AND field=$2 ORDER BY sort_key",
            )?,
        })
    }
}

impl crate::Connection {
    pub(super) fn record_statements(&mut self) -> Result<Statements, StorageError> {
        if self.statements.is_none() {
            self.statements = Some(Statements::prepare(&mut self.client)?);
        }
        Ok(self.statements.clone().expect("prepared above"))
    }
}

/// The row-locked transaction, lent to the bound State for the plan.
pub(super) struct PgSource {
    // SAFETY: the 'static lifetime is erased from a transaction borrowing the
    // storage connection guard. `Lent` reclaims (or drops) it before that guard
    // is released; after reclamation every access fails and poisons.
    tx: Mutex<Option<ObservedTransaction<'static>>>,
    namespace: String,
    statements: Statements,
    counts: BTreeMap<&'static str, usize>,
    reads: Mutex<u64>,
    fallbacks: Mutex<Vec<String>>,
    poison: Mutex<Option<String>>,
}
impl PgSource {
    pub(super) fn reads(&self) -> u64 {
        *lock(&self.reads)
    }
    pub(super) fn take_fallbacks(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.fallbacks))
    }

    fn rows(
        &self,
        statement: &Statement,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<Vec<postgres::Row>, String> {
        let mut tx = lock(&self.tx);
        let tx = tx
            .as_mut()
            .ok_or_else(|| "transition record source used after the plan".to_string())?;
        tx.query(statement, params).map_err(|e| e.to_string())
    }
    fn path(path: &ContainerPath) -> Result<&'static str, String> {
        if path.outer.is_some() {
            return Err(format!("{path:?}: nested containers are not stored"));
        }
        Ok(path.field)
    }
    fn pairs(
        &self,
        name: &str,
        rows: Vec<postgres::Row>,
    ) -> Result<Vec<(TreeKey, Vec<u8>)>, String> {
        *lock(&self.reads) += rows.len().max(1) as u64;
        rows.into_iter()
            .map(|r| Ok((decode_key(name, r.get::<_, &[u8]>(0))?, r.get(1))))
            .collect()
    }
    pub(super) fn check(&self) -> Result<(), Error> {
        match lock(&self.poison).clone() {
            Some(e) => Err(Error::Storage(backend(format!(
                "transition source failed: {e}"
            )))),
            None => Ok(()),
        }
    }
}
impl RecordSource for PgSource {
    fn get(&self, path: &ContainerPath, key: &TreeKey) -> Result<Option<Vec<u8>>, String> {
        let name = Self::path(path)?;
        let rows = self.rows(
            &self.statements.get,
            &[&self.namespace, &name, &encode_key(key)],
        )?;
        *lock(&self.reads) += 1;
        Ok(rows.into_iter().next().map(|r| r.get(0)))
    }
    fn after(
        &self,
        path: &ContainerPath,
        after: Option<&TreeKey>,
        limit: usize,
    ) -> Result<Vec<(TreeKey, Vec<u8>)>, String> {
        let name = Self::path(path)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = match after {
            None => self.rows(&self.statements.first, &[&self.namespace, &name, &limit])?,
            Some(key) => self.rows(
                &self.statements.after,
                &[&self.namespace, &name, &encode_key(key), &limit],
            )?,
        };
        self.pairs(name, rows)
    }
    fn len(&self, path: &ContainerPath) -> Result<usize, String> {
        let name = Self::path(path)?;
        Ok(self.counts.get(name).copied().unwrap_or(0))
    }
    fn scan(&self, path: &ContainerPath) -> Result<Vec<(TreeKey, Vec<u8>)>, String> {
        let name = Self::path(path)?;
        let rows = self.rows(&self.statements.scan, &[&self.namespace, &name])?;
        self.pairs(name, rows)
    }
    fn fallback(&self, path: &ContainerPath, at: &'static Location<'static>) {
        lock(&self.fallbacks).push(format!("{} at {}:{}", path.field, at.file(), at.line()));
    }
    fn fail(&self, error: String) {
        lock(&self.poison).get_or_insert(error);
    }
}
/// Reclaims the lent transaction on every exit path, before the connection
/// guard (declared earlier) is released.
pub(super) struct Lent<'conn> {
    pub(super) source: Arc<PgSource>,
    borrow: std::marker::PhantomData<&'conn mut postgres::Client>,
}
impl<'conn> Lent<'conn> {
    pub(super) fn new(
        tx: ObservedTransaction<'conn>,
        namespace: &str,
        statements: Statements,
        accounting: &Accounting,
    ) -> Self {
        let source = Arc::new(PgSource {
            // SAFETY: the source requires an owned handle. This guard borrows the
            // connection for 'conn and empties tx on drop or reclaim. Later
            // source reads can only see the empty slot.
            tx: Mutex::new(Some(unsafe {
                std::mem::transmute::<ObservedTransaction<'_>, ObservedTransaction<'static>>(tx)
            })),
            namespace: namespace.to_string(),
            statements,
            counts: accounting.fields.clone(),
            reads: Mutex::new(0),
            fallbacks: Mutex::new(vec![]),
            poison: Mutex::new(None),
        });
        Self {
            source,
            borrow: std::marker::PhantomData,
        }
    }
    pub(super) fn reclaim(self) -> Result<ObservedTransaction<'conn>, Error> {
        lock(&self.source.tx)
            .take()
            .ok_or_else(|| Error::Storage(backend("transition transaction already reclaimed")))
    }
}
impl Drop for Lent<'_> {
    fn drop(&mut self) {
        drop(lock(&self.source.tx).take());
    }
}
