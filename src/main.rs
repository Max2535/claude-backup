//! Incremental mirror of Claude Code config/state to another drive, and back.
//!
//! claude-backup [backup|restore] [--config FILE] [--dir PATH] [--dry-run]
//! Config: --config > claude-backup.conf next to the exe (created with defaults if missing).
//! Backup dir: --dir > $CLAUDE_BACKUP_DIR > `dir` in config.

use std::env;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

const CONF_NAME: &str = "claude-backup.conf";

/// Written next to the exe on first run; also the built-in defaults.
const DEFAULT_CONF: &str = r"# claude-backup config: one `key = value` per line, # starts a comment.

# Where backups go. A relative path is resolved against this file's folder,
# so `dir = .` keeps the backup next to the exe (handy on a USB drive).
dir = D:\claude-backup

# What to back up: item = <folder name inside backup> | <path on this machine>
# %VAR% is expanded on each machine, so restoring under another username still works.
item = .claude | %USERPROFILE%\.claude
item = .claude.json | %USERPROFILE%\.claude.json
item = appdata-caveman | %APPDATA%\caveman

# Not backed up: path relative to an item root, `/` separated, `*` = any one segment.
# Delete the .credentials.json line to also back up the login token (stored as plain text!).
skip = .credentials.json
skip = cache
skip = debug
skip = telemetry
skip = shell-snapshots
skip = paste-cache
skip = session-env
skip = statsig
skip = ide
skip = daemon.lock
skip = daemon.log
skip = daemon.status.json
skip = plugins/.trash
skip = jobs/*/tmp
";

/// Filename prefixes always skipped at any depth (temp files, per-PID markers).
const SKIP_PREFIX: &[&str] = &[".claude.json.tmp.", ".caveman-active."];

struct Config {
    dir: PathBuf,
    /// (name inside backup dir, unexpanded local path)
    items: Vec<(String, String)>,
    skip: Vec<String>,
}

struct Opts {
    skip: Vec<String>,
    dry_run: bool,
    backup: bool,
}

#[derive(Default)]
struct Stats {
    copied: u64,
    unchanged: u64,
    skipped: u64,
    bytes: u64,
    errors: Vec<String>,
}

impl Stats {
    fn err(&mut self, p: &Path, e: impl std::fmt::Display) {
        let msg = format!("{}: {e}", p.display());
        eprintln!("error: {msg}");
        self.errors.push(msg);
    }
}

fn parse(text: &str, base: &Path) -> Result<Config, String> {
    let (mut dir, mut items, mut skip) = (None, Vec::new(), Vec::new());
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let err = |m: &str| format!("config line {}: {m}: `{line}`", i + 1);
        let (k, v) = line.split_once('=').ok_or_else(|| err("expected key = value"))?;
        let v = v.trim();
        match k.trim() {
            "dir" if !v.is_empty() => dir = Some(base.join(v)),
            "item" => {
                let (name, path) = v.split_once('|').ok_or_else(|| err("expected name | path"))?;
                let name = name.trim();
                let plain = matches!(
                    Path::new(name).components().collect::<Vec<_>>()[..],
                    [Component::Normal(_)]
                );
                if !plain {
                    return Err(err("item name must be a single folder name"));
                }
                items.push((name.to_string(), path.trim().to_string()));
            }
            "skip" if !v.is_empty() => skip.push(v.replace('\\', "/").trim_matches('/').to_string()),
            _ => return Err(err("unknown key or empty value")),
        }
    }
    let dir = dir.ok_or("config: `dir` is missing")?;
    if items.is_empty() {
        return Err("config: no `item` lines, nothing to back up".into());
    }
    Ok(Config { dir, items, skip })
}

/// Replace %VAR% with environment values; unset var is an error, not an empty string.
fn expand(s: &str) -> Result<PathBuf, String> {
    let (mut out, mut rest) = (String::new(), s);
    while let Some(a) = rest.find('%') {
        let len = rest[a + 1..].find('%').ok_or_else(|| format!("unclosed % in `{s}`"))?;
        let name = &rest[a + 1..a + 1 + len];
        let val = env::var(name).map_err(|_| format!("env var %{name}% not set (in `{s}`)"))?;
        out.push_str(&rest[..a]);
        out.push_str(&val);
        rest = &rest[a + len + 2..];
    }
    out.push_str(rest);
    Ok(out.into())
}

fn load(explicit: Option<PathBuf>) -> Result<(PathBuf, Config), String> {
    let path = match &explicit {
        Some(p) => p.clone(),
        None => env::current_exe()
            .map_err(|e| format!("cannot locate exe: {e}"))?
            .with_file_name(CONF_NAME),
    };
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound && explicit.is_none() => {
            fs::write(&path, DEFAULT_CONF).map_err(|e| format!("{}: {e}", path.display()))?;
            println!("created default config: {}", path.display());
            DEFAULT_CONF.to_string()
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let cfg = parse(&text, path.parent().unwrap_or(Path::new(".")))?;
    Ok((path, cfg))
}

/// Skip rules apply to backup only; restore brings back whatever the backup holds.
fn skipped(rel: &Path, o: &Opts) -> bool {
    let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    let name = parts.last().copied().unwrap_or("");
    let matches = |pat: &String| {
        let pat: Vec<&str> = pat.split('/').collect();
        pat.len() == parts.len() && pat.iter().zip(&parts).all(|(p, s)| *p == "*" || p == s)
    };
    SKIP_PREFIX.iter().any(|p| name.starts_with(p)) || (o.backup && o.skip.iter().any(matches))
}

/// Size + mtime check. 2s tolerance covers FAT/exFAT timestamp granularity.
fn needs_copy(src: &fs::Metadata, dst: &Path) -> bool {
    let Ok(d) = fs::metadata(dst) else { return true };
    if d.len() != src.len() {
        return true;
    }
    match (src.modified(), d.modified()) {
        (Ok(a), Ok(b)) => diff(a, b) > Duration::from_secs(2),
        _ => true,
    }
}

fn diff(a: SystemTime, b: SystemTime) -> Duration {
    a.duration_since(b).or_else(|_| b.duration_since(a)).unwrap_or_default()
}

fn copy_file(src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    // Read-only sources (git pack files) yield read-only copies: clear the flag
    // so we can overwrite next run and set mtime now.
    make_writable(dst);
    fs::copy(src, dst)?;
    make_writable(dst);
    // Keep mtime equal so the next run sees it as unchanged.
    fs::OpenOptions::new().write(true).open(dst)?.set_modified(meta.modified()?)
}

fn make_writable(p: &Path) {
    if let Ok(m) = fs::metadata(p) {
        let mut perm = m.permissions();
        if perm.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            let _ = fs::set_permissions(p, perm);
        }
    }
}

fn sync(src: &Path, dst: &Path, rel: &Path, o: &Opts, st: &mut Stats) {
    let meta = match fs::symlink_metadata(src) {
        Ok(m) => m,
        Err(e) => return st.err(src, e),
    };
    // Follow links/junctions (e.g. skills -> ~/.agents/skills) and store real
    // copies, so restore works without the link target. Skip links to an ancestor.
    let meta = if meta.is_symlink() {
        let target = fs::canonicalize(src);
        let here = src.parent().and_then(|p| fs::canonicalize(p).ok());
        match (target, here) {
            (Ok(t), Some(h)) if h.starts_with(&t) => {
                println!("skip looping link: {}", src.display());
                st.skipped += 1;
                return;
            }
            (Ok(_), _) => match fs::metadata(src) {
                Ok(m) => m,
                Err(e) => return st.err(src, e),
            },
            (Err(e), _) => {
                println!("skip broken link: {} ({e})", src.display());
                st.skipped += 1;
                return;
            }
        }
    } else {
        meta
    };
    if meta.is_dir() {
        let entries = match fs::read_dir(src) {
            Ok(r) => r,
            Err(e) => return st.err(src, e),
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    st.err(src, e);
                    continue;
                }
            };
            let name = entry.file_name();
            let r = rel.join(&name);
            if skipped(&r, o) {
                st.skipped += 1;
                continue;
            }
            sync(&entry.path(), &dst.join(&name), &r, o, st);
        }
    } else if !needs_copy(&meta, dst) {
        st.unchanged += 1;
    } else if o.dry_run {
        println!("would copy: {}", src.display());
        st.copied += 1;
        st.bytes += meta.len();
    } else {
        match copy_file(src, dst, &meta) {
            Ok(()) => {
                st.copied += 1;
                st.bytes += meta.len();
            }
            Err(e) => st.err(src, e),
        }
    }
}

/// Put this exe + its config into the backup dir so the new machine can
/// restore without Rust.
fn copy_tool(dir: &Path, conf: &Path, st: &mut Stats) {
    let Ok(exe) = env::current_exe() else { return };
    for (src, name) in [(exe.as_path(), exe.file_name()), (conf, Some(CONF_NAME.as_ref()))] {
        let target = dir.join(name.unwrap_or_default());
        if fs::canonicalize(src).ok() == fs::canonicalize(&target).ok() {
            continue;
        }
        if let Err(e) = fs::create_dir_all(dir).and_then(|_| fs::copy(src, &target)) {
            st.err(&target, e);
        }
    }
    let readme = dir.join("README.md");
    if fs::read_to_string(&readme).ok().as_deref() != Some(README) {
        if let Err(e) = fs::write(&readme, README) {
            st.err(&readme, e);
        }
    }
}

const README: &str = include_str!("../README.md");

fn run(args: &[String]) -> Result<Stats, String> {
    let (mut backup, mut dry_run) = (true, false);
    let mut conf = None;
    let mut dir = env::var_os("CLAUDE_BACKUP_DIR").map(PathBuf::from);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "backup" => backup = true,
            "restore" => backup = false,
            "--config" => conf = Some(it.next().ok_or("--config needs a file")?.into()),
            "--dir" => dir = Some(it.next().ok_or("--dir needs a path")?.into()),
            "--dry-run" => dry_run = true,
            "-h" | "--help" => {
                println!(
                    "claude-backup [backup|restore] [--config FILE] [--dir PATH] [--dry-run]\n\
                     config: --config > {CONF_NAME} next to the exe (auto-created)\n\
                     dir:    --dir > $CLAUDE_BACKUP_DIR > `dir` in config"
                );
                return Ok(Stats::default());
            }
            other => return Err(format!("unknown argument: {other} (see --help)")),
        }
    }
    let (conf_path, cfg) = load(conf)?;
    let dir = dir.unwrap_or(cfg.dir);
    let o = Opts { skip: cfg.skip, dry_run, backup };
    let mut st = Stats::default();

    println!("config: {}", conf_path.display());
    if backup {
        println!("backup -> {}", dir.display());
    } else {
        if !dir.exists() {
            return Err(format!("backup dir not found: {}", dir.display()));
        }
        println!("restore <- {}  (close all Claude Code sessions first)", dir.display());
    }

    for (name, path) in &cfg.items {
        let local = expand(path)?;
        let saved = dir.join(name);
        let (src, dst) = if backup { (&local, &saved) } else { (&saved, &local) };
        if !src.exists() {
            println!("missing, skip: {}", src.display());
            continue;
        }
        sync(src, dst, Path::new(""), &o, &mut st);
    }
    if backup && !dry_run {
        copy_tool(&dir, &conf_path, &mut st);
    }
    Ok(st)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(st) => {
            println!(
                "copied {} ({:.1} MB), unchanged {}, skipped {}, errors {}",
                st.copied,
                st.bytes as f64 / 1_048_576.0,
                st.unchanged,
                st.skipped,
                st.errors.len()
            );
            if st.errors.is_empty() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_and_rejects_bad_lines() {
        let c = parse(DEFAULT_CONF, Path::new("X:\\tool")).unwrap();
        assert_eq!(c.dir, PathBuf::from(r"D:\claude-backup"));
        assert_eq!(c.items.len(), 3);
        assert!(c.skip.iter().any(|s| s == "jobs/*/tmp"));

        let rel = parse("dir = .\nitem = a | %USERPROFILE%\\a\nskip = x\\y\\", Path::new("E:\\bk")).unwrap();
        assert_eq!(rel.dir, Path::new("E:\\bk").join("."));
        assert_eq!(rel.skip, ["x/y"]);

        assert!(parse("dir = D:\\x\nbogus = 1\nitem = a | b", Path::new(".")).is_err());
        assert!(parse("dir = D:\\x\nitem = ..\\a | b", Path::new(".")).is_err());
        assert!(parse("dir = D:\\x", Path::new(".")).is_err());

        env::set_var("CB_TEST_HOME", r"C:\Users\New");
        assert_eq!(expand(r"%CB_TEST_HOME%\.claude").unwrap(), PathBuf::from(r"C:\Users\New\.claude"));
        assert!(expand("%CB_TEST_UNSET_XYZ%\\a").is_err());
        assert!(expand("%oops").is_err());
    }

    #[test]
    fn backup_skips_noise_and_is_incremental() {
        let root = env::temp_dir().join(format!("claude-backup-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (src, dst) = (root.join("src"), root.join("dst"));
        for f in [
            "settings.json",
            "projects/C--Users-X/a.jsonl",
            "plugins/cache/p/plugin.json", // nested "cache" must be kept
            "cache/junk",
            "jobs/abc/tmp/big.bin",
            ".credentials.json",
            "x/.caveman-active.123",
        ] {
            let p = src.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, f).unwrap();
        }
        let ro = src.join("settings.json");
        let mut perm = fs::metadata(&ro).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&ro, perm).unwrap();
        let skip = parse(DEFAULT_CONF, Path::new(".")).unwrap().skip;
        let o = Opts { skip: skip.clone(), dry_run: false, backup: true };

        let mut st = Stats::default();
        sync(&src, &dst, Path::new(""), &o, &mut st);
        assert!(st.errors.is_empty(), "{:?}", st.errors);
        assert_eq!(st.copied, 3);
        assert!(dst.join("plugins/cache/p/plugin.json").exists());
        assert!(!dst.join("cache").exists());
        assert!(!dst.join("jobs/abc/tmp").exists());
        assert!(!dst.join(".credentials.json").exists());
        assert!(!dst.join("x/.caveman-active.123").exists());

        let mut st2 = Stats::default();
        sync(&src, &dst, Path::new(""), &o, &mut st2);
        assert_eq!((st2.copied, st2.unchanged), (0, 3));

        // Changed read-only file must overwrite its read-only copy.
        make_writable(&ro);
        fs::write(&ro, "changed!").unwrap();
        let mut st3 = Stats::default();
        sync(&src, &dst, Path::new(""), &o, &mut st3);
        assert!(st3.errors.is_empty(), "{:?}", st3.errors);
        assert_eq!(st3.copied, 1);

        // Restore ignores skip rules: credentials come back if they were backed up.
        fs::write(dst.join(".credentials.json"), "tok").unwrap();
        let back = root.join("back");
        let r = Opts { skip, dry_run: false, backup: false };
        sync(&dst, &back, Path::new(""), &r, &mut Stats::default());
        assert!(back.join(".credentials.json").exists());

        fs::remove_dir_all(&root).unwrap();
    }
}
