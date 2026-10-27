//! Structural search and rewrite (ast-grep style), on our own tree-sitter trees.
//!
//! A pattern is code in the target language with metavariables: `$NAME` matches exactly one named node,
//! `$$$NAME` zero or more sibling nodes (arguments, statements), `$_` / `$$$` match without binding. A name
//! used twice must match identical text. Metavariables are swapped for placeholder identifiers before the
//! pattern is parsed (`$A` is not valid Rust or Python), and the pattern is the one named node spanning it.
//! Candidates match when they have the same kind, the same children (comments ignored) and equal leaf text,
//! so string literals and comments that merely contain the pattern text never match. Rust macro bodies
//! (token soup to tree-sitter) are re-parsed as call arguments, so `format!("{}", x.unwrap())` is searched.

use crate::lang::Lang;
use crate::parse::{parse_ranges, parse_tree, reparse, text};
use std::collections::HashMap;
use tree_sitter::{InputEdit, Node, Point, Range, Tree};

/// A metavariable: its name (`_` = binds nothing) and whether it is `$$$` (any number of siblings).
#[derive(Clone, Debug)]
struct Var {
    name: String,
    multi: bool,
}

/// A pattern compiled for one language.
pub(crate) struct Pattern {
    pub(crate) lang: Lang,
    src: String,
    tree: Tree,
    /// byte range of the pattern's node in `src`
    range: (usize, usize),
    /// placeholder identifier -> metavariable
    vars: HashMap<String, Var>,
    /// the pattern's identifier and keyword leaves: a file lacking any of them as a whole word cannot match
    words: Vec<String>,
    /// longest other leaf text (punctuation, literals), when there are no words
    literal: Option<String>,
    /// Rust `token_tree` and `macro_invocation` kind ids (0 elsewhere): macro bodies are searched apart
    macro_kinds: (u16, u16),
}

/// One match: byte range in the file and the text bound to each metavariable name.
pub(crate) struct Match {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) binds: Vec<(String, String)>,
    /// byte ranges of code the match skipped (comments, punctuation and keywords the pattern leaves out)
    /// outside every binding: a replacement would drop them
    pub(crate) dropped: Vec<(usize, usize)>,
}

/// Bindings and skipped ranges while matching; both roll back together on backtracking.
#[derive(Default)]
struct State {
    binds: Vec<(String, String)>,
    dropped: Vec<(usize, usize)>,
    /// byte range each metavariable (`$_` too) took: skipped code inside one is kept, not dropped
    spans: Vec<(usize, usize)>,
}

impl State {
    fn mark(&self) -> (usize, usize, usize) {
        (self.binds.len(), self.dropped.len(), self.spans.len())
    }

    fn reset(&mut self, (b, d, s): (usize, usize, usize)) {
        self.binds.truncate(b);
        self.dropped.truncate(d);
        self.spans.truncate(s);
    }

    /// Skipped ranges outside every metavariable's range.
    fn lost(&self) -> Vec<(usize, usize)> {
        self.dropped.iter().filter(|(a, b)| !self.spans.iter().any(|(s, e)| s <= a && b <= e)).copied().collect()
    }
}

fn metavar_re() -> &'static regex::Regex {
    static RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"\$\$\$([A-Z_][A-Z0-9_]*)?|\$([A-Z_][A-Z0-9_]*)").expect("valid regex"));
    &RE
}

/// Metavariable names a pattern or replacement uses (`_` and anonymous `$$$` excluded).
pub(crate) fn var_names(s: &str) -> Vec<String> {
    metavar_re()
        .captures_iter(s)
        .filter_map(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_string()))
        .filter(|n| n != "_")
        .collect()
}

