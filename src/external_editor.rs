//! Opening scripts in another editor (Studio Settings > External Editor).
//!
//! A script that comes from a file of the open project folder opens that
//! file: project_sync.rs already writes edits to it and reads changes back.
//! Any other script is mirrored: every script of the place (outside the
//! project's files) is written to
//! `<cache>/rustudio/external/<place>/<Service>/<…>/<Name>[.server|.client].luau`
//! with a `sourcemap.json` in Rojo's format beside them, so luau-lsp can
//! resolve `require`s and the editor can open the whole folder. The mirror
//! syncs both ways:
//! - a file saved in the editor sets the script's Source (one undo step)
//!   once the watcher has been quiet for mech::PROJECT_DEBOUNCE_MS;
//! - Source changed in rustudio, and scripts added, moved or deleted, are
//!   written to the folder every SCAN_EVERY.
//!
//! Like project files, changes made during Play wait until Play stops.
//! Studio has no external-editor setting; this is a rustudio addition.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rbx_dom_weak::types::{Ref, Variant};
use rbx_dom_weak::WeakDom;
use serde_json::{json, Value as J};

use crate::app::Editor;

/// How often rustudio-side changes are written to the mirror folder.
const SCAN_EVERY: Duration = Duration::from_secs(1);

pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    /// The command; the first word is the program.
    pub command: &'static str,
}

/// Editors with a known command line. `{folder}` first opens the folder as
/// the workspace (so luau-lsp finds sourcemap.json), then the file.
pub const PRESETS: &[Preset] = &[
    Preset { id: "vscode", name: "Visual Studio Code", command: "code {folder} --goto {file}:{line}" },
    Preset { id: "vscodium", name: "VSCodium", command: "codium {folder} --goto {file}:{line}" },
    Preset { id: "cursor", name: "Cursor", command: "cursor {folder} --goto {file}:{line}" },
    Preset { id: "zed", name: "Zed", command: "zed {folder} {file}:{line}" },
    // Arch's zed package names the program zeditor.
    Preset { id: "zeditor", name: "Zed (zeditor)", command: "zeditor {folder} {file}:{line}" },
    Preset { id: "sublime", name: "Sublime Text", command: "subl {folder} {file}:{line}" },
    Preset { id: "neovim", name: "Neovim (in a terminal)", command: "xdg-terminal-exec nvim +{line} {file}" },
];

/// Splits a command line into words: spaces separate, "…" and '…' group,
/// a backslash escapes the next character outside single quotes.
pub fn split_command(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"') | None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                has = true;
            }
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                has = true;
            }
            (None, c) if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                has = false;
            }
            (None, c) => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The command's words with `{file}`, `{line}` and `{folder}` filled in
/// (inside each word, so paths with spaces stay one argument).
pub fn fill(command: &str, file: &Path, line: usize, folder: &Path) -> Vec<String> {
    split_command(command)
        .into_iter()
        .map(|w| {
            w.replace("{file}", &file.to_string_lossy())
                .replace("{folder}", &folder.to_string_lossy())
                .replace("{line}", &line.to_string())
        })
        .collect()
}

/// Whether the command's program exists (a path, or a name on PATH).
pub fn program_found(command: &str) -> bool {
    let Some(prog) = split_command(command).into_iter().next() else { return false };
    thread_local! {
        static CACHE: std::cell::RefCell<HashMap<String, (Instant, bool)>> = Default::default();
    }
    if let Some(found) = CACHE.with(|c| {
        c.borrow().get(&prog).filter(|(at, _)| at.elapsed() < Duration::from_secs(5)).map(|(_, f)| *f)
    }) {
        return found;
    }
    let found = find_program(&prog).is_some();
    CACHE.with(|c| c.borrow_mut().insert(prog, (Instant::now(), found)));
    found
}

