//! Follow the shapes and missing values a draft carries through step bindings.
use super::*;

pub(super) struct Binding<'b> {
    pub(super) name: &'b str,
    pub(super) way: &'b str,
}
impl Binding<'_> {
    pub(super) fn held<'a>(
        &self,
        check: &Check<'a>,
        held: &Held<'a>,
        item: &mut dyn FnMut(&'a Expr) -> Carried<'a>,
    ) -> Carried<'a> {
        let (bind, way) = (self.name, self.way);
        match held {
            Held::Shape(schema) => check.shape(schema, way, bind),
            Held::Unbound => check.lacking(&format!("on {way}, `{bind}` is not bound")),
            Held::Unfollowed => check.opaque(1),
            Held::Members(keys) => {
                let parts = keys
                    .iter()
                    .map(|(&key, members)| {
                        let mut build = Build::default();
                        let member = MemberShape {
                            binding: self,
                            key,
                            check,
                        };
                        for value in members {
                            member.add(value, &mut build);
                        }
                        (key.to_string(), check.finish(build))
                    })
                    .collect();
                check.built(parts, false)
            }
            Held::ItemOf(collection) => item(collection),
        }
    }
}
struct MemberShape<'a, 'k> {
    binding: &'k Binding<'k>,
    key: &'k str,
    check: &'k Check<'a>,
}
impl<'a> MemberShape<'a, '_> {
    fn add(&self, member: &Member<'a>, build: &mut Build<'a>) {
        let (bind, way, key, check) = (self.binding.name, self.binding.way, self.key, self.check);
        match member {
            Member::Answer(outcome, output) => {
                let answered = format!("outcome `{outcome}` of member `{key}` on {way}");
                let output = check.shape(output, &answered, &format!("{bind}.{key}.output"));
                build.all(&check.built(
                    vec![
                        ("participant".to_string(), check.opaque(1)),
                        ("outcome".to_string(), check.opaque(1)),
                        ("output".to_string(), output),
                    ],
                    false,
                ));
            }
            Member::Loose(schema) => build.form(Form::Shape {
                schema,
                way: way.into(),
                name: format!("{bind}.{key}").into(),
            }),
            Member::Absent(why) => {
                build.lack(&format!("on {way}, {why}, `{bind}.{key}` is absent"));
            }
        }
    }
}

impl Built<'_> {
    fn missing(&self, key: &str) -> String {
        if self.array {
            format!(
                "position `{key}` is past the end of an array written with {} \
                                 item{}",
                self.values.len(),
                if self.values.len() == 1 { "" } else { "s" }
            )
        } else if self.fields.is_empty() {
            format!("`{key}` is not in an object written with no fields")
        } else {
            format!(
                "`{key}` is not in an object written with the fields {}",
                self.fields.join(", ")
            )
        }
    }
}
impl<'a> Carried<'a> {
    pub(super) fn follow(&self, path: &[String], out: &mut Build<'a>) {
        if path.is_empty() {
            out.all(self);
            return;
        }
        for lack in &self.lacks {
            out.lack(lack);
        }
        out.gaps.push(self.gaps.clone());
        for form in self.forms.iter() {
            form.follow(path, out);
        }
    }
}
impl<'a> Form<'a> {
    fn follow(&self, path: &[String], out: &mut Build<'a>) {
        let key = &path[0];
        let tail = &path[1..];
        match self {
            Form::Shape { schema, way, name } => {
                SchemaPath { path, way, name }.read(schema, 0, out)
            }
            Form::Opaque(nodes) => out.form(Form::Opaque(*nodes)),
            Form::Built(built) => match built.get(key) {
                Some(part) => part.follow(tail, out),
                None => out.lack(&built.missing(key)),
            },
            Form::Items(item) => {
                if key.parse::<usize>().is_ok() {
                    item.follow(tail, out)
                } else {
                    out.form(Form::Opaque(1))
                }
            }
        }
    }

    fn items(&self, out: &mut Build<'a>) {
        match self {
            Form::Shape { schema, way, name } => array_item(schema, way, &format!("{name}[]"), out),
            Form::Opaque(nodes) => out.form(Form::Opaque(*nodes)),
            Form::Built(built) if built.array => {
                for part in &built.values {
                    out.all(part);
                }
            }
            Form::Built(_) => out.form(Form::Opaque(1)),
            Form::Items(item) => out.all(item),
        }
    }
}
struct SchemaPath<'p> {
    path: &'p [String],
    name: &'p Rc<str>,
    way: &'p Rc<str>,
}
impl SchemaPath<'_> {
    fn read<'a>(&self, schema: &'a Schema, depth: usize, out: &mut Build<'a>) {
        let Some(key) = self.path.get(depth) else {
            out.form(Form::Shape {
                schema,
                way: self.way.clone(),
                name: dotted(self.name, self.path).into(),
            });
            return;
        };
        match schema {
            Schema::Union { variants } => {
                for v in variants {
                    self.read(v, depth, out);
                }
            }
            Schema::Object {
                properties,
                required: _,
            } => match properties.get(key) {
                // A field merely optional in one schema is readable.
                Some(next) => self.read(next, depth + 1, out),
                None => out.lack(&no_field(
                    self.way,
                    key,
                    &dotted(self.name, &self.path[..depth]),
                    properties,
                )),
            },
            Schema::Array {
                items,
                max_items: _,
            } => self.read(items, depth + 1, out),
            // Ordinary typing refuses to read inside these.
            Schema::Nullable { inner: _ }
            | Schema::Null
            | Schema::Boolean
            | Schema::Integer { .. }
            | Schema::String { .. } => out.form(Form::Opaque(1)),
        }
    }
}

impl<'a> Check<'a> {
    pub(super) fn items_of(&self, collection: &Carried<'a>) -> Carried<'a> {
        let mut out = Build::default();
        for form in collection.forms.iter() {
            form.items(&mut out);
        }
        // Where the collection is Missing nothing has an item; its own read carries that.
        out.gaps.clear();
        self.finish(out)
    }
}

fn array_item<'a>(schema: &'a Schema, way: &Rc<str>, name: &str, out: &mut Build<'a>) {
    match schema {
        Schema::Array {
            items,
            max_items: _,
        } => out.form(Form::Shape {
            schema: items,
            way: way.clone(),
            name: name.into(),
        }),
        Schema::Union { variants } => {
            for v in variants {
                array_item(v, way, name, out);
            }
        }
        Schema::Object { .. }
        | Schema::Nullable { .. }
        | Schema::Null
        | Schema::Boolean
        | Schema::Integer { .. }
        | Schema::String { .. } => out.form(Form::Opaque(1)),
    }
}