/// Ways to make a snippet parse on its own, per language: `{}` is the pattern.
fn wrappers(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Rust => &["{}", "fn __gw() { {} }", "type __GW = {};"],
        Lang::Python => &["{}", "{}\ndef __gw(): pass"],
        Lang::Go => &["{}", "func __gw() { {} }", "var __gw = {}", "type __GW {}"],
        Lang::Java => &["{}", "class __GW { {} }", "class __GW { void __gw() { {} } }", "class __GW { Object __gw = {}; }"],
        Lang::CSharp => &["{}", "class __GW { {} }", "class __GW { void __gw() { {} } }", "class __GW { object __gw = {}; }"],
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => &["{}", "({})", "class __GW { {} }"],
        Lang::C => &["{}", "void __gw(void) { {} }", "int __gw = {};"],
        Lang::Cpp => &["{}", "void __gw() { {} }", "auto __gw = {};", "class __GW { {} };"],
        Lang::Php => &["<?php {}", "<?php {};", "<?php class __GW { {} }"],
        Lang::Kotlin => &["{}", "fun __gw() {\n{}\n}", "val __gw = {}", "class __GW {\n{}\n}"],
        Lang::Swift => &["{}", "let __gw = {}", "class __GW { {} }"],
        Lang::Scala => &["{}", "object __GW { {} }", "object __GW { val __gw = {} }", "class __GW { {} }"],
        Lang::Lua => &["{}", "local __gw = {}"],
        Lang::Ruby | Lang::Bash => &["{}"],
    }
}