fn find_program(prog: &str) -> Option<PathBuf> {
    let p = Path::new(prog);
    if p.components().count() > 1 {
        return p.is_file().then(|| p.to_path_buf());
    }
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT".into()).split(';').map(str::to_owned).collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| {
        exts.iter().map(|e| dir.join(format!("{prog}{e}"))).find(|c| c.is_file())
    })
}

/// Starts the editor and leaves it running (a thread reaps it when it exits).
fn launch(args: &[String], cwd: &Path) -> Result<(), String> {
    let Some((prog, rest)) = args.split_first() else { return Err("the command is empty".into()) };
    // Windows: `code` and friends are .cmd scripts, which only cmd runs.
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(prog).args(rest);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = std::process::Command::new(prog);
        c.args(rest);
        c
    };
    cmd.current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("{prog}: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn is_script(class: &str) -> bool {
    matches!(class, "Script" | "LocalScript" | "ModuleScript")
}

/// A name usable as a file name on every system.
fn file_name(name: &str) -> String {
    let s: String =
        name.chars().map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c }).collect();
    let s = s.trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() || s == "." || s == ".." {
        "_".into()
    } else {
        s
    }
}

fn source_of(dom: &WeakDom, r: Ref) -> Option<&str> {
    match dom.get_by_ref(r)?.properties.get(&rbx_dom_weak::ustr("Source")) {
        Some(Variant::String(s)) => Some(s),
        _ => None,
    }
}

/// Where each script goes in the mirror (relative), and the sourcemap.
/// Scripts `skip` says no to (project files) are left out. Same-named
/// siblings get `~2`, `~3`… in tree order.
fn layout(dom: &WeakDom, place: &str, skip: &dyn Fn(Ref) -> bool) -> (Vec<(Ref, PathBuf)>, J) {
    fn walk(
        dom: &WeakDom,
        r: Ref,
        rel: &Path,
        skip: &dyn Fn(Ref) -> bool,
        out: &mut Vec<(Ref, PathBuf)>,
    ) -> Vec<J> {
        let Some(inst) = dom.get_by_ref(r) else { return Vec::new() };
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut nodes = Vec::new();
        for &k in inst.children() {
            let Some(child) = dom.get_by_ref(k) else { continue };
            let base = file_name(&child.name);
            let n = seen.entry(base.to_lowercase()).or_insert(0);
            *n += 1;
            let name = if *n == 1 { base } else { format!("{base}~{n}") };
            let class = child.class.as_str();
            let mut file = None;
            if is_script(class) && !skip(k) {
                let suffix = match class {
                    "Script" => ".server",
                    "LocalScript" => ".client",
                    _ => "",
                };
                let p = rel.join(format!("{name}{suffix}.luau"));
                file = Some(p.to_string_lossy().replace('\\', "/"));
                out.push((k, p));
            }
            let kids = walk(dom, k, &rel.join(&name), skip, out);
            if file.is_some() || !kids.is_empty() {
                let mut node = json!({ "name": child.name, "className": class });
                if let Some(f) = file {
                    node["filePaths"] = json!([f]);
                }
                if !kids.is_empty() {
                    node["children"] = J::Array(kids);
                }
                nodes.push(node);
            }
        }
        nodes
    }
    let mut out = Vec::new();
    let kids = walk(dom, dom.root_ref(), Path::new(""), skip, &mut out);
    (out, json!({ "name": place, "className": "DataModel", "children": kids }))
}

/// The mirror folder of one open place.
pub struct Mirror {
    pub dir: PathBuf,
    place: String,
    doc_id: u64,
    files: HashMap<Ref, PathBuf>,
    by_path: HashMap<PathBuf, Ref>,
    /// Text as last written or read, per file.
    disk: HashMap<PathBuf, String>,
    sourcemap: Vec<u8>,
    _watcher: Option<notify::RecommendedWatcher>,
    events: Option<mpsc::Receiver<notify::Result<notify::Event>>>,
    pending: HashSet<PathBuf>,
    pending_since: Option<Instant>,
    scanned: Instant,
}

