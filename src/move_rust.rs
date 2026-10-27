//! `move_symbol` for Rust: move a top-level fn/struct/enum/union/trait/const/static/type alias (with the
//! `impl` blocks of a moved type in the same file) to another module file of the same crate.
//!
//! - Module paths come from the `mod` tree (lib.rs/main.rs, foo.rs, foo/mod.rs). A missing destination file
//!   is created and declared with `mod` in its parent module.
//! - Every path to the item is pointed at the new module: `use` trees (grouped ones are split), `crate::a::f`,
//!   `super::f`, `self::f`, `$crate::a::f` in macros and paths from other crates (`my_crate::a::f`). Modules
//!   that named it unqualified (the old module, `use a::*` importers) get a `use`.
//! - The moved code takes the `use` items it needs; names of its old module it uses are imported, and widened
//!   to `pub(crate)` when the new module could not see them. The item itself (and the private fields and
//!   methods of a moved type) become `pub(crate)` when code outside the new module's subtree needs them.
//! - Refused, with the reason: another crate as destination, `#[path]` modules, `macro_rules!` macros of the
//!   old file used by the moved code, glob re-exports (`pub use a::*`) the item would silently leave.
use crate::engine::{esc, Engine};

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn move_rust(&mut self, fi: u32, si: u32, name: &str, dest: &str, dest_exists: bool, dry_run: bool, target: &str, to_file: &str) -> String {
        let line = self.files[fi as usize].ex.symbols[si as usize].start_line;
        let impact = self.impact(fi, si);
        let plan = match self.plan_rust(fi, name, dest, dest_exists, line) {
            Ok(p) => p,
            Err(("clash", msg)) => {
                let src_rel = self.files[fi as usize].path.clone();
                let open = format!("<move_symbol target=\"{}\" to=\"{}\" applied=\"false\">", esc(name), esc(dest));
                let body = format!(
                    "<choose>\n{}</choose>\n",
                    esc(&format!("A pick another to_file (recommended): `{name}` {msg}\nB rename one of them first -> rename(target=\"{src_rel}:{name}\", new_name), then move_symbol again\n"))
                );
                return self.finish(&open, "</move_symbol>", &body, 4000, "");
            }
            Err((code, msg)) => return self.move_err(target, to_file, code, &msg),
        };
        let n_files = plan.writes.len() + usize::from(plan.created.is_some());
        let applied = if dry_run {
            "false"
        } else {
            let mut res = Ok(());
            if let Some((rel, content)) = &plan.created {
                let abs = self.root.join(rel);
                let r = abs.parent().map(std::fs::create_dir_all).unwrap_or(Ok(())).and_then(|_| std::fs::OpenOptions::new().write(true).create_new(true).open(&abs).and_then(|mut f| std::io::Write::write_all(&mut f, content.as_bytes())));
                match r {
                    Ok(()) => {
                        self.refresh(rel);
                    }
                    Err(e) => res = Err(e.to_string()),
                }
            }
            if let Err(e) = res.and_then(|_| self.commit_files(plan.writes)) {
                return self.move_err(target, to_file, "write_failed", &e);
            }
            "true"
        };
        let mut body = Self::lines_block(&plan.lines);
        if !plan.widened.is_empty() {
            body.push_str(&format!(
                "<visibility reason=\"now pub(crate): code outside the new module uses them\">{}</visibility>\n",
                esc(&plan.widened.join(", "))
            ));
        }
        body.push_str(if applied == "true" {
            "<next>Done: files are written and re-indexed. Run cargo check.</next>\n"
        } else {
            "<next>Plan only: call again without dry_run to apply.</next>\n"
        });
        let open = format!(
            "<move_symbol target=\"{}\" from=\"{}\" to=\"{}\" applied=\"{applied}\" files=\"{n_files}\" impact=\"{}\" why=\"{}\">",
            esc(name),
            esc(&plan.from),
            esc(dest),
            impact.level,
            esc(&impact.reason)
        );
        self.finish(&open, "</move_symbol>", &body, 4000, "")
    }
}

#[cfg(test)]
#[path = "move_rust_tests.rs"]
mod tests;
