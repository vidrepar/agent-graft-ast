//! Which calls of a method a rename or signature change edits, by receiver type. When several
//! unrelated types define the method's name, only calls known to reach the target are edited; the
//! rest (receiver type unknown) are left as they are and listed for the model to decide.

use super::Target;
use crate::engine::{esc, Engine};
use crate::lang::Lang;
use crate::parse::{text, SymKind};
use tree_sitter::Node;

/// The calls of a method target in one file, as (start line, rank among same-named calls on that line).
#[derive(Default)]
pub(crate) struct Sites {
    /// calls whose receiver type picks another definition: never edited
    pub(crate) other: Vec<(u32, usize)>,
    /// Some when other types define the name too: the only calls that may be edited
    pub(crate) strict: Option<Vec<(u32, usize)>>,
    /// the same two lists for calls written in macro tokens (`vec![x.f()]`), ranked among themselves
    pub(crate) macro_other: Vec<(u32, usize)>,
    pub(crate) macro_strict: Option<Vec<(u32, usize)>>,
}

impl Sites {
    /// May the macro call keyed `k` be edited? None = not decided here (left alone and listed).
    pub(crate) fn macro_call(&self, k: Option<(u32, usize)>) -> Option<bool> {
        match (k, &self.macro_strict) {
            (Some(k), _) if self.macro_other.contains(&k) => Some(false),
            (Some(k), Some(ok)) => ok.contains(&k).then_some(true),
            (None, Some(_)) => None,
            (_, None) => Some(true),
        }
    }
}

/// `recv.name(..)` written in macro tokens, as extraction records them: (method token, argument token
/// tree, (receiver line, rank among these calls on that line)), in document order.
pub(crate) fn macro_method_calls<'t>(root: Node<'t>, src: &[u8], name: &str) -> Vec<(Node<'t>, Node<'t>, (u32, usize))> {
    let mut found: Vec<(Node, Node, u32)> = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "token_tree" {
            let kids: Vec<Node> = n.children(&mut n.walk()).collect();
            for w in kids.windows(4) {
                if matches!(w[0].kind(), "identifier" | "self")
                    && text(w[1], src) == "."
                    && w[2].kind() == "identifier"
                    && text(w[2], src) == name
                    && w[3].kind() == "token_tree"
                    && text(w[3], src).starts_with('(')
                {
                    found.push((w[2], w[3], w[0].start_position().row as u32 + 1));
                }
            }
        }
        stack.extend(n.named_children(&mut n.walk()));
    }
    found.sort_by_key(|f| f.0.start_byte());
    let (mut line, mut rank) = (0, 0);
    found
        .into_iter()
        .map(|(m, a, l)| {
            rank = if l == line { rank + 1 } else { 0 };
            line = l;
            (m, a, (l, rank))
        })
        .collect()
}

impl Engine {
    /// Owner names of the methods named like the target that belong to no type related to it.
    fn unrelated_owners(&self, t: &Target) -> Vec<String> {
        let owners: Vec<&str> = t.defs.iter().filter_map(|&(f, s)| self.owner_name(f, s)).collect();
        let mut out: Vec<String> = Vec::new();
        for &(f, s) in self.graph.by_name.get(&t.name).into_iter().flatten() {
            let fr = &self.files[f as usize];
            if t.defs.contains(&(f, s)) || fr.ex.symbols[s as usize].kind != SymKind::Method || fr.path.ends_with(".d.ts") {
                continue;
            }
            if !crate::context::same_family(fr.lang, t.lang) {
                continue;
            }
            let Some(o) = self.owner_name(f, s) else { continue };
            /* an override or a trait's implementation is the same family, not another type */
            let related = owners.iter().any(|t| self.graph.type_reaches(o, t) || self.graph.type_reaches(t, o));
            if !related && !out.iter().any(|x| x == o) {
                out.push(o.to_string());
            }
        }
        out
    }

    /// Unrelated types define the method's name too, in a language whose receivers are typed: only calls
    /// known to reach the target are edited, anywhere in the repo.
    pub(crate) fn strict_target(&self, t: &Target) -> bool {
        let typed_lang = matches!(t.lang, Lang::Rust | Lang::Python) || t.lang.is_js_family();
        t.method && typed_lang && !self.unrelated_owners(t).is_empty()
    }

    /// Classify the calls of `t` in `file`; see `Sites`.
    pub(crate) fn method_sites(&self, t: &Target, file: u32) -> Sites {
        let shared = self.graph.by_name.get(&t.name).is_some_and(|v| v.iter().any(|d| !t.defs.contains(d)));
        if !t.method || !shared {
            return Sites::default();
        }
        let strict = self.strict_target(t);
        let owners: Vec<&str> = t.defs.iter().filter_map(|&(f, s)| self.owner_name(f, s)).collect();
        let (mut sites, mut target, mut macro_target) = (Sites::default(), Vec::new(), Vec::new());
        /* calls in macro tokens are ranked apart: they have no call node to line up with */
        let (mut line, mut rank, mut mline, mut mrank) = (0, 0, 0, 0);
        for c in self.files[file as usize].ex.calls.iter().filter(|c| c.callee == t.name) {
            let key = if c.in_macro {
                mrank = if c.line == mline { mrank + 1 } else { 0 };
                mline = c.line;
                (c.line, mrank)
            } else {
                rank = if c.line == line { rank + 1 } else { 0 };
                line = c.line;
                (c.line, rank)
            };
            let Some(ty) = self.graph.receiver_type(&self.files, file, c) else { continue };
            let reaches = match self.graph.typed_targets(&self.files, file, c) {
                Some(v) if v.is_empty() => continue,
                Some(v) => v.iter().any(|d| t.defs.contains(d)),
                /* a receiver of a type defined elsewhere (`Vec`, `Map`) has the target only through a trait the repo implements for it */
                None => owners.iter().any(|o| self.graph.type_reaches(&ty, o)),
            };
            match (c.in_macro, reaches) {
                (false, true) => target.push(key),
                (false, false) => sites.other.push(key),
                (true, true) => macro_target.push(key),
                (true, false) => sites.macro_other.push(key),
            }
        }
        if strict {
            sites.strict = Some(target);
            sites.macro_strict = Some(macro_target);
        }
        sites
    }

    /// `<check>`: uses of the target's name left unedited because nothing says which type's method
    /// they call. `unsure` = (path, lines).
    pub(crate) fn unsure_block(&self, t: &Target, unsure: &[(String, Vec<u32>)]) -> String {
        if unsure.is_empty() {
            return String::new();
        }
        let mut types: Vec<String> = t.defs.iter().filter_map(|&(f, s)| self.owner_name(f, s)).map(str::to_string).collect();
        types.dedup();
        types.extend(self.unrelated_owners(t));
        types.truncate(8);
        let n: usize = unsure.iter().map(|u| u.1.len()).sum();
        format!(
            "<check reason=\"receiver type unknown and {} define `{}`: left unchanged, edit the ones that mean {}\" sites=\"{n}\">\n{}</check>\n",
            esc(&types.join(", ")),
            esc(&t.name),
            esc(types.first().map(String::as_str).unwrap_or("the target")),
            Self::lines_block(unsure)
        )
    }
}