impl Pattern {
    /// Compile `pattern` for `lang`; Err says why it is not one syntax node there.
    pub(crate) fn compile(pattern: &str, lang: Lang) -> Result<Pattern, String> {
        let mut vars: HashMap<String, Var> = HashMap::new();
        let mut by_name: HashMap<(String, bool), String> = HashMap::new();
        let mut n = 0;
        let body = metavar_re()
            .replace_all(pattern.trim(), |c: &regex::Captures| {
                let multi = c.get(0).is_some_and(|m| m.as_str().starts_with("$$$"));
                let name = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str()).unwrap_or("_").to_string();
                let key = (name.clone(), multi);
                if name != "_" {
                    if let Some(p) = by_name.get(&key) {
                        return p.clone();
                    }
                }
                let p = format!("__gmv{n}");
                n += 1;
                vars.insert(p.clone(), Var { name, multi });
                by_name.insert(key, p.clone());
                p
            })
            .into_owned();
        if body.is_empty() {
            return Err("empty pattern".into());
        }
        /* a metavariable where the grammar wants a string (`import $X from $Y`) parses as a quoted
        placeholder: try the bare ones first, then quoted ones, fewest first */
        for (k, v) in vars.clone() {
            vars.insert(format!("\"{k}\""), v);
        }
        let mut masks: Vec<u32> = (0..1u32 << n.min(6)).collect();
        masks.sort_by_key(|m| m.count_ones());
        let placeholder = regex::Regex::new(r"__gmv(\d+)").expect("valid regex");
        let bodies = masks.into_iter().map(|m| {
            placeholder
                .replace_all(&body, |c: &regex::Captures| {
                    let i: u32 = c[1].parse().unwrap_or(0);
                    if i < 32 && m & (1 << i) != 0 {
                        format!("\"{}\"", &c[0])
                    } else {
                        c[0].to_string()
                    }
                })
                .into_owned()
        });
        for (body, w) in bodies.flat_map(|b| wrappers(lang).iter().map(move |w| (b.clone(), w))) {
            let at = w.find("{}").expect("wrapper has a slot");
            let src = format!("{}{body}{}", &w[..at], &w[at + 2..]);
            let Some(tree) = parse_tree(lang, src.as_bytes()) else { continue };
            if tree.root_node().has_error() {
                continue;
            }
            let Some(node) = tree.root_node().named_descendant_for_byte_range(at, at + body.len()) else { continue };
            if node.start_byte() != at || node.end_byte() != at + body.len() {
                continue;
            }
            if vars.contains_key(text(node, src.as_bytes())) {
                return Err("a lone metavariable matches everything; add code around it".into());
            }
            let range = (node.start_byte(), node.end_byte());
            let macro_kinds = match lang {
                Lang::Rust => {
                    let l = lang.ts_language();
                    (l.id_for_node_kind("token_tree", true), l.id_for_node_kind("macro_invocation", true))
                }
                _ => (0, 0),
            };
            let mut p = Pattern { lang, src, tree, range, vars, words: Vec::new(), literal: None, macro_kinds };
            p.words = p.word_leaves();
            p.literal = if p.words.is_empty() { p.longest_leaf() } else { None };
            return Ok(p);
        }
        Err(format!("pattern does not parse as one {} syntax node", lang.name()))
    }

    fn root(&self) -> Node<'_> {
        self.tree.root_node().named_descendant_for_byte_range(self.range.0, self.range.1).expect("compiled range")
    }

    fn var(&self, p: Node) -> Option<&Var> {
        self.vars.get(text(p, self.src.as_bytes()))
    }

    fn word_leaves(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut stack = vec![self.root()];
        while let Some(n) = stack.pop() {
            if self.var(n).is_some() {
                continue;
            }
            if n.child_count() == 0 {
                let t = text(n, self.src.as_bytes());
                if !t.is_empty() && t.bytes().all(|b| is_word_byte(self.lang, b)) && !out.iter().any(|w| w == t) {
                    out.push(t.to_string());
                }
            }
            let mut c = n.walk();
            stack.extend(n.children(&mut c));
        }
        /* rarest first is unknowable here; longest first rejects most files soonest */
        out.sort_by_key(|w| std::cmp::Reverse(w.len()));
        out
    }

    /// True when a match can only lie inside a function body: the pattern names a word and is not itself an
    /// item, declaration or definition (those can sit outside functions or contain them).
    pub(crate) fn fits_in_functions(&self) -> bool {
        let k = self.root().kind();
        !self.words.is_empty() && !["item", "declaration", "definition", "class", "module", "program", "source_file", "impl"].iter().any(|w| k.contains(w))
    }

    /// The parts of `src` worth parsing: the outermost functions (`spans`, 1-based line ranges from the
    /// index) holding every whole-word occurrence of the pattern's longest word. None when an occurrence
    /// lies outside every function, or the functions cover most of the file anyway.
    pub(crate) fn regions(&self, src: &[u8], spans: &[(u32, u32)]) -> Option<Vec<Range>> {
        let word = self.words.first()?.as_bytes();
        let starts: Vec<usize> = std::iter::once(0).chain(memchr::memchr_iter(b'\n', src).map(|i| i + 1)).collect();
        let mut picked: Vec<(u32, u32)> = Vec::new();
        for at in memchr::memmem::find_iter(src, word) {
            let whole = !at.checked_sub(1).is_some_and(|j| is_word_byte(self.lang, src[j])) && !src.get(at + word.len()).is_some_and(|b| is_word_byte(self.lang, *b));
            if !whole {
                continue;
            }
            let line = starts.partition_point(|&s| s <= at) as u32;
            let span = spans.iter().find(|(a, b)| *a <= line && line <= *b)?;
            if picked.last() != Some(span) {
                picked.push(*span);
            }
        }
        picked.sort_unstable();
        picked.dedup();
        let mut out: Vec<Range> = Vec::new();
        for (a, b) in picked {
            let line_start = starts[(a - 1) as usize];
            let indent = src[line_start..].iter().take_while(|c| **c == b' ' || **c == b'\t').count();
            let start = line_start + indent;
            let end = starts.get(b as usize).map_or(src.len(), |s| s - 1);
            if out.last().is_some_and(|r| r.end_byte >= start) {
                continue;
            }
            out.push(Range {
                start_byte: start,
                end_byte: end,
                start_point: Point { row: (a - 1) as usize, column: indent },
                end_point: Point { row: (b - 1) as usize, column: end - starts[(b - 1) as usize] },
            });
        }
        let covered: usize = out.iter().map(|r| r.end_byte - r.start_byte).sum();
        (!out.is_empty() && covered * 10 < src.len() * 7).then_some(out)
    }

    /// Every match inside `ranges` only; None when those ranges do not parse cleanly on their own.
    pub(crate) fn find_in_ranges(&self, src: &str, ranges: &[Range]) -> Option<(Vec<Match>, Tree)> {
        let tree = parse_ranges(self.lang, src.as_bytes(), ranges, None)?;
        if tree.root_node().has_error() {
            return None;
        }
        Some((self.matches_in(&tree, src), tree))
    }

    /// False when `src` cannot contain a match: a word leaf is missing as a whole word, or the literal is.
    pub(crate) fn may_match(&self, src: &[u8]) -> bool {
        if let Some(lit) = &self.literal {
            return memchr::memmem::find(src, lit.as_bytes()).is_some();
        }
        self.words.iter().all(|w| {
            memchr::memmem::find_iter(src, w.as_bytes()).any(|i| {
                let before = i.checked_sub(1).map(|j| src[j]);
                let after = src.get(i + w.len()).copied();
                !before.is_some_and(|b| is_word_byte(self.lang, b)) && !after.is_some_and(|b| is_word_byte(self.lang, b))
            })
        })
    }

    fn longest_leaf(&self) -> Option<String> {
        let mut best: Option<String> = None;
        let mut stack = vec![self.root()];
        while let Some(n) = stack.pop() {
            if self.var(n).is_some() {
                continue;
            }
            if n.child_count() == 0 {
                let t = text(n, self.src.as_bytes());
                if t.len() >= 2 && best.as_ref().is_none_or(|b| t.len() > b.len()) {
                    best = Some(t.to_string());
                }
            }
            let mut c = n.walk();
            stack.extend(n.children(&mut c));
        }
        best
    }

    /// Every match in `src` (a file in this pattern's language), in document order, nested ones included.
    pub(crate) fn find_all(&self, src: &str) -> Vec<Match> {
        self.find_all_tree(src).0
    }

    /// `find_all`, also returning the file's tree (None when the literal prefilter skipped parsing).
    pub(crate) fn find_all_tree(&self, src: &str) -> (Vec<Match>, Option<Tree>) {
        if !self.may_match(src.as_bytes()) {
            return (Vec::new(), None);
        }
        let tree = parse_tree(self.lang, src.as_bytes());
        let out = tree.as_ref().map(|t| self.matches_in(t, src)).unwrap_or_default();
        (out, tree)
    }

    /// Every match in a tree already parsed from `src`.
    pub(crate) fn matches_in(&self, tree: &Tree, src: &str) -> Vec<Match> {
        let mut out = Vec::new();
        self.walk(tree.root_node(), src, 0, &mut out);
        out.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        out.dedup_by(|a, b| a.start == b.start && a.end == b.end);
        out
    }

    /// Walk a tree whose `src` starts at byte `base` of the file.
    fn walk(&self, root: Node, src: &str, base: usize, out: &mut Vec<Match>) {
        let pat = self.root();
        let kind = pat.kind_id();
        let (tt, mi) = self.macro_kinds;
        /* one cursor for the whole walk: a cursor or a parent() per node costs more than the matching */
        let mut c = root.walk();
        loop {
            let n = c.node();
            let k = n.kind_id();
            if k == kind {
                let mut st = State::default();
                if self.node(pat, n, src, &mut st) {
                    let dropped = st.lost().iter().map(|(a, b)| (base + a, base + b)).collect();
                    out.push(Match { start: base + n.start_byte(), end: base + n.end_byte(), binds: st.binds, dropped });
                }
            }
            if tt != 0 && k == mi {
                let mut cc = n.walk();
                for body in n.children(&mut cc).filter(|b| b.kind_id() == tt) {
                    self.macro_body(body, src, base, out);
                }
            }
            /* token soup: never descended (macro bodies are re-parsed above) */
            let descend = !(tt != 0 && k == tt);
            if descend && c.goto_first_child() {
                continue;
            }
            while !c.goto_next_sibling() {
                if !c.goto_parent() {
                    return;
                }
            }
        }
    }

    /// A Rust macro's `( ... )` re-parsed as call arguments; nothing when it is not expression-shaped.
    fn macro_body(&self, tt: Node, src: &str, base: usize, out: &mut Vec<Match>) {
        let (a, b) = (tt.start_byte() + 1, tt.end_byte().saturating_sub(1));
        let Some(inner) = src.get(a..b) else { return };
        const PRE: &str = "fn __gw() { __gm(";
        let wrapped = format!("{PRE}{inner}); }}");
        let Some(tree) = parse_tree(Lang::Rust, wrapped.as_bytes()) else { return };
        if tree.root_node().has_error() {
            return;
        }
        let mut found = Vec::new();
        self.walk(tree.root_node(), &wrapped, 0, &mut found);
        for m in found {
            if m.start >= PRE.len() && m.end <= PRE.len() + inner.len() {
                let at = |x: usize| x - PRE.len() + a + base;
                let dropped = m.dropped.iter().map(|&(s, e)| (at(s), at(e))).collect();
                out.push(Match { start: at(m.start), end: at(m.end), binds: m.binds, dropped });
            }
        }
    }

    /// Bind `name` to `t`, or check it equals the earlier binding.
    fn bind(name: &str, t: &str, span: (usize, usize), st: &mut State) -> bool {
        st.spans.push(span);
        let binds = &mut st.binds;
        if name == "_" {
            return true;
        }
        match binds.iter().find(|(n, _)| n == name) {
            Some((_, prev)) => prev == t,
            None => {
                binds.push((name.to_string(), t.to_string()));
                true
            }
        }
    }

    fn node(&self, p: Node, c: Node, src: &str, st: &mut State) -> bool {
        if let Some(v) = self.var(p) {
            return !v.multi && c.is_named() && Self::bind(&v.name, text(c, src.as_bytes()), (c.start_byte(), c.end_byte()), st);
        }
        if p.kind_id() != c.kind_id() {
            return false;
        }
        let (pk, ck) = (kids(p), kids(c));
        if pk.is_empty() {
            return ck.is_empty() && text(p, self.src.as_bytes()) == text(c, src.as_bytes());
        }
        /* comments between the children the pattern names are not part of any binding */
        let mut cur = c.walk();
        st.dropped.extend(c.children(&mut cur).filter(|k| k.is_extra()).map(|k| (k.start_byte(), k.end_byte())));
        self.seq(&pk, &ck, src, st)
    }

    fn is_sep(&self, p: Node) -> bool {
        !p.is_named() && matches!(text(p, self.src.as_bytes()), "," | ";")
    }

    fn multi_at(&self, pk: &[Node], i: usize) -> bool {
        pk.get(i).and_then(|p| self.var(*p)).is_some_and(|v| v.multi)
    }

    /// Children in order; `$$$` takes any run of siblings, and an empty `$$$` takes its separator with it.
    /// Unnamed nodes of the code (punctuation, keywords) that the pattern leaves out are skipped, as in
    /// ast-grep's default strictness: `throw $X` matches `throw e;`. Everything the pattern names must match.
    fn seq(&self, pk: &[Node], ck: &[Node], src: &str, st: &mut State) -> bool {
        let Some(&p) = pk.first() else {
            /* the pattern is done: whatever follows (an `else`, more imported names, `;`) is skipped as in
            ast-grep, and recorded so a rewrite never deletes it */
            st.dropped.extend(ck.iter().map(|c| (c.start_byte(), c.end_byte())));
            return true;
        };
        let mark = st.mark();
        if let Some(v) = self.var(p).filter(|v| v.multi).cloned() {
            for k in 0..=ck.len() {
                let t = if k == 0 { "" } else { &src[ck[0].start_byte()..ck[k - 1].end_byte()] };
                let span = if k == 0 { (0, 0) } else { (ck[0].start_byte(), ck[k - 1].end_byte()) };
                if Self::bind(&v.name, t, span, st) && self.seq(&pk[1..], &ck[k..], src, st) {
                    return true;
                }
                st.reset(mark);
                if k == 0 && pk.len() > 1 && self.is_sep(pk[1]) {
                    if Self::bind(&v.name, "", (0, 0), st) && self.seq(&pk[2..], ck, src, st) {
                        return true;
                    }
                    st.reset(mark);
                }
            }
            return false;
        }
        if self.is_sep(p) && self.multi_at(pk, 1) {
            let name = self.var(pk[1]).map(|v| v.name.clone()).unwrap_or_default();
            if Self::bind(&name, "", (0, 0), st) && self.seq(&pk[2..], ck, src, st) {
                return true;
            }
            st.reset(mark);
        }
        if let Some(&c) = ck.first() {
            if self.node(p, c, src, st) && self.seq(&pk[1..], &ck[1..], src, st) {
                return true;
            }
            st.reset(mark);
            if !c.is_named() {
                st.dropped.push((c.start_byte(), c.end_byte()));
                if self.seq(pk, &ck[1..], src, st) {
                    return true;
                }
            }
        }
        st.reset(mark);
        false
    }
}