impl Mirror {
    fn new(dir: PathBuf, place: &str, doc_id: u64) -> Result<Self, String> {
        // Start clean: files of an earlier run would point at other scripts.
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut m = Mirror {
            dir,
            place: place.to_string(),
            doc_id,
            files: HashMap::new(),
            by_path: HashMap::new(),
            disk: HashMap::new(),
            sourcemap: Vec::new(),
            _watcher: None,
            events: None,
            pending: HashSet::new(),
            pending_since: None,
            scanned: Instant::now(),
        };
        use notify::Watcher;
        let (tx, rx) = mpsc::channel();
        if let Ok(mut w) = notify::recommended_watcher(tx) {
            if w.watch(&m.dir, notify::RecursiveMode::Recursive).is_ok() {
                m._watcher = Some(w);
                m.events = Some(rx);
            }
        }
        Ok(m)
    }

    /// Brings the folder in line with the place: new, moved and removed
    /// scripts, and Source changed in rustudio.
    fn scan(&mut self, dom: &WeakDom, skip: &dyn Fn(Ref) -> bool) -> Vec<String> {
        self.scanned = Instant::now();
        let mut errors = Vec::new();
        let (want, map) = layout(dom, &self.place, skip);
        let want: HashMap<Ref, PathBuf> = want.into_iter().map(|(r, p)| (r, self.dir.join(p))).collect();
        // Gone or moved: remove the old file (and folders left empty).
        let stale: Vec<PathBuf> =
            self.files.iter().filter(|(r, p)| want.get(r) != Some(p)).map(|(_, p)| p.clone()).collect();
        for p in stale {
            let _ = std::fs::remove_file(&p);
            self.disk.remove(&p);
            let mut d = p.parent();
            while let Some(dir) = d.filter(|d| *d != self.dir && d.starts_with(&self.dir)) {
                if std::fs::remove_dir(dir).is_err() {
                    break;
                }
                d = dir.parent();
            }
        }
        for (&r, p) in &want {
            let src = source_of(dom, r).unwrap_or("");
            if self.disk.get(p).map(String::as_str) == Some(src) {
                continue;
            }
            let res = p.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(p, src));
            match res {
                Ok(()) => {
                    self.disk.insert(p.clone(), src.to_string());
                }
                Err(e) => errors.push(format!("{}: {e}", p.display())),
            }
        }
        self.by_path = want.iter().map(|(r, p)| (p.clone(), *r)).collect();
        self.files = want;
        let sm = serde_json::to_vec_pretty(&map).unwrap_or_default();
        if sm != self.sourcemap {
            match std::fs::write(self.dir.join("sourcemap.json"), &sm) {
                Ok(()) => self.sourcemap = sm,
                Err(e) => errors.push(format!("sourcemap.json: {e}")),
            }
        }
        errors
    }

    /// Mirror files saved by the editor since the last call, with their
    /// text (once the watcher has been quiet for the debounce).
    fn poll_disk(&mut self) -> Vec<(Ref, String)> {
        if let Some(rx) = &self.events {
            while let Ok(ev) = rx.try_recv() {
                let Ok(ev) = ev else { continue };
                if matches!(ev.kind, notify::EventKind::Access(_)) {
                    continue;
                }
                self.pending.extend(ev.paths);
                self.pending_since = Some(Instant::now());
            }
        }
        let Some(since) = self.pending_since else { return Vec::new() };
        if since.elapsed() < Duration::from_millis(crate::mech::PROJECT_DEBOUNCE_MS as u64) {
            return Vec::new();
        }
        self.pending_since = None;
        let mut out = Vec::new();
        for p in std::mem::take(&mut self.pending) {
            let Some(&r) = self.by_path.get(&p) else { continue };
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            if self.disk.get(&p) != Some(&text) {
                self.disk.insert(p, text.clone());
                out.push((r, text));
            }
        }
        out
    }
}

