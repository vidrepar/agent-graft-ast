//! Zero-overhead interceptors: refuse lockfiles, binaries and secrets before any parsing.

use once_cell::sync::Lazy;
use regex::bytes::Regex;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Lockfile,
    Binary,
    Secret(&'static str),
    TooLarge(u64),
    NotFound,
    OutsideRoot,
    Hidden,
}

impl Refusal {
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Lockfile => "REFUSED_LOCKFILE",
            Refusal::Binary => "REFUSED_BINARY",
            Refusal::Secret(_) => "REFUSED_SECRET",
            Refusal::TooLarge(_) => "REFUSED_TOO_LARGE",
            Refusal::NotFound => "NOT_FOUND",
            Refusal::OutsideRoot => "REFUSED_OUTSIDE_ROOT",
            Refusal::Hidden => "REFUSED_HIDDEN",
        }
    }

    pub fn reason(&self) -> String {
        match self {
            Refusal::Lockfile => "Lockfiles are generated dependency manifests; read the package manifest instead.".into(),
            Refusal::Binary => "Binary content carries no useful tokens for an agent.".into(),
            Refusal::Secret(why) => format!("File likely contains credentials ({why}); contents withheld."),
            Refusal::TooLarge(n) => format!("File is {n} bytes, above the read limit; request a symbol instead."),
            Refusal::NotFound => "Path does not exist or is not indexed.".into(),
            Refusal::OutsideRoot => "Path resolves outside the indexed repository root.".into(),
            Refusal::Hidden => "Hidden and tool directories (.git, node_modules, ...) are not served.".into(),
        }
    }
}

pub const MAX_FILE_BYTES: u64 = 1_500_000;

const LOCKFILES: &[&str] = &[
    "package-lock.json", "pnpm-lock.yaml", "yarn.lock", "bun.lockb", "bun.lock", "Cargo.lock", "poetry.lock",
    "Pipfile.lock", "uv.lock", "Gemfile.lock", "composer.lock", "go.sum", "flake.lock", "npm-shrinkwrap.json",
    "pdm.lock", "mix.lock", "Podfile.lock", "packages.lock.json",
];

const BINARY_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "tiff", "avif", "pdf", "zip", "gz", "tgz", "bz2", "xz", "7z",
    "tar", "wasm", "so", "dylib", "dll", "exe", "bin", "o", "a", "lib", "class", "jar", "pyc", "pyo", "woff", "woff2",
    "ttf", "otf", "eot", "mp3", "mp4", "mov", "avi", "webm", "wav", "flac", "ogg", "sqlite", "sqlite3", "db", "redb",
    "psd", "lockb", "node", "rlib",
];

const SECRET_EXTS: &[&str] = &["tfstate", "tfvars", "pem", "key", "p12", "pfx", "keystore", "jks", "asc", "gpg", "kdbx"];

const SECRET_NAMES: &[&str] = &[
    ".npmrc", ".pypirc", ".netrc", ".pgpass", "credentials", "credentials.json", "id_rsa", "id_dsa", "id_ecdsa",
    "id_ed25519", ".htpasswd", ".git-credentials", ".dockercfg", ".envrc", "secrets.json", "secrets.yaml", "secrets.yml", "service-account.json",
];

/// Name-only check: no IO, runs before the file is opened.
pub fn check_path(rel: &str) -> Option<Refusal> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    if LOCKFILES.contains(&name) {
        return Some(Refusal::Lockfile);
    }
    let lower = name.to_ascii_lowercase();
    if lower == ".env" || lower.starts_with(".env.") || lower.ends_with(".env") || lower == ".dev.vars" {
        let template = [".example", ".sample", ".template", ".dist"].iter().any(|s| lower.ends_with(s));
        if !template {
            return Some(Refusal::Secret("dotenv file"));
        }
    }
    if SECRET_NAMES.contains(&lower.as_str()) {
        return Some(Refusal::Secret("credential file name"));
    }
    if let Some(ext) = lower.rsplit_once('.').map(|(_, e)| e) {
        if SECRET_EXTS.contains(&ext) {
            return Some(Refusal::Secret("key/certificate file"));
        }
        if BINARY_EXTS.contains(&ext) {
            return Some(Refusal::Binary);
        }
    }
    None
}

static SECRET_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"AKIA[0-9A-Z]{16}",
        r"|ghp_[A-Za-z0-9]{36}",
        r"|github_pat_[A-Za-z0-9_]{60,}",
        r"|xox[baprs]-[A-Za-z0-9-]{10,}",
        r"|sk-(?:proj-|ant-)?[A-Za-z0-9_-]{32,}",
        r"|sk_live_[A-Za-z0-9]{20,}",
        r"|AIza[0-9A-Za-z_-]{35}",
    ))
    .unwrap()
});

/// Pattern 0 is the private-key marker; the rest prefilter `SECRET_RE`.
static SECRET_LITERALS: Lazy<aho_corasick::AhoCorasick> = Lazy::new(|| {
    aho_corasick::AhoCorasick::new(["PRIVATE KEY-----", "AKIA", "ghp_", "github_pat_", "xox", "sk-", "sk_live_", "AIza"])
        .expect("static patterns")
});

/// Content check: NUL-byte sniff (memchr, SIMD) plus credential signatures.
pub fn check_content(bytes: &[u8]) -> Option<Refusal> {
    let head = &bytes[..bytes.len().min(8192)];
    if memchr::memchr(0, head).is_some() {
        return Some(Refusal::Binary);
    }
    /* one pass for every literal; the regex only runs on files that contain a token prefix */
    let mut token_prefix = false;
    for m in SECRET_LITERALS.find_overlapping_iter(bytes) {
        if m.pattern().as_usize() == 0 {
            return Some(Refusal::Secret("embedded private key"));
        }
        token_prefix = true;
    }
    if token_prefix && SECRET_RE.is_match(bytes) {
        return Some(Refusal::Secret("embedded API token"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_lockfiles_by_name() {
        assert_eq!(check_path("pnpm-lock.yaml"), Some(Refusal::Lockfile));
        assert_eq!(check_path("crates/x/Cargo.lock"), Some(Refusal::Lockfile));
        assert_eq!(check_path("src/lock.rs"), None);
    }

    #[test]
    fn refuses_secret_files_but_not_templates() {
        assert!(matches!(check_path(".env"), Some(Refusal::Secret(_))));
        assert!(matches!(check_path("apps/web/.env.local"), Some(Refusal::Secret(_))));
        assert_eq!(check_path(".env.example"), None);
        assert!(matches!(check_path("certs/server.pem"), Some(Refusal::Secret(_))));
        assert!(matches!(check_path("home/.ssh/id_ed25519"), Some(Refusal::Secret(_))));
    }

    #[test]
    fn refuses_binaries() {
        assert_eq!(check_path("logo.PNG"), Some(Refusal::Binary));
        assert_eq!(check_content(b"abc\0def"), Some(Refusal::Binary));
        assert_eq!(check_content(b"fn main() {}"), None);
    }

    #[test]
    fn detects_embedded_credentials() {
        let key = b"const k = \"-----BEGIN RSA PRIVATE KEY-----\\nMII\";";
        assert!(matches!(check_content(key), Some(Refusal::Secret(_))));
        let aws = b"aws_key = 'AKIAABCDEFGHIJKLMNOP'";
        assert!(matches!(check_content(aws), Some(Refusal::Secret(_))));
        assert_eq!(check_content(b"let sk = skip();"), None);
    }
}
