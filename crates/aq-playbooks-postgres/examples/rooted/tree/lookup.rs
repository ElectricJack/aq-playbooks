use super::*;

enum Lookup {
    Found(Link),
    Missing(Link),
    Absent,
}

impl Tree<'_, '_> {
    // Returns the first uncached node required to establish one exact path.
    // The SQL result never decides absence: local verified edges do.
    fn missing_path(&self, root: &Link, path: &[&str]) -> Result<Option<Link>> {
        let mut current = root.clone();
        for key in path {
            let Some(node) = self.cache.get(&current.hash) else {
                return Ok(Some(current));
            };
            if node.link()? != current {
                return Err("cached root integrity".into());
            }
            let Node::Object(map) = node.clone() else {
                return Err("not object".into());
            };
            let next = match self.cached_child(map, key)? {
                Lookup::Found(link) => link,
                Lookup::Missing(link) => return Ok(Some(link)),
                Lookup::Absent => return Ok(None),
            };
            current = next;
        }
        // Target metadata is already authenticated by its parent. Do not fetch
        // opaque history leaves merely to read count/byte quota metadata.
        Ok(None)
    }
    fn cached_child(&self, mut map: Option<Link>, key: &str) -> Result<Lookup> {
        for depth in 0..64 {
            let Some(link) = map else { break };
            let Some(node) = self.cache.get(&link.hash) else {
                return Ok(Lookup::Missing(link));
            };
            if node.link()?.ne(&link) {
                return Err("cached map integrity".into());
            }
            let Node::Entry {
                key: k,
                value,
                left,
                right,
            } = node
            else {
                return Err("not entry".into());
            };
            match key.cmp(k.as_str()) {
                std::cmp::Ordering::Equal => {
                    return Ok(Lookup::Found(value.clone()));
                }
                std::cmp::Ordering::Less => map = left.clone(),
                std::cmp::Ordering::Greater => map = right.clone(),
            }
            if depth == 63 {
                return Err("prototype lookup depth".into());
            }
        }
        Ok(Lookup::Absent)
    }
    pub fn prefetch_paths(&mut self, root: &Link, paths: &[Vec<&str>]) -> Result<()> {
        if paths.len() > 128 || paths.iter().any(|p| p.len() > 8) {
            return Err("prototype path set bound".into());
        }
        for _ in 0..512 {
            let missing: Vec<_> = paths
                .iter()
                .map(|p| self.missing_path(root, p))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();
            if missing.is_empty() {
                return Ok(());
            }
            self.fetch_many(&missing)?;
        }
        Err("prototype path proof depth".into())
    }
    /// Explicit bounded subtree audit. Used for whole installed catalog reads,
    /// and OUTSIDE measurement for cold full-State differential audits.
    pub fn prefetch_subtree(&mut self, root: &Link) -> Result<()> {
        let mut frontier = vec![root.clone()];
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..256 {
            if frontier.is_empty() {
                return Ok(());
            }
            for chunk in frontier.chunks(4096) {
                self.fetch_many(chunk)?;
            }
            let next = self.descendants(frontier, &mut seen)?;
            if seen.len() > 262144 {
                return Err("prototype subtree node bound".into());
            }
            frontier = next;
        }
        Err("prototype subtree depth bound".into())
    }
    fn descendants(
        &mut self,
        frontier: Vec<Link>,
        seen: &mut std::collections::BTreeSet<String>,
    ) -> Result<Vec<Link>> {
        let mut next = vec![];
        for link in frontier {
            if !seen.insert(link.hash.clone()) {
                continue;
            }
            match self.read(&link)? {
                Node::Object(map) => next.extend(map),
                Node::Entry {
                    value, left, right, ..
                } => {
                    next.push(value);
                    next.extend(left);
                    next.extend(right)
                }
                Node::Leaf(_) => {}
            }
        }
        Ok(next)
    }
}