impl Editor {
    /// The place's name for its mirror folder.
    fn mirror_name(&self) -> String {
        self.doc
            .cloud
            .as_ref()
            .map(|c| if c.experience.is_empty() { c.name.clone() } else { c.experience.clone() })
            .filter(|n| !n.is_empty())
            .or_else(|| self.doc.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "Untitled".into())
    }

    /// The file a project folder keeps this script in, if any.
    fn project_file(&self, r: Ref) -> Option<(PathBuf, PathBuf)> {
        let st = self.project.as_ref().filter(|st| st.doc_id == self.doc.id)?;
        let file = match st.map.get(&r)? {
            crate::project::Origin::File { file, .. } => file.clone(),
            crate::project::Origin::Dir { init: Some((f, _)), .. } => f.clone(),
            _ => return None,
        };
        Some((file, st.root.clone()))
    }

    /// Opens a script with the external editor. False (with the reason in
    /// Output) when it couldn't; the caller then opens a script tab.
    pub fn open_script_external(&mut self, r: Ref) -> bool {
        if self.play.is_some() || !self.doc.class_of(r).is_some_and(|c| is_script(&c)) {
            return false;
        }
        let (file, folder) = match self.project_file(r) {
            Some(f) => f,
            None => {
                if let Err(e) = self.ensure_mirror() {
                    self.error(format!("External editor: can't write the scripts' folder: {e}"));
                    return false;
                }
                let m = self.external.as_ref().expect("mirror just made");
                match m.files.get(&r) {
                    Some(p) => (p.clone(), m.dir.clone()),
                    None => return false,
                }
            }
        };
        let command = self.settings.external_editor.command.clone();
        let args = fill(&command, &file, 1, &folder);
        match launch(&args, &folder) {
            Ok(()) => {
                let name = self.doc.dom.get_by_ref(r).map(|i| i.name.clone()).unwrap_or_default();
                self.info(format!("Opened {name} in {} ({})", args[0], file.display()));
                true
            }
            Err(e) => {
                self.error(format!(
                    "External editor didn't start ({e}); opening the script here. \
                     Change the command in File > Studio Settings."
                ));
                false
            }
        }
    }

    fn ensure_mirror(&mut self) -> Result<(), String> {
        if self.external.as_ref().is_some_and(|m| m.doc_id == self.doc.id) {
            return Ok(());
        }
        let place = self.mirror_name();
        let dir = crate::paths::cache().join("rustudio").join("external").join(file_name(&place));
        self.external = Some(Mirror::new(dir, &place, self.doc.id)?);
        self.external_scan();
        Ok(())
    }

    fn external_scan(&mut self) {
        let Some(mut m) = self.external.take() else { return };
        let synced: HashSet<Ref> = self
            .project
            .as_ref()
            .filter(|st| st.doc_id == self.doc.id)
            .map(|st| st.map.keys().copied().collect())
            .unwrap_or_default();
        for e in m.scan(&self.doc.dom, &|r| synced.contains(&r)) {
            self.warn(format!("External editor: {e}"));
        }
        self.external = Some(m);
    }