/// A byte that continues an identifier (`$` too in JS/TS and Java, where `$x` is one name).
fn is_word_byte(lang: Lang, b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 || (b == b'$' && (lang.is_js_family() || lang == Lang::Java))
}

/// Children that carry syntax (comments and other extras skipped).
fn kids(n: Node) -> Vec<Node> {
    let mut c = n.walk();
    n.children(&mut c).filter(|k| !k.is_extra()).collect()
}

/// Fill a replacement template with a match's bindings; an empty `$$$` drops its `,` separator too.
/// Err names a metavariable the pattern does not bind.
pub(crate) fn render(template: &str, m: &Match) -> Result<String, String> {
    let mut out = String::new();
    let mut at = 0;
    for c in metavar_re().captures_iter(template) {
        let whole = c.get(0).expect("match");
        let mut lead = &template[at..whole.start()];
        let name = c.get(1).or_else(|| c.get(2)).map(|x| x.as_str()).unwrap_or("_");
        let Some((_, t)) = m.binds.iter().find(|(n, _)| n == name) else {
            return Err(format!("replacement uses ${name}, which the pattern does not bind"));
        };
        at = whole.end();
        if t.is_empty() && whole.as_str().starts_with("$$$") {
            let trimmed = lead.trim_end();
            if let Some(l) = trimmed.strip_suffix(',') {
                lead = l;
            } else if let Some(rest) = template[at..].strip_prefix(',') {
                at = template.len() - rest.trim_start().len();
            }
        }
        out.push_str(lead);
        out.push_str(t);
    }
    out.push_str(&template[at..]);
    Ok(out)
}

