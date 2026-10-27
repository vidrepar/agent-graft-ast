use super::*;
use std::fs;

#[test]
fn positions_count_utf16_units() {
    let t = "a\nxé😀b\n";
    /* `b` is byte 9: line 2, after x (1), é (1 unit), 😀 (2 units) */
    assert_eq!(to_loc(t, 9), (2, 5));
    assert_eq!(to_byte(t, 2, 5), Some(9));
    assert_eq!(to_byte(t, 1, 1), Some(0));
    assert_eq!(to_byte(t, 2, 1), Some(2));
    assert_eq!(to_byte(t, 3, 1), Some(t.len()));
    assert_eq!(to_byte(t, 1, 9), None);
}

/// A live tsserver for `root`, or None (with a note) when this machine has none.
pub(crate) fn live(root: &Path) -> Option<TsService> {
    match locate(root) {
        Ok(_) => Some(TsService::enabled(root)),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn far() -> std::time::Instant {
    std::time::Instant::now() + std::time::Duration::from_secs(60)
}

#[test]
fn rename_references_and_errors_follow_written_text() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path().canonicalize().unwrap();
    fs::write(r.join("tsconfig.json"), r#"{"compilerOptions":{"strict":true,"noEmit":true}}"#).unwrap();
    fs::write(r.join("a.ts"), "export function hello(n: number) { return n; }\n").unwrap();
    fs::write(r.join("b.ts"), "import { hello } from './a';\nconst x = { hello };\nhello(1);\n").unwrap();
    let Some(ts) = live(&r) else { return };
    let locs = ts.rename(&r.join("a.ts"), (1, 17), false).unwrap().unwrap();
    let mut got: Vec<(String, (u32, u32), String)> =
        locs.iter().map(|l| (l.file.file_name().unwrap().to_string_lossy().into_owned(), l.start, l.prefix.clone())).collect();
    got.sort();
    /* the shorthand property keeps its key: `{ hello: newName }` */
    assert_eq!(
        got,
        vec![
            ("a.ts".into(), (1, 17), String::new()),
            ("b.ts".into(), (1, 10), String::new()),
            ("b.ts".into(), (2, 13), "hello: ".into()),
            ("b.ts".into(), (3, 1), String::new()),
        ]
    );
    let refs = ts.references(&r.join("a.ts"), (1, 17)).unwrap();
    assert_eq!(refs.len(), 4, "{refs:?}");
    assert!(refs.iter().any(|l| l.is_def && l.file.ends_with("a.ts")));
    assert!(ts.errors(&[r.join("b.ts")], far()).unwrap().0.is_empty());
    /* a change on disk reaches tsserver once touched */
    fs::write(r.join("b.ts"), "import { hello } from './a';\nhello('x');\n").unwrap();
    ts.touch(r.join("b.ts"));
    let (errs, _) = ts.errors(&[r.join("b.ts")], far()).unwrap();
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!((errs[0].line, errs[0].code), (2, 2345));
    /* and a killed server comes back on the next request */
    ts.stop();
    assert_eq!(ts.references(&r.join("a.ts"), (1, 17)).unwrap().len(), 3);
}
