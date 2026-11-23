//! Derive each expression's shape, missing-value evidence and expanded size in one traversal.
use super::*;

#[derive(Clone, Copy)]
pub(super) struct Site<'a, 's> {
    pub(super) at: &'s str,
    pub(super) scope: &'s Scope<'a>,
    pub(super) place: Place<'a, 's>,
}
impl<'a, 'k> Pass<'a, 'k> {
    pub(super) fn derive(&mut self, expr: &'a Expr, site: Site<'a, '_>) -> Out<'a> {
        Expression {
            pass: self,
            expr,
            site,
        }
        .derive()
    }
}

struct Expression<'a, 'k, 'p, 's> {
    pass: &'p mut Pass<'a, 'k>,
    expr: &'a Expr,
    site: Site<'a, 's>,
}
impl<'a> Expression<'a, '_, '_, '_> {
    fn under(&self, field: &str) -> String {
        operand(self.site.at, self.expr, field)
    }
    fn child(&mut self, field: &str, expr: &'a Expr) -> Out<'a> {
        self.scoped(field, expr, self.site.scope)
    }
    fn scoped(&mut self, field: &str, expr: &'a Expr, scope: &Scope<'a>) -> Out<'a> {
        let at = self.under(field);
        self.pass.derive(
            expr,
            Site {
                at: &at,
                scope,
                place: self.site.place,
            },
        )
    }
    fn derive(&mut self) -> Out<'a> {
        let check = self.pass.check;
        match self.expr {
            Expr::Literal { value } => self.literal(value),
            Expr::Ref { root, path } => self.pass.read(root, path, self.site),

            Expr::Object { fields } => self.object(fields),
            Expr::Array { items } => self.array(items),
            Expr::Exists { value } => self.exists(value),
            Expr::Not { value } => check.test(vec![self.child(".value", value)]),

            Expr::And { values } | Expr::Or { values } => check.test(
                values
                    .iter()
                    .enumerate()
                    .map(|(i, e)| self.child(&format!(".values[{i}]"), e))
                    .collect(),
            ),

            Expr::Eq { left, right } | Expr::Lt { left, right } => {
                check.test(vec![self.child(".left", left), self.child(".right", right)])
            }

            Expr::Contains { collection, value } => check.test(vec![
                self.child(".collection", collection),
                self.child(".value", value),
            ]),

            Expr::Map {
                collection,
                item,
                value,
                max_items: _,
            } => self.map(collection, item, value),
            Expr::Filter {
                collection,
                item,
                predicate,
                max_items: _,
            } => self.filter(collection, item, predicate),
            Expr::Difference {
                left,
                right,
                identity,
                max_items: _,
            } => self.difference(left, right, identity),
            Expr::Lookup {
                key,
                cases,
                default,
            } => self.lookup(key, cases, default),
            Expr::Let { bindings, body } => self.let_bindings(bindings, body),
        }
    }
    // Each literal row keeps its own fields and each array its own length.
    fn literal(&mut self, value: &'a Value) -> Out<'a> {
        let check = self.pass.check;
        let carried = check.literal(value);
        Out {
            size: carried.nodes.max(1),
            carried,
            origin: None,
        }
    }
    fn object(&mut self, fields: &'a BTreeMap<String, Expr>) -> Out<'a> {
        let check = self.pass.check;
        let parts = fields
            .iter()
            .map(|(k, e)| (k.clone(), self.child(&format!(".{k}"), e)))
            .collect();
        check.build_value(parts, false)
    }
    fn array(&mut self, items: &'a [Expr]) -> Out<'a> {
        let check = self.pass.check;
        let parts = items
            .iter()
            .enumerate()
            .map(|(i, e)| (i.to_string(), self.child(&format!("[{i}]"), e)))
            .collect();
        check.build_value(parts, true)
    }
    // A Missing operand becomes false at an exists test.
    fn exists(&mut self, value: &'a Expr) -> Out<'a> {
        let check = self.pass.check;
        let tested = self.child(".value", value);
        Out {
            carried: check.opaque(1),
            size: tested.size.saturating_add(1),
            origin: None,
        }
    }
    fn map(&mut self, collection: &'a Expr, item: &'a str, value: &'a Expr) -> Out<'a> {
        let check = self.pass.check;
        let over = self.child(".collection", collection);
        let inner = self.pass.with_item(self.site.scope, item, &over);
        let each = self.scoped(".value", value, &inner);
        // Missing where the collection is, or where the body is for
        // any item; otherwise every item is what the body carries.
        let gaps = join_gaps([over.carried.gaps.clone(), each.carried.gaps.clone()]);
        let mut mapped = each.carried;
        mapped.gaps = None;
        let mut carried = check.items(mapped);
        carried.gaps = gaps;
        Out {
            carried,
            size: over.size.saturating_add(each.size).saturating_add(1),
            origin: None,
        }
    }
    fn filter(&mut self, collection: &'a Expr, item: &'a str, predicate: &'a Expr) -> Out<'a> {
        let check = self.pass.check;
        let over = self.child(".collection", collection);
        let inner = self.pass.with_item(self.site.scope, item, &over);
        let kept = self.scoped(".predicate", predicate, &inner);
        // The items it keeps are items of the collection.
        let mut carried = check.items(check.items_of(&over.carried));
        carried.gaps = join_gaps([over.carried.gaps.clone(), kept.carried.gaps.clone()]);
        Out {
            carried,
            size: over.size.saturating_add(kept.size).saturating_add(1),
            origin: None,
        }
    }
    fn difference(&mut self, left: &'a Expr, right: &'a Expr, identity: &[String]) -> Out<'a> {
        let check = self.pass.check;
        let sides = [
            ("left", self.child(".left", left)),
            ("right", self.child(".right", right)),
        ];
        // The identity is read in every item of both sides, and an
        // item without it stops the run: no `exists` can test it.
        for (side, out) in &sides {
            let mut found = Build::default();
            check.items_of(&out.carried).follow(identity, &mut found);
            if !found.lacks.is_empty() {
                let read = Rc::new(Read {
                    at: self.under(".identity"),
                    written: dotted(side, identity),
                    note: Some(format!(
                        "`{side}` is an item of the {side} collection, and `difference` \
                                 needs the identity of every item"
                    )),
                    identity: true,
                });
                for reason in found.lacks {
                    self.pass.refuse(
                        &read,
                        &reason,
                        ReadUse {
                            key: false,
                            through: None,
                        },
                    );
                }
            }
        }
        let [(_, kept), (_, removed)] = sides;
        // The items it keeps are items of `left`.
        let mut carried = check.items(check.items_of(&kept.carried));
        carried.gaps = join_gaps([kept.carried.gaps.clone(), removed.carried.gaps.clone()]);
        Out {
            carried,
            size: kept.size.saturating_add(removed.size).saturating_add(1),
            origin: None,
        }
    }
    fn lookup(&mut self, key: &'a Expr, cases: &'a [LookupCase], default: &'a Expr) -> Out<'a> {
        let check = self.pass.check;
        // A Missing key takes the default without a trace, so the
        // key is consumed here, inside `exists` or not.
        let keyed = self.child(".key", key);
        self.pass.sink(&keyed.carried.gaps, true);
        let mut size = keyed.size.saturating_add(1);
        let mut result = Build::default();
        let branches = cases
            .iter()
            .enumerate()
            .map(|(i, c)| (format!(".cases[{i}].then"), &c.then, value_nodes(&c.when)))
            .chain([(".default".to_string(), default, 0)]);
        // It is whichever branch is chosen.
        for (field, branch, when) in branches {
            let out = self.child(&field, branch);
            size = size.saturating_add(when).saturating_add(out.size);
            result.all(&out.carried);
        }
        Out {
            carried: check.finish(result),
            size,
            origin: None,
        }
    }
    fn let_bindings(&mut self, bindings: &'a [LetBinding], body: &'a Expr) -> Out<'a> {
        let mut inner = self.site.scope.clone();
        let mut size = 1u64;
        for (i, b) in bindings.iter().enumerate() {
            let at = self.under(&format!(".bindings[{i}].value"));
            let value = self.pass.derive(
                &b.value,
                Site {
                    at: &at,
                    scope: &inner,
                    place: self.site.place,
                },
            );
            size = size.saturating_add(value.size);
            // A Missing value is bound as it is: it is refused where
            // the name is consumed, and `exists` may test the name.
            let note = match &value.origin {
                Some(origin) => format!("is `{origin}`"),
                None => "is computed from other values".to_string(),
            };
            inner.insert(
                &b.name,
                Named {
                    carried: value.carried,
                    origin: value.origin,
                    note: note.into(),
                },
            );
        }
        let out = self.scoped(".body", body, &inner);
        Out {
            carried: out.carried,
            size: size.saturating_add(out.size),
            origin: out.origin,
        }
    }
}
impl<'a> Check<'a> {
    // A test of its operands: a boolean, Missing where any operand is.
    fn test(&self, parts: Vec<Out<'a>>) -> Out<'a> {
        let mut carried = self.opaque(1);
        carried.gaps = join_gaps(parts.iter().map(|o| o.carried.gaps.clone()));
        Out {
            carried,
            size: parts.iter().fold(1u64, |n, o| n.saturating_add(o.size)),
            origin: None,
        }
    }
    // A value built from parts: Missing where any part is.
    fn build_value(&self, parts: Vec<(String, Out<'a>)>, array: bool) -> Out<'a> {
        let size = parts
            .iter()
            .fold(1u64, |n, (_, o)| n.saturating_add(o.size));
        let gaps = join_gaps(parts.iter().map(|(_, o)| o.carried.gaps.clone()));
        let mut carried = self.built(
            parts
                .into_iter()
                .map(|(k, o)| {
                    let mut part = o.carried;
                    part.gaps = None;
                    (k, part)
                })
                .collect(),
            array,
        );
        carried.gaps = gaps;
        Out {
            carried,
            size,
            origin: None,
        }
    }
}
