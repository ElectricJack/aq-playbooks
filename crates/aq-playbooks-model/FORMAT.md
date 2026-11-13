# Portable format 1

Artifacts use UTF-8 JSON, no whitespace or BOM, recursively ordered object keys
by Rust string ordering (Unicode scalar order), integer JSON numbers in signed
64-bit or unsigned 64-bit range, and serde_json's string escaping: quotes,
backslashes, standard control escapes, other U+0000–U+001F as lowercase `\u00xx`;
other Unicode is emitted directly. Fractional/exponential numeric values are
rejected; exact domain decimals use bounded strings. Canonical integer zero is `0`. Absent object fields and explicit null remain distinct.
Model fields serialize explicitly, including empty collections and Option nulls;
unknown model fields are rejected. Hashes are `sha256:` plus lowercase hex of
canonical bytes. The artifact digest hashes ArtifactContent; the executable
fingerprint excludes source binding, source mappings, compiler build and purpose.
Canonical bytes of the complete Artifact are the stored artifact blob.

Schemas form a closed bounded subset: null, boolean, bounded i64 integer,
bounded UTF-8 string (optional enum), bounded arrays, closed objects with explicit
required keys, and nullable. No refs, regex, floating point, open objects, dynamic
code, implicit unions or discriminator guessing, or implicit coercion. Schema depth is 32.
String length counts UTF-8 bytes. A schema is a contract, not an authority grant.

Expressions have no side effects and deterministic ordered arrays. References
traverse named roots using object keys or zero-based array-index strings. Missing,
null, and unknown are distinct; even boolean short circuits preserve unresolved
required inputs. Exists maps Missing to false and Known(null) to true; Unknown
stays Unknown. Filter preserves order; map preserves length/order; an item whose
body is Unknown makes either one Unknown at once, and an item whose body is
Missing makes it Missing only once every item is evaluated and none was
Unknown, so a Missing item never hides an Unknown one; difference
preserves left order and rejects missing/duplicate identities in either input.
Collection lexical roots cannot shadow existing roots. Every evaluation has
explicit depth, cumulative node/element work and serialized-byte limits.

`lookup` chooses a value by a key learned at run time, from a literal table,
with a required default:

    {"op":"lookup","key":{"op":"ref","root":"input","path":["kind"]},
     "cases":[{"when":"chore","then":{"op":"literal","value":"simple-high"}},
              {"when":"design","then":{"op":"literal","value":"complex-high"}}],
     "default":{"op":"literal","value":"standard-high"}}

The key is evaluated first. An Unknown key is the result, unchanged; it never
falls through to the default. A Known key selects the case whose `when` equals
it (JSON value equality, which for canonical values is equality of canonical
bytes), or the default when none does. A Missing key, such as an absent
optional field, also selects the default: `lookup(input.hint, [{when: 4, then:
4}], default 0)` is 0 when `hint` is absent. Only the selected expression is
evaluated: the others are not evidence for the result, so an unresolved
reference in an unselected case or default does not propagate. At most 256
cases; each `when` is at most 1024 canonical bytes; no two `when` are
canonically equal. Each `when` must be a value the key can take: of the key's
kind and shape, and in the key's string enum when it has one (an integer key
accepts any integer, a string key any length). The result type is the union of
every case's `then` and the default (string results merge into one enum); past
16 alternatives the lookup is refused. Checked against a target schema, each
result meets the target itself, as a literal does. An empty array, `[]` or an
array expression with no items, has no item type of its own and fits any array
type, also when it is merged with other results.

`let` binds names in order; each binding sees the ones before it, and the body
sees all of them:

    {"op":"let","bindings":[{"name":"a","value":{"op":"literal","value":5}},
                            {"name":"b","value":{"op":"literal","value":7}}],
     "body":{"op":"lookup","key":{"op":"lt","left":{"op":"ref","root":"a"},
                                  "right":{"op":"ref","root":"b"}},
             "cases":[{"when":true,"then":{"op":"ref","root":"b"}}],
             "default":{"op":"ref","root":"a"}}}

is `max(a, b)`, 7, with no arithmetic. Each binding is evaluated exactly once,
however often it is used. A binding that is Unknown makes the whole `let`
Unknown, even when the body never reads it, as `and(false, unknown)` stays
unknown. A Missing binding is bound as Missing, so `exists` reads it as false
and any other use propagates it exactly as the inlined reference would. At
most 16 bindings. A name is a valid identifier and is never `input`, `event`,
`definitions` or `vars`, a step binding or iteration item anywhere in the
playbook, or a name of an enclosing `let` or collection item, including an
earlier binding of the same `let`.

Evaluation stops past depth 32 and past 10 000 elements: one per expression
evaluated, one per lookup case whether selected or not, and a map or filter body
once per item. Validation refuses an expression that uses `lookup` or `let`
when, on its worst path (every map or filter at its `max_items`, the costliest
lookup branch), it could exceed either limit, so such an expression never fails
on them at run time. A 256-case lookup on a plain key, inside a map, therefore
fits a `max_items` of at most 38. The compiler also holds every expression of a new draft
to the depth limit; verifying a stored artifact does not, so artifacts stored
before these rules keep loading.

Diagnostics for both name the place inside the step, for example
`steps.route.input.level.lookup.cases[0].when`: object fields by name, array
items as `[i]`, and every other expression by its `op` followed by its operand.

Both expressions are part of format 1: `FORMAT_VERSION` stays 1, and an
artifact that does not use them keeps its canonical bytes, digest and
executable fingerprint. A runner built before them refuses an artifact that
uses them when it deserializes it (unknown `op`).

Source header syntax is an opening `---aq-playbook` line, a single JSON object
serialized as SourceHeader (may span lines), and a closing `---` line. Normalize
CRLF and bare CR to LF; preserve every other byte, including final newline.
Source digest hashes the complete normalized source bytes, including header.

A bounded explicit `union` schema admits values matching any of 2–16 alternatives.
Every alternative obeys the same recursive depth and payload bounds. Assignability
requires every source alternative to fit the target; it does not guess or coerce
values. This represents named business outcomes with different output shapes.

Join `member_keys` supplies stable declared participant identities. It is either
empty (legacy runtime-ID keys) or has exactly one unique bounded name per member.
A named result is a closed object keyed by those identities. Each value contains
`participant` (the actual child/operation identity), `outcome`, and `output`.
The compiler derives each participant's output from its statically declared
owned child/operation contract. Successful all/any-accepted joins narrow those
contracts to accepted business outcomes. All-success results require every key;
any/timeout results retain optional keys. Downstream ordinary typed references
can read, for example, `joined.review.output.subject`. Engine failure/cancellation
keeps nullable outcome/output outside successful qualification. Membership and
ownership remain runtime-checked; a declared result key is not bearer authority.

Callable dependencies declare `selection`: `exact` uses the captured artifact
hash; `catalog` resolves the named binding at root admission. A catalog allowlist
may constrain acceptable full artifact hashes; an empty allowlist permits the
current interface-compatible version. The captured artifact remains compilation
provenance. Compatible implementations accept every declared caller input and
produce only declared caller outcomes with assignable output schemas. Admission
also validates scope, current caller authority, and the entire selected acyclic
closure against one catalog revision, then freezes exact hashes for all children.
Changing activation cannot retarget an already admitted root or its grandchildren.
