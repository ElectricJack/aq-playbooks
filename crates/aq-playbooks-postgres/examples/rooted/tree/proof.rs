use super::*;

fn candidate_node(hash: &str, bytes: &[u8], expected: Option<&Link>) -> Result<Option<Node>> {
    let parse = || -> Result<(Node, Link)> {
        let node: Node = serde_json::from_slice(bytes)?;
        let link = node.link()?;
        Ok((node, link))
    };
    let (node, actual) = match parse() {
        Ok(candidate) => candidate,
        Err(_) if expected.is_none() => return Ok(None),
        Err(error) => return Err(error),
    };
    let hashes_match = hash_bytes(bytes) == hash && actual.hash == hash;
    if hashes_match && canonical_bytes(&node)? == bytes {
        if expected.is_none_or(|link| link == &actual) {
            return Ok(Some(node));
        }
    }
    // A malformed speculative neighbor must be fetched and checked normally if it becomes required.
    if expected.is_some() {
        return Err("lookahead node integrity".into());
    }
    Ok(None)
}

impl Tree<'_, '_> {
    pub(super) fn fetch_many(&mut self, links: &[Link]) -> Result<()> {
        let expected: BTreeMap<_, _> = links
            .iter()
            .filter(|r| !self.cache.contains_key(&r.hash))
            .map(|r| (r.hash.clone(), r.clone()))
            .collect();
        if expected.is_empty() {
            return Ok(());
        }
        if expected.len() > 4096 {
            return Err("prototype frontier >4096 nodes".into());
        }
        if std::env::var_os("AQ_PB04_LOOKAHEAD").is_some() {
            return self.fetch_lookahead(&expected);
        }
        let keys: Vec<_> = expected.keys().cloned().collect();
        self.work.read_sql_calls += 1;
        let rows = self.tx.query(
            "SELECT hash,payload FROM pb04_nodes WHERE hash=ANY($1)",
            &[&keys],
        )?;
        if rows.len() != keys.len() {
            return Err("missing proof node".into());
        }
        for row in rows {
            self.cache_proof(row, &expected)?;
        }
        Ok(())
    }
    fn cache_proof(&mut self, row: postgres::Row, expected: &BTreeMap<String, Link>) -> Result<()> {
        let hash: String = row.get(0);
        let bytes: Vec<u8> = row.get(1);
        if !self.account_fetch(bytes.len()) {
            return Err("prototype read bound".into());
        }
        let node: Node = serde_json::from_slice(&bytes)?;
        let authentic_bytes = hash_bytes(&bytes) == hash && canonical_bytes(&node)? == bytes;
        if !authentic_bytes || node.link()? != expected[&hash] {
            return Err("proof node integrity".into());
        }
        self.cache.insert(hash, node);
        Ok(())
    }
    // SQL only supplies bounded candidate bytes. Local hash/canonical checks and
    // the ordinary expected-edge walk still establish membership and absence.
    // This isolated experiment does not change the authoritative tree format.
    fn fetch_lookahead(&mut self, expected: &BTreeMap<String, Link>) -> Result<()> {
        let keys: Vec<_> = expected.keys().cloned().collect();
        // Unrequested values may be opaque arrays containing all retained
        // history. Default to map structure only; retain the broad experiment
        // behind a second switch so its measured byte slope stays reproducible.
        let values = std::env::var_os("AQ_PB04_LOOKAHEAD_VALUES").is_some();
        for roots in keys.chunks(128) {
            self.work.read_sql_calls += 1;
            let rows = self.tx.query(
                "WITH RECURSIVE candidates(hash,payload,depth) AS (
                   SELECT hash,payload,0 FROM pb04_nodes WHERE hash=ANY($1)
                   UNION
                   SELECT n.hash,n.payload,c.depth+1 FROM candidates c
                   CROSS JOIN LATERAL (SELECT pb04_probe_json(c.payload) AS body) d
                   CROSS JOIN LATERAL (VALUES
                     (d.body#>>'{Object,hash}'),
                     (CASE WHEN $2 THEN d.body#>>'{Entry,value,hash}' ELSE NULL END),
                     (d.body#>>'{Entry,left,hash}'),
                     (d.body#>>'{Entry,right,hash}')
                   ) child(hash)
                   JOIN pb04_nodes n ON n.hash=child.hash
                   WHERE c.depth<2
                 ) SELECT DISTINCT ON(hash) hash,payload FROM candidates ORDER BY hash LIMIT 4097",
                &[&roots, &values],
            )?;
            if rows.len() > 4096 {
                return Err("prototype lookahead node bound".into());
            }
            for row in rows {
                self.cache_candidate(row, expected)?;
            }
        }
        if expected.keys().any(|hash| !self.cache.contains_key(hash)) {
            return Err("missing lookahead proof root".into());
        }
        Ok(())
    }
    fn cache_candidate(
        &mut self,
        row: postgres::Row,
        expected: &BTreeMap<String, Link>,
    ) -> Result<()> {
        let hash: String = row.get(0);
        let bytes: Vec<u8> = row.get(1);
        let within_budget = self.account_fetch(bytes.len());
        if !within_budget || bytes.len() > 16 * 1024 * 1024 {
            return Err("prototype lookahead byte bound".into());
        }
        if let Some(node) = candidate_node(&hash, &bytes, expected.get(&hash))? {
            self.cache.insert(hash, node);
        }
        Ok(())
    }
}