/// Row/column of byte `at` in `src`.
fn point_at(src: &str, at: usize) -> Point {
    let before = &src.as_bytes()[..at];
    let row = memchr::memchr_iter(b'\n', before).count();
    let col = at - memchr::memrchr(b'\n', before).map_or(0, |i| i + 1);
    Point { row, column: col }
}

/// Where `start` ends up after inserting `text` at it.
fn advance(start: Point, text: &str) -> Point {
    match memchr::memrchr(b'\n', text.as_bytes()) {
        Some(i) => Point { row: start.row + memchr::memchr_iter(b'\n', text.as_bytes()).count(), column: text.len() - i - 1 },
        None => Point { row: start.row, column: start.column + text.len() },
    }
}

/// The tree for `new` (= `old` with `edits` applied, non-overlapping) by re-parsing only the edited regions.
/// `ranges`: the tree covers only these ranges of `old` (see `Pattern::regions`); the edits lie inside them.
pub(crate) fn reparse_edited(lang: Lang, mut tree: Tree, old: &str, edits: &[(usize, usize, String)], new: &str, ranges: Option<&[Range]>) -> Option<Tree> {
    let mut sorted: Vec<&(usize, usize, String)> = edits.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.0));
    /* last edit first, so every earlier byte and point is still valid in `old` */
    for (s, e, t) in sorted {
        let start_position = point_at(old, *s);
        tree.edit(&InputEdit {
            start_byte: *s,
            old_end_byte: *e,
            new_end_byte: s + t.len(),
            start_position,
            old_end_position: point_at(old, *e),
            new_end_position: advance(start_position, t),
        });
    }
    let Some(ranges) = ranges else { return reparse(lang, new.as_bytes(), &tree) };
    /* the same ranges in `new`: each boundary moves by the growth of the edits before it */
    let shift = |at: usize| -> usize {
        let grow: isize = edits.iter().filter(|(s, _, _)| *s < at).map(|(s, e, t)| t.len() as isize - (e - s) as isize).sum();
        (at as isize + grow) as usize
    };
    let moved: Vec<Range> = ranges
        .iter()
        .map(|r| {
            let (s, e) = (shift(r.start_byte), shift(r.end_byte));
            Range { start_byte: s, end_byte: e, start_point: point_at(new, s), end_point: point_at(new, e) }
        })
        .collect();
    let t = parse_ranges(lang, new.as_bytes(), &moved, Some(&tree))?;
    (!t.root_node().has_error()).then_some(t)
}

/// Outermost matches only, so no two edits overlap (input sorted by start, longest first).
pub(crate) fn outermost(ms: &[Match]) -> Vec<&Match> {
    let mut keep: Vec<&Match> = Vec::new();
    for m in ms {
        if keep.last().is_none_or(|k| m.start >= k.end) {
            keep.push(m);
        }
    }
    keep
}