    /// Once a frame: edits saved in the external editor come into the
    /// place, and rustudio's changes go out to the folder (not during Play).
    pub fn external_tick(&mut self) {
        let Some(m) = self.external.as_mut() else { return };
        if self.play.is_some() || m.doc_id != self.doc.id {
            return;
        }
        let saved = m.poll_disk();
        if !saved.is_empty() {
            let edits: Vec<_> = saved
                .into_iter()
                .filter(|(r, text)| source_of(&self.doc.dom, *r).is_some_and(|s| s != text))
                .map(|(r, text)| (r, rbx_dom_weak::ustr("Source"), Variant::String(text)))
                .collect();
            if !edits.is_empty() {
                self.doc.set_props("Edit script in external editor", edits, None);
            }
        }
        if self.external.as_ref().is_some_and(|m| m.scanned.elapsed() >= SCAN_EVERY) {
            self.external_scan();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::InstanceBuilder;

    #[test]
    fn splits_and_fills_commands() {
        assert_eq!(split_command(r#"code  "a b" 'c d' e\ f"#), ["code", "a b", "c d", "e f"]);
        assert_eq!(split_command(r#"x "" y"#), ["x", "", "y"]);
        let args = fill("code {folder} --goto {file}:{line}", Path::new("/p q/a.luau"), 7, Path::new("/p q"));
        assert_eq!(args, ["code", "/p q", "--goto", "/p q/a.luau:7"]);
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(file_name("a/b:c"), "a_b_c");
        assert_eq!(file_name(".."), "_");
        assert_eq!(file_name("x. "), "x");
    }

    fn place() -> (WeakDom, Ref, Ref, Ref) {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let sss = dom.insert(root, InstanceBuilder::new("ServerScriptService").with_name("ServerScriptService"));
        let a = dom.insert(
            sss,
            InstanceBuilder::new("Script").with_name("Main").with_property("Source", Variant::String("print(1)".into())),
        );
        let b = dom.insert(sss, InstanceBuilder::new("ModuleScript").with_name("Main"));
        let _part = dom.insert(root, InstanceBuilder::new("Folder").with_name("Empty"));
        (dom, sss, a, b)
    }

    #[test]
    fn lays_out_scripts_with_a_sourcemap() {
        let (dom, _, a, b) = place();
        let (files, map) = layout(&dom, "Game", &|_| false);
        let files: HashMap<Ref, PathBuf> = files.into_iter().collect();
        assert_eq!(files[&a], Path::new("ServerScriptService/Main.server.luau"));
        // A same-named sibling gets its own file.
        assert_eq!(files[&b], Path::new("ServerScriptService/Main~2.luau"));
        // Only scripts and their ancestors are listed.
        let kids = map["children"].as_array().unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0]["name"], "ServerScriptService");
        assert_eq!(kids[0]["children"][0]["filePaths"][0], "ServerScriptService/Main.server.luau");
        let (none, _) = layout(&dom, "Game", &|r| r == a || r == b);
        assert!(none.is_empty());
    }

    #[test]
    fn mirror_syncs_both_ways() {
        let tmp = std::env::temp_dir().join(format!("rustudio-mirror-test-{}", std::process::id()));
        let (mut dom, sss, a, _) = place();
        let mut m = Mirror::new(tmp.clone(), "t", 1).unwrap();
        // Saves are fed in by hand below (the watcher's own events would
        // restart the debounce).
        m.events = None;
        assert!(m.scan(&dom, &|_| false).is_empty());
        let file = tmp.join("ServerScriptService/Main.server.luau");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "print(1)");
        assert!(tmp.join("sourcemap.json").is_file());
        // Source changed in rustudio: written out.
        dom.get_by_ref_mut(a).unwrap().properties.insert("Source".into(), Variant::String("print(2)".into()));
        m.scan(&dom, &|_| false);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "print(2)");
        // Saved in the editor: read back once.
        std::fs::write(&file, "print(3)").unwrap();
        m.pending.insert(file.clone());
        m.pending_since = Some(Instant::now() - Duration::from_secs(5));
        let got = m.poll_disk();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], (a, "print(3)".to_string()));
        // Renamed: the old file goes, the new one comes.
        dom.get_by_ref_mut(a).unwrap().name = "Boot".into();
        m.scan(&dom, &|_| false);
        assert!(!file.exists());
        assert!(tmp.join("ServerScriptService/Boot.server.luau").is_file());
        // Deleted: so is its file (and the folder once empty).
        dom.destroy(sss);
        m.scan(&dom, &|_| false);
        assert!(!tmp.join("ServerScriptService").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
