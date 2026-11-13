//! Script editor autocomplete: what can go at the caret, the way Studio's
//! script editor offers it (docs/UI-PARITY.md "Script editor autocomplete").
//!
//! `complete` is pure: the text, the caret and the open place in; the byte
//! range a choice replaces and the suggestions out. It offers
//! - keywords, Roblox's globals and libraries, and the names the script
//!   declares above the caret (locals, functions, parameters, loop variables);
//! - after `.` / `:`, the members of what the expression before it is: the
//!   place's own instances (`workspace.Baseplate.`, `script.Parent.`),
//!   classes from the API dump (properties, methods, events, following
//!   property and return types: `game:GetService("Players").LocalPlayer.`),
//!   datatypes (`part.Position.`), libraries (`math.`), enums
//!   (`Enum.Material.`) and signals (`part.Touched:`);
//! - locals' types from their declarations (`local p = workspace.Part`,
//!   `local h: Humanoid`, `for _, c in x:GetChildren()`, and a
//!   `:Connect(function(hit)` callback's parameters from the event's);
//! - inside the first string argument of GetService, Instance.new,
//!   FindFirstChild / WaitForChild, IsA / …OfClass / …WhichIsA and
//!   GetPropertyChangedSignal: services, classes, children and properties.
//!
//! Scope isn't followed: a local declared above the caret is offered even
//! when its block has ended.

use std::collections::HashMap;
use std::ops::Range;

use rbx_dom_weak::types::Ref;
use rbx_dom_weak::WeakDom;

use crate::api::{Api, Kind, Member};
use crate::script_editor::{tokenize, Tok, GLOBALS, KEYWORDS};

/// The most suggestions one list holds.
const MAX_ITEMS: usize = 300;

pub struct CompletionCtx<'a> {
    /// The place, for children (`workspace.Baseplate`) and services.
    pub dom: &'a WeakDom,
    /// The script being edited (`script`).
    pub script: Ref,
    /// Roblox's API dump: classes, members, enums.
    pub api: Option<&'static Api>,
    /// Ctrl+Space: list even before anything is typed.
    pub force: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Keyword,
    Global,
    Library,
    Local,
    Function,
    Property,
    Method,
    Event,
    Child,
    Class,
    Enum,
    EnumItem,
}

impl ItemKind {
    /// Which of two suggestions with the same name wins (lower first).
    fn rank(self) -> u8 {
        match self {
            ItemKind::Local | ItemKind::Function => 0,
            ItemKind::Child => 1,
            ItemKind::Property | ItemKind::Method | ItemKind::Event => 2,
            _ => 3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub label: String,
    pub kind: ItemKind,
    /// A one-line signature shown beside the list.
    pub detail: String,
    /// The class whose icon the row shows (children, classes, services).
    pub class: Option<String>,
    /// Listed before the other matches (services the place has).
    pub preferred: bool,
}

impl Item {
    fn new(label: impl Into<String>, kind: ItemKind, detail: impl Into<String>) -> Self {
        Item { label: label.into(), kind, detail: detail.into(), class: None, preferred: false }
    }
    fn with_class(mut self, class: &str) -> Self {
        self.class = Some(class.to_string());
        self
    }
}

/// What an expression is, as far as completion follows it.
#[derive(Clone, Debug, PartialEq)]
enum Ty {
    /// That instance of the open place.
    Inst(Ref),
    /// Some instance of the class.
    Class(String),
    /// A datatype value (`Vector3`, `RBXScriptConnection`, `string`, `number`…).
    Data(String),
    /// An event (class, name): its `:Connect` callback gets the event's parameters.
    Signal(String, String),
    /// A global table: a library (`math`) or a datatype's constructors (`Vector3`).
    Lib(String),
    /// `Enum`.
    Enums,
    /// `Enum.Material`.
    EnumType(String),
    /// `Enum.Material.Plastic`.
    EnumItem(String),
}

/// Suggestions for the caret at byte `cursor` of `src`: the byte range the
/// chosen one replaces (the word being typed) and the list, best first.
pub fn complete(src: &str, cursor: usize, ctx: &CompletionCtx) -> (Range<usize>, Vec<Item>) {
    let mut cursor = cursor.min(src.len());
    while !src.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let before = &src[..cursor];
    let start = word_start(before);
    let prefix = &before[start..];
    let nothing = (start..cursor, Vec::new());
    let mut items = Vec::new();

    // Inside a comment or number: nothing. Inside a string: the names a
    // call takes there (`GetService("Pl|`).
    let toks = tokenize(src);
    let at = toks.iter().find(|(r, _)| r.start < cursor && cursor <= r.end);
    match at.map(|(r, t)| (r.clone(), *t)) {
        Some((_, Tok::Comment | Tok::Num)) => return nothing,
        Some((r, Tok::Str)) => {
            let b = src.as_bytes();
            let quote = b[r.start];
            let closed = r.end - r.start >= 2 && b[r.end - 1] == quote;
            if !matches!(quote, b'"' | b'\'') || (closed && cursor == r.end) {
                return nothing;
            }
            let typed = &src[r.start + 1..cursor];
            if typed.contains(['\\', '\n']) {
                return nothing;
            }
            let scan = scan(&src[..r.start], ctx);
            string_arg_items(&src[..r.start], ctx, &scan.types, &mut items);
            return finish(r.start + 1..cursor, typed, items);
        }
        _ => {}
    }

    let lead = &before[..start];
    let scan = scan(lead, ctx);
    match lead.as_bytes().last() {
        // `something.` / `something:` (not `..`): its members.
        Some(&sep) if (sep == b'.' && !lead.ends_with("..")) || sep == b':' => {
            let expr = &lead[..lead.len() - 1];
            if let Some(ty) = chain(expr).and_then(|(_, segs)| eval(&segs, ctx, &scan.types)) {
                members(&ty, sep == b':', ctx, &mut items);
            }
        }
        _ => {
            if prefix.is_empty() && !ctx.force {
                return nothing;
            }
            match last_word(lead) {
                // Naming a new local: only `local function`.
                Some("local" | "const") => items.push(Item::new("function", ItemKind::Keyword, "")),
                // Naming a function or loop variable: nothing to offer.
                Some("function" | "for") => return nothing,
                _ => bare_words(&scan, ctx, &mut items),
            }
        }
    }
    finish(start..cursor, prefix, items)
}

/// Keeps what matches the typed word: names starting with it first, then
/// (from two letters on) names with a later word starting with it
/// (`child` → FindFirstChild); A–Z, no repeats.
fn finish(range: Range<usize>, prefix: &str, mut items: Vec<Item>) -> (Range<usize>, Vec<Item>) {
    let lower = prefix.to_lowercase();
    let rank = |label: &str| {
        if label.to_lowercase().starts_with(&lower) {
            0
        } else if lower.len() >= 2 && word_starts(label).any(|i| label[i..].to_lowercase().starts_with(&lower)) {
            1
        } else {
            2
        }
    };
    items.retain(|i| rank(&i.label) < 2 && i.label != prefix);
    // Among equals, the one written the way it was typed first (`wo` → workspace, Workspace).
    items.sort_by_cached_key(|i| {
        let case = !i.label.starts_with(prefix);
        (rank(&i.label), !i.preferred, case, i.label.to_lowercase(), i.label.clone(), i.kind.rank())
    });
    items.dedup_by(|a, b| a.label == b.label);
    items.truncate(MAX_ITEMS);
    (range, items)
}

/// Where the words of a name start after the first: `FindFirstChild` →
/// First, Child; `get_name` → name.
fn word_starts(label: &str) -> impl Iterator<Item = usize> + '_ {
    let b = label.as_bytes();
    (1..b.len()).filter(move |&i| {
        (b[i].is_ascii_uppercase() && !b[i - 1].is_ascii_uppercase()) || (b[i - 1] == b'_' && b[i] != b'_')
    })
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Where the identifier ending at the end of `s` starts.
fn word_start(s: &str) -> usize {
    s.char_indices().rev().find(|&(_, c)| !is_ident(c)).map_or(0, |(i, c)| i + c.len_utf8())
}

/// The last word or symbol of `s`, spaces skipped.
fn last_word(s: &str) -> Option<&str> {
    let s = s.trim_end();
    let start = word_start(s);
    if start < s.len() {
        Some(&s[start..])
    } else {
        let c = s.chars().next_back()?;
        Some(&s[s.len() - c.len_utf8()..])
    }
}

// ------------------------------------------------------------ expressions

/// One step of a chain like `game:GetService("Players").LocalPlayer`.
#[derive(Debug)]
struct Seg<'a> {
    name: &'a str,
    /// Reached with `:` (a method call).
    colon: bool,
    /// The text between the call's parentheses, when called.
    call: Option<&'a str>,
}

/// The chain of names, calls and `.` / `:` that ends `s`, read backwards,
/// and where it starts. `local p = workspace.Baseplate` → (10, [workspace, Baseplate]).
fn chain(s: &str) -> Option<(usize, Vec<Seg<'_>>)> {
    let b = s.as_bytes();
    let mut end = s.len();
    let mut segs = Vec::new();
    loop {
        let mut call = None;
        if end > 0 && b[end - 1] == b')' {
            let open = matching_open(s, end - 1)?;
            call = Some(&s[open + 1..end - 1]);
            end = open;
        }
        let start = word_start(&s[..end]);
        let name = &s[start..end];
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        let sep = if start > 0 { b[start - 1] } else { 0 };
        let dot = sep == b'.' && !(start >= 2 && b[start - 2] == b'.');
        let colon = sep == b':';
        segs.push(Seg { name, colon, call });
        if dot || colon {
            end = start - 1;
        } else {
            segs.reverse();
            return Some((start, segs));
        }
    }
}

/// The `(` that the `)` at `close` closes, skipping quoted strings.
fn matching_open(s: &str, close: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0;
    let mut i = close + 1;
    while i > 0 {
        i -= 1;
        match b[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            q @ (b'"' | b'\'') => {
                i = s[..i].rfind(q as char)?;
            }
            _ => {}
        }
    }
    None
}

/// The text of a call's first argument when it is a plain string.
fn first_string_arg(args: &str) -> Option<&str> {
    let a = args.trim_start();
    let q = a.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let rest = &a[1..];
    rest.find(q).map(|end| &rest[..end])
}

/// What a chain is, following the place, the API dump and the locals.
fn eval(segs: &[Seg], ctx: &CompletionCtx, locals: &HashMap<String, Ty>) -> Option<Ty> {
    let (first, rest) = segs.split_first()?;
    let mut ty = match first.call {
        Some(_) => match first.name {
            "UserSettings" => Ty::Class("UserSettings".into()),
            "settings" => Ty::Class("GlobalSettings".into()),
            _ => return None,
        },
        None => match locals.get(first.name) {
            Some(t) => t.clone(),
            None => global_ty(first.name, ctx)?,
        },
    };
    for seg in rest {
        ty = step(ty, seg, ctx)?;
    }
    Some(ty)
}

fn global_ty(name: &str, ctx: &CompletionCtx) -> Option<Ty> {
    let dom = ctx.dom;
    Some(match name {
        "game" | "Game" => Ty::Inst(dom.root_ref()),
        "workspace" | "Workspace" => service(ctx, "Workspace"),
        "script" => match dom.get_by_ref(ctx.script) {
            Some(_) => Ty::Inst(ctx.script),
            None => Ty::Class("Script".into()),
        },
        "plugin" => Ty::Class("Plugin".into()),
        "Enum" => Ty::Enums,
        _ if lib_members(name).is_some() => Ty::Lib(name.to_string()),
        _ => return None,
    })
}

/// The service of that class in the place, else just its class.
fn service(ctx: &CompletionCtx, class: &str) -> Ty {
    let dom = ctx.dom;
    dom.root()
        .children()
        .iter()
        .find(|c| dom.get_by_ref(**c).is_some_and(|i| i.class == class))
        .map_or_else(|| Ty::Class(class.to_string()), |c| Ty::Inst(*c))
}

fn child_where(dom: &WeakDom, parent: Ref, f: impl Fn(&rbx_dom_weak::Instance) -> bool) -> Option<Ref> {
    let inst = dom.get_by_ref(parent)?;
    inst.children().iter().copied().find(|c| dom.get_by_ref(*c).is_some_and(&f))
}

fn is_a(ctx: &CompletionCtx, class: &str, base: &str) -> bool {
    ctx.api.is_some_and(|a| a.is_a(class, base))
}

/// One step along a chain: `.Name`, `:Method(…)`, `.func(…)`.
fn step(ty: Ty, seg: &Seg, ctx: &CompletionCtx) -> Option<Ty> {
    let plain = !seg.colon && seg.call.is_none();
    let arg = seg.call.and_then(first_string_arg);
    match ty {
        Ty::Inst(r) => {
            let dom = ctx.dom;
            let inst = dom.get_by_ref(r)?;
            if plain {
                if seg.name == "Parent" {
                    let p = inst.parent();
                    return Some(match dom.get_by_ref(p) {
                        Some(_) => Ty::Inst(p),
                        None => Ty::Class("Instance".into()),
                    });
                }
                if let Some(c) = child_where(dom, r, |i| i.name == seg.name) {
                    return Some(Ty::Inst(c));
                }
            }
            if seg.colon {
                let found = match (seg.name, arg) {
                    ("FindFirstChild" | "WaitForChild", Some(a)) => child_where(dom, r, |i| i.name == a),
                    ("FindFirstChildOfClass", Some(a)) => child_where(dom, r, |i| i.class == a),
                    ("FindFirstChildWhichIsA", Some(a)) => child_where(dom, r, |i| is_a(ctx, &i.class, a)),
                    _ => None,
                };
                if let Some(c) = found {
                    return Some(Ty::Inst(c));
                }
            }
            class_step(inst.class.as_str(), seg, arg, ctx)
        }
        Ty::Class(c) => class_step(&c, seg, arg, ctx),
        Ty::Data(d) => data_step(&d, seg),
        Ty::Signal(..) => data_step("RBXScriptSignal", seg),
        Ty::Lib(l) => {
            if l == "Instance" && matches!(seg.name, "new" | "fromExisting") && seg.call.is_some() && !seg.colon {
                let known = arg.filter(|a| ctx.api.is_some_and(|api| api.classes.contains_key(*a)));
                return Some(Ty::Class(known.unwrap_or("Instance").to_string()));
            }
            let (_, kind, ty, _) = lib_members(&l)?.iter().find(|m| m.0 == seg.name)?;
            let ok = match kind {
                ItemKind::Property => plain,
                _ => !seg.colon && seg.call.is_some(),
            };
            ok.then(|| ty_from_str(ty, &l)).flatten()
        }
        Ty::Enums => {
            (plain && ctx.api?.enums.contains_key(seg.name)).then(|| Ty::EnumType(seg.name.to_string()))
        }
        Ty::EnumType(e) => {
            let items = ctx.api?.enums.get(&e)?;
            (plain && items.iter().any(|(n, _)| n == seg.name)).then_some(Ty::EnumItem(e))
        }
        Ty::EnumItem(e) => match (seg.name, plain) {
            ("Name", true) => Some(Ty::Data("string".into())),
            ("Value", true) => Some(Ty::Data("number".into())),
            ("EnumType", true) => Some(Ty::EnumType(e)),
            _ => None,
        },
    }
}

/// A step on some instance of `class`, from the API dump.
fn class_step(class: &str, seg: &Seg, arg: Option<&str>, ctx: &CompletionCtx) -> Option<Ty> {
    let api = ctx.api?;
    if seg.colon {
        match (seg.name, arg) {
            ("GetService" | "FindService", Some(a)) => return Some(service(ctx, a)),
            (
                "FindFirstChildOfClass" | "FindFirstChildWhichIsA" | "FindFirstAncestorOfClass"
                | "FindFirstAncestorWhichIsA",
                Some(a),
            ) if api.classes.contains_key(a) => return Some(Ty::Class(a.to_string())),
            ("Clone", _) => return Some(Ty::Class(class.to_string())),
            _ => {}
        }
    }
    let (_, m) = api.lookup(class, seg.name)?;
    let plain = !seg.colon && seg.call.is_none();
    match m.kind {
        Kind::Property if plain => value_ty(m.value_type.as_ref()?),
        Kind::Function if seg.call.is_some() => value_ty(m.value_type.as_ref()?),
        Kind::Event if plain => Some(Ty::Signal(class.to_string(), seg.name.to_string())),
        _ => None,
    }
}

/// A dump type as completion follows it.
fn value_ty((cat, name): &(String, String)) -> Option<Ty> {
    Some(match cat.as_str() {
        "Class" => Ty::Class(name.clone()),
        "Enum" => Ty::EnumItem(name.clone()),
        "DataType" => match name.as_str() {
            "Objects" | "Instances" | "Array" | "Dictionary" | "Map" | "Tuple" | "Variant" | "Function" => {
                return None
            }
            _ => Ty::Data(name.clone()),
        },
        "Primitive" => Ty::Data(
            match name.as_str() {
                "string" => "string",
                "bool" => "boolean",
                "null" => return None,
                _ => "number",
            }
            .into(),
        ),
        _ => return None,
    })
}

fn data_step(d: &str, seg: &Seg) -> Option<Ty> {
    let (_, kind, ty, _) = data_members(d)?.iter().find(|m| m.0 == seg.name)?;
    let ok = match kind {
        ItemKind::Property => !seg.colon && seg.call.is_none(),
        _ => seg.colon && seg.call.is_some(),
    };
    ok.then(|| ty_from_str(ty, d)).flatten()
}

/// The type a table entry gives: "" none, "self" the type itself, `#Class`
/// an instance, `Enum.X` an enum item, else a datatype.
fn ty_from_str(t: &str, this: &str) -> Option<Ty> {
    match t {
        "" => None,
        "self" => Some(Ty::Data(this.to_string())),
        s if s.starts_with('#') => Some(Ty::Class(s[1..].to_string())),
        s if s.starts_with("Enum.") => Some(Ty::EnumItem(s[5..].to_string())),
        s => Some(Ty::Data(s.to_string())),
    }
}

/// How a type reads in a signature.
fn ty_name(ty: &Ty, ctx: &CompletionCtx) -> String {
    match ty {
        Ty::Inst(r) => ctx.dom.get_by_ref(*r).map_or("Instance".into(), |i| i.class.to_string()),
        Ty::Class(c) | Ty::Data(c) => c.clone(),
        Ty::Signal(..) => "RBXScriptSignal".into(),
        Ty::Lib(l) => l.clone(),
        Ty::Enums => "Enums".into(),
        Ty::EnumType(e) => format!("Enum.{e}"),
        Ty::EnumItem(e) => format!("Enum.{e}"),
    }
}

/// A dump type as Studio writes it in signatures.
fn dump_type_name((cat, name): &(String, String)) -> String {
    match (cat.as_str(), name.as_str()) {
        ("Primitive", "bool") => "boolean".into(),
        ("Primitive", "null") => "()".into(),
        ("Primitive", "float" | "double" | "int" | "int64") => "number".into(),
        ("Enum", n) => format!("Enum.{n}"),
        (_, n) => n.to_string(),
    }
}

/// `FindFirstChild(name: string, recursive: boolean = false): Instance`.
fn signature(name: &str, m: &Member) -> String {
    let params = || {
        m.params
            .iter()
            .map(|p| match &p.default {
                Some(d) => format!("{}: {} = {d}", p.name, dump_type_name(&p.ty)),
                None => format!("{}: {}", p.name, dump_type_name(&p.ty)),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let ty = m.value_type.as_ref().map(dump_type_name);
    match m.kind {
        Kind::Property => {
            let ro = if m.read_only { " (read only)" } else { "" };
            format!("{name}: {}{ro}", ty.unwrap_or_default())
        }
        Kind::Function => match ty.as_deref() {
            Some("()") | None => format!("{name}({})", params()),
            Some(t) => format!("{name}({}): {t}", params()),
        },
        Kind::Event => format!("{name}({}): RBXScriptSignal", params()),
        Kind::Callback => format!("{name}: ({}) -> ()", params()),
    }
}

// ------------------------------------------------------------ suggestions

/// After `.` (or `:` when `colon`): what can follow what `ty` is.
fn members(ty: &Ty, colon: bool, ctx: &CompletionCtx, items: &mut Vec<Item>) {
    match ty {
        Ty::Inst(r) => {
            let Some(inst) = ctx.dom.get_by_ref(*r) else { return };
            if !colon {
                for c in inst.children() {
                    if let Some(child) = ctx.dom.get_by_ref(*c) {
                        // `x.My Part` isn't a valid index; Studio offers names that are.
                        if child.name.chars().all(is_ident) && !child.name.starts_with(|c: char| c.is_ascii_digit()) {
                            let class = child.class.as_str();
                            items.push(
                                Item::new(&child.name, ItemKind::Child, format!("{}: {class}", child.name))
                                    .with_class(class),
                            );
                        }
                    }
                }
            }
            class_members(inst.class.as_str(), colon, ctx, items);
        }
        Ty::Class(c) => class_members(c, colon, ctx, items),
        Ty::Data(d) => table_items(data_members(d), colon, items),
        Ty::Signal(..) => table_items(data_members("RBXScriptSignal"), colon, items),
        Ty::Lib(l) if !colon => table_items(lib_members(l), false, items),
        Ty::Enums if !colon => {
            for e in ctx.api.map(|a| a.enums.keys()).into_iter().flatten() {
                items.push(Item::new(e, ItemKind::Enum, format!("Enum.{e}")));
            }
        }
        Ty::EnumType(e) => {
            if colon {
                items.push(Item::new("GetEnumItems", ItemKind::Method, format!("GetEnumItems(): {{Enum.{e}}}")));
            } else if let Some(list) = ctx.api.and_then(|a| a.enums.get(e)) {
                for (n, v) in list {
                    items.push(Item::new(n, ItemKind::EnumItem, format!("Enum.{e}.{n} = {v}")));
                }
            }
        }
        Ty::EnumItem(e) => {
            if colon {
                items.push(Item::new("IsA", ItemKind::Method, "IsA(enumName: string): boolean"));
            } else {
                items.push(Item::new("Name", ItemKind::Property, "Name: string (read only)"));
                items.push(Item::new("Value", ItemKind::Property, "Value: number (read only)"));
                items.push(Item::new("EnumType", ItemKind::Property, format!("EnumType: Enum.{e} (read only)")));
            }
        }
        _ => {}
    }
}

/// The members of `class` and its superclasses that a game script may use:
/// properties, events and callbacks after `.`, methods after `:`.
fn class_members(class: &str, colon: bool, ctx: &CompletionCtx, items: &mut Vec<Item>) {
    let Some(api) = ctx.api else { return };
    let mut cur = Some(class);
    let mut guard = 0;
    while let Some(c) = cur {
        let Some(info) = api.classes.get(c) else { break };
        for name in &info.order {
            let Some(m) = info.members.get(name) else { continue };
            if m.security != "None" || m.deprecated || m.script_hidden {
                continue;
            }
            let kind = match m.kind {
                Kind::Function => ItemKind::Method,
                Kind::Property | Kind::Callback => ItemKind::Property,
                Kind::Event => ItemKind::Event,
            };
            if (kind == ItemKind::Method) == colon {
                items.push(Item::new(name, kind, signature(name, m)));
            }
        }
        cur = info.superclass.as_deref();
        guard += 1;
        if guard > 64 {
            break;
        }
    }
}

/// Entries of a datatype or library table: methods after `:`, the rest after `.`.
fn table_items(list: Option<&'static [M]>, colon: bool, items: &mut Vec<Item>) {
    for (name, kind, _, detail) in list.into_iter().flatten() {
        if (*kind == ItemKind::Method) == colon {
            items.push(Item::new(*name, *kind, *detail));
        }
    }
}

/// A bare word: keywords, globals, libraries and the script's own names.
fn bare_words(scan: &Scan, ctx: &CompletionCtx, items: &mut Vec<Item>) {
    for k in KEYWORDS {
        items.push(Item::new(*k, ItemKind::Keyword, ""));
    }
    for (name, detail) in GLOBAL_FUNCS {
        items.push(Item::new(*name, ItemKind::Global, *detail));
    }
    for g in GLOBALS {
        let (kind, detail) = if lib_members(g).is_some() {
            (ItemKind::Library, (*g).to_string())
        } else {
            let detail = match *g {
                "game" | "Game" => "game: DataModel".to_string(),
                "workspace" | "Workspace" => format!("{g}: Workspace"),
                "script" => format!("script: {}", ty_name(&global_ty("script", ctx).unwrap_or(Ty::Enums), ctx)),
                "shared" | "_G" => format!("{g}: table"),
                _ => GLOBAL_FUNCS.iter().find(|(n, _)| n == g).map_or_else(String::new, |(_, d)| d.to_string()),
            };
            (ItemKind::Global, detail)
        };
        items.push(Item::new(*g, kind, detail));
    }
    for name in LIBS.iter().map(|(n, _)| n) {
        items.push(Item::new(*name, ItemKind::Library, *name));
    }
    for (name, kind, detail) in &scan.names {
        items.push(Item::new(name, *kind, detail));
    }
}

/// Inside the first string argument of a call: the names it takes.
fn string_arg_items(head: &str, ctx: &CompletionCtx, locals: &HashMap<String, Ty>, items: &mut Vec<Item>) {
    let Some(head) = head.trim_end().strip_suffix('(') else { return };
    let Some((_, segs)) = chain(head.trim_end()) else { return };
    let Some((last, base)) = segs.split_last() else { return };
    if base.is_empty() {
        return;
    }
    let Some(ty) = eval(base, ctx, locals) else { return };
    let Some(api) = ctx.api else { return };
    let classes = |items: &mut Vec<Item>, keep: &dyn Fn(&str, &crate::api::Class) -> bool| {
        for name in &api.class_order {
            if api.classes.get(name).is_some_and(|c| keep(name, c)) {
                items.push(Item::new(name, ItemKind::Class, "").with_class(name));
            }
        }
    };
    let class_of = |ty: &Ty| match ty {
        Ty::Inst(r) => ctx.dom.get_by_ref(*r).map(|i| i.class.to_string()),
        Ty::Class(c) => Some(c.clone()),
        _ => None,
    };
    // A service a game script uses: one with script-usable members of its
    // own, or one the place has (ReplicatedStorage has no members).
    let in_place: Vec<&str> = ctx
        .dom
        .root()
        .children()
        .iter()
        .filter_map(|c| ctx.dom.get_by_ref(*c).map(|i| i.class.as_str()))
        .collect();
    let usable_service = |name: &str, c: &crate::api::Class| {
        c.service
            && !c.deprecated
            && (in_place.contains(&name)
                || c.members.values().any(|m| m.security == "None" && !m.deprecated && !m.script_hidden))
    };
    match (last.name, last.colon) {
        ("new", false) if ty == Ty::Lib("Instance".into()) => classes(items, &|_, c| c.creatable && !c.deprecated),
        _ if !last.colon || class_of(&ty).is_none() => {}
        ("GetService" | "FindService", _) => {
            classes(items, &usable_service);
            for i in items.iter_mut() {
                i.preferred = in_place.contains(&i.label.as_str());
            }
        }
        ("FindFirstChild" | "WaitForChild", _) => {
            if let Ty::Inst(r) = ty {
                for c in ctx.dom.get_by_ref(r).map(|i| i.children()).unwrap_or_default() {
                    if let Some(i) = ctx.dom.get_by_ref(*c) {
                        items.push(Item::new(&i.name, ItemKind::Child, format!("{}: {}", i.name, i.class)).with_class(&i.class));
                    }
                }
            }
        }
        (
            "IsA" | "FindFirstChildOfClass" | "FindFirstChildWhichIsA" | "FindFirstAncestorOfClass"
            | "FindFirstAncestorWhichIsA" | "FindFirstDescendantOfClass",
            _,
        ) => classes(items, &|_, c| !c.deprecated),
        ("GetPropertyChangedSignal", _) => {
            let mut all = Vec::new();
            class_members(&class_of(&ty).unwrap_or_default(), false, ctx, &mut all);
            items.extend(all.into_iter().filter(|i| i.kind == ItemKind::Property));
        }
        _ => {}
    }
}

// ------------------------------------------------------------ the script's names

/// What the script declares above the caret.
#[derive(Default)]
struct Scan {
    /// Names in order, with their kind and what to show beside them.
    names: Vec<(String, ItemKind, String)>,
    /// What each name is, when its declaration says.
    types: HashMap<String, Ty>,
}

impl Scan {
    fn add(&mut self, name: &str, kind: ItemKind, ty: Option<Ty>, ctx: &CompletionCtx) {
        let detail = match (&ty, kind) {
            (_, ItemKind::Function) => format!("function {name}"),
            (Some(t), _) => format!("local {name}: {}", ty_name(t, ctx)),
            (None, _) => format!("local {name}"),
        };
        match ty {
            Some(t) => {
                self.types.insert(name.to_string(), t);
            }
            None => {
                self.types.remove(name);
            }
        }
        self.names.push((name.to_string(), kind, detail));
    }
}

/// A name a script can declare: a word that isn't a keyword (nor `_`, "unused").
fn is_name(t: &str) -> bool {
    t.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && t != "_" && !KEYWORDS.contains(&t)
}

/// Names declared in `before`: `local a, b`, `const c`, `local function f`,
/// `function f`, parameters and `for` variables, with their types when the
/// declaration says (`= expr`, `: Type`, a `:Connect(function(hit)` callback).
fn scan(before: &str, ctx: &CompletionCtx) -> Scan {
    // The words and symbols in order (byte ranges kept), without spaces,
    // strings, comments or numbers.
    let toks: Vec<(Range<usize>, &str)> = tokenize(before)
        .into_iter()
        .filter(|(r, t)| matches!(t, Tok::Plain | Tok::Keyword | Tok::Global | Tok::Op) && !before[r.clone()].trim().is_empty())
        .map(|(r, _)| {
            let s = &before[r.clone()];
            (r, s)
        })
        .collect();
    let word = |i: usize| toks.get(i).map(|t| t.1);
    let mut out = Scan::default();
    for i in 0..toks.len() {
        match toks[i].1 {
            // `local function f` is handled by the `function` that follows.
            "local" | "const" if word(i + 1) != Some("function") => {
                let (names, end) = name_list(&toks, i + 1, ctx);
                let rhs = (names.len() == 1 && word(end) == Some("="))
                    .then(|| rhs_ty(before, toks[end].0.end, ctx, &out.types))
                    .flatten();
                for (n, ann) in names {
                    let ty = ann.or_else(|| rhs.clone());
                    out.add(&n, ItemKind::Local, ty, ctx);
                }
            }
            "for" => {
                let (names, end) = name_list(&toks, i + 1, ctx);
                // `for _, c in x:GetChildren() do`: the value is an Instance.
                let item = (word(end) == Some("in"))
                    .then(|| {
                        let from = toks[end].0.end;
                        let to = before[from..].find(" do").map_or(before.len(), |p| from + p);
                        let iter = &before[from..to];
                        if iter.contains("GetChildren()") || iter.contains("GetDescendants()") {
                            Some(Ty::Class("Instance".into()))
                        } else if iter.contains("GetPlayers()") {
                            Some(Ty::Class("Player".into()))
                        } else {
                            None
                        }
                    })
                    .flatten();
                let n = names.len();
                for (k, (name, ann)) in names.into_iter().enumerate() {
                    let ty = ann.or_else(|| (k == 1 || n == 1).then(|| item.clone()).flatten());
                    out.add(&name, ItemKind::Local, ty, ctx);
                }
            }
            "function" => {
                // `function f(` declares f; `function M.f(` adds to M, already known.
                if let (Some(name), Some("(")) = (word(i + 1), word(i + 2)) {
                    if is_name(name) {
                        out.add(name, ItemKind::Function, None, ctx);
                    }
                }
                let Some(open) = toks[i..].iter().position(|t| t.1 == "(").map(|p| i + p) else { continue };
                let (params, _) = name_list(&toks, open + 1, ctx);
                // `x.Event:Connect(function(a, b)`: the event's parameters.
                let event = (i >= 3
                    && word(i - 1) == Some("(")
                    && matches!(word(i - 2), Some("Connect" | "Once" | "ConnectParallel"))
                    && word(i - 3) == Some(":"))
                .then(|| chain(&before[..toks[i - 3].0.start]))
                .flatten()
                .and_then(|(_, segs)| eval(&segs, ctx, &out.types));
                let event_params = match (&event, ctx.api) {
                    (Some(Ty::Signal(class, name)), Some(api)) => api.lookup(class, name).map(|(_, m)| &m.params),
                    _ => None,
                };
                for (k, (name, ann)) in params.into_iter().enumerate() {
                    let from_event = event_params.and_then(|p| p.get(k)).and_then(|p| value_ty(&p.ty));
                    out.add(&name, ItemKind::Local, ann.or(from_event), ctx);
                }
            }
            _ => {}
        }
    }
    out
}

/// The names of a list like `a, b: Part, c` starting at `toks[i]`, each
/// with the type its annotation names; and where the list ends (`=`, `in`, `)`).
fn name_list(toks: &[(Range<usize>, &str)], mut i: usize, ctx: &CompletionCtx) -> (Vec<(String, Option<Ty>)>, usize) {
    let mut out = Vec::new();
    while let Some(&(_, t)) = toks.get(i) {
        if is_name(t) {
            out.push((t.to_string(), None));
        } else if t != "_" && t != "." {
            break; // not a name (`...` is three "." tokens)
        }
        i += 1;
        // `: Type`: the type, then on to the next `,` or the end of the list.
        if toks.get(i).map(|t| t.1) == Some(":") {
            if let (Some(last), Some(&(_, ty))) = (out.last_mut(), toks.get(i + 1)) {
                last.1 = annotation_ty(ty, ctx);
            }
            while let Some(&(_, t)) = toks.get(i) {
                if matches!(t, "," | "=" | ")" | "in") {
                    break;
                }
                i += 1;
            }
        }
        match toks.get(i).map(|t| t.1) {
            Some(",") => i += 1,
            Some(".") => {}
            _ => break,
        }
    }
    (out, i)
}

/// The type a `: Name` annotation names, if completion knows it.
fn annotation_ty(name: &str, ctx: &CompletionCtx) -> Option<Ty> {
    if ctx.api.is_some_and(|a| a.classes.contains_key(name)) {
        Some(Ty::Class(name.to_string()))
    } else if data_members(name).is_some() {
        Some(Ty::Data(name.to_string()))
    } else {
        None
    }
}

/// What the right side of `local x = …` (from byte `from` to the line's
/// end) is, when it is one literal or one chain.
fn rhs_ty(before: &str, from: usize, ctx: &CompletionCtx, locals: &HashMap<String, Ty>) -> Option<Ty> {
    let line = &before[from..];
    let line = &line[..line.find('\n').unwrap_or(line.len())];
    // Up to a comment.
    let code_end = tokenize(line).iter().find(|(_, t)| *t == Tok::Comment).map_or(line.len(), |(r, _)| r.start);
    let rhs = line[..code_end].trim().trim_end_matches(';').trim_end();
    if rhs.is_empty() {
        return None;
    }
    if rhs.starts_with(['"', '\'', '`']) || rhs.starts_with("[[") {
        return Some(Ty::Data("string".into()));
    }
    if rhs.parse::<f64>().is_ok() || rhs.starts_with("0x") {
        return Some(Ty::Data("number".into()));
    }
    if rhs == "true" || rhs == "false" {
        return Some(Ty::Data("boolean".into()));
    }
    let (start, segs) = chain(rhs)?;
    (start == 0).then(|| eval(&segs, ctx, locals)).flatten()
}

// ------------------------------------------------------------ tables

/// A member of a datatype or global table: name, kind, the type it gives
/// (see `ty_from_str`) and its signature. Members are Roblox's (the
/// creator docs' datatype and library pages); the Luau libraries are
/// checked against the vendored Luau in the tests.
type M = (&'static str, ItemKind, &'static str, &'static str);

use ItemKind::{Function as F, Method as Me, Property as P};

/// Global functions, with their signatures.
const GLOBAL_FUNCS: &[(&str, &str)] = &[
    ("assert", "assert(value: any, message: string?): any"),
    ("collectgarbage", "collectgarbage(option: string): number"),
    ("delay", "delay(delayTime: number, callback: function)"),
    ("elapsedTime", "elapsedTime(): number"),
    ("error", "error(message: any, level: number?)"),
    ("gcinfo", "gcinfo(): number"),
    ("getfenv", "getfenv(target: any?): table"),
    ("getmetatable", "getmetatable(t: any): table?"),
    ("ipairs", "ipairs(t: table): (function, table, number)"),
    ("loadstring", "loadstring(contents: string, chunkname: string?): (function?, string?)"),
    ("newproxy", "newproxy(addMetatable: boolean?): userdata"),
    ("next", "next(t: table, lastKey: any?): (any, any)"),
    ("pairs", "pairs(t: table): (function, table, nil)"),
    ("pcall", "pcall(func: function, ...: any): (boolean, ...any)"),
    ("print", "print(...: any)"),
    ("rawequal", "rawequal(a: any, b: any): boolean"),
    ("rawget", "rawget(t: table, index: any): any"),
    ("rawlen", "rawlen(t: table | string): number"),
    ("rawset", "rawset(t: table, index: any, value: any): table"),
    ("require", "require(module: ModuleScript | number): any"),
    ("select", "select(index: number | string, ...: any): ...any"),
    ("setfenv", "setfenv(target: any, env: table)"),
    ("setmetatable", "setmetatable(t: table, newMeta: table?): table"),
    ("settings", "settings(): GlobalSettings"),
    ("spawn", "spawn(callback: function)"),
    ("tick", "tick(): number"),
    ("time", "time(): number"),
    ("tonumber", "tonumber(arg: any, base: number?): number?"),
    ("tostring", "tostring(e: any): string"),
    ("type", "type(v: any): string"),
    ("typeof", "typeof(object: any): string"),
    ("unpack", "unpack(list: table, i: number?, j: number?): ...any"),
    ("UserSettings", "UserSettings(): UserSettings"),
    ("version", "version(): string"),
    ("wait", "wait(seconds: number?): (number, number)"),
    ("warn", "warn(...: any)"),
    ("xpcall", "xpcall(f: function, msgh: function, ...: any): (boolean, ...any)"),
];

/// Global tables: Luau's libraries, Roblox's `task`, and the datatypes'
/// constructors and constants.
const LIBS: &[(&str, &[M])] = &[
    ("math", &[
        ("abs", F, "number", "math.abs(x: number): number"),
        ("acos", F, "number", "math.acos(x: number): number"),
        ("asin", F, "number", "math.asin(x: number): number"),
        ("atan", F, "number", "math.atan(x: number): number"),
        ("atan2", F, "number", "math.atan2(y: number, x: number): number"),
        ("ceil", F, "number", "math.ceil(x: number): number"),
        ("clamp", F, "number", "math.clamp(x: number, min: number, max: number): number"),
        ("cos", F, "number", "math.cos(x: number): number"),
        ("cosh", F, "number", "math.cosh(x: number): number"),
        ("deg", F, "number", "math.deg(x: number): number"),
        ("e", P, "number", "math.e: number"),
        ("exp", F, "number", "math.exp(x: number): number"),
        ("floor", F, "number", "math.floor(x: number): number"),
        ("fmod", F, "number", "math.fmod(x: number, y: number): number"),
        ("frexp", F, "", "math.frexp(x: number): (number, number)"),
        ("huge", P, "number", "math.huge: number"),
        ("isfinite", F, "boolean", "math.isfinite(x: number): boolean"),
        ("isinf", F, "boolean", "math.isinf(x: number): boolean"),
        ("isnan", F, "boolean", "math.isnan(x: number): boolean"),
        ("ldexp", F, "number", "math.ldexp(s: number, e: number): number"),
        ("lerp", F, "number", "math.lerp(a: number, b: number, t: number): number"),
        ("log", F, "number", "math.log(x: number, base: number?): number"),
        ("log10", F, "number", "math.log10(x: number): number"),
        ("map", F, "number", "math.map(x: number, inmin: number, inmax: number, outmin: number, outmax: number): number"),
        ("max", F, "number", "math.max(x: number, ...: number): number"),
        ("min", F, "number", "math.min(x: number, ...: number): number"),
        ("modf", F, "", "math.modf(x: number): (number, number)"),
        ("nan", P, "number", "math.nan: number"),
        ("noise", F, "number", "math.noise(x: number, y: number?, z: number?): number"),
        ("phi", P, "number", "math.phi: number"),
        ("pi", P, "number", "math.pi: number"),
        ("pow", F, "number", "math.pow(x: number, y: number): number"),
        ("rad", F, "number", "math.rad(x: number): number"),
        ("random", F, "number", "math.random(m: number?, n: number?): number"),
        ("randomseed", F, "", "math.randomseed(x: number)"),
        ("round", F, "number", "math.round(x: number): number"),
        ("sign", F, "number", "math.sign(x: number): number"),
        ("sin", F, "number", "math.sin(x: number): number"),
        ("sinh", F, "number", "math.sinh(x: number): number"),
        ("sqrt", F, "number", "math.sqrt(x: number): number"),
        ("sqrt2", P, "number", "math.sqrt2: number"),
        ("tan", F, "number", "math.tan(x: number): number"),
        ("tanh", F, "number", "math.tanh(x: number): number"),
        ("tau", P, "number", "math.tau: number"),
    ]),
    ("string", &[
        ("byte", F, "", "string.byte(s: string, i: number?, j: number?): ...number"),
        ("char", F, "string", "string.char(...: number): string"),
        ("find", F, "", "string.find(s: string, pattern: string, init: number?, plain: boolean?): (number?, number?)"),
        ("format", F, "string", "string.format(formatstring: string, ...: any): string"),
        ("gmatch", F, "", "string.gmatch(s: string, pattern: string): function"),
        ("gsub", F, "", "string.gsub(s: string, pattern: string, repl: string | table | function, n: number?): (string, number)"),
        ("len", F, "number", "string.len(s: string): number"),
        ("lower", F, "string", "string.lower(s: string): string"),
        ("match", F, "", "string.match(s: string, pattern: string, init: number?): ...string?"),
        ("pack", F, "string", "string.pack(format: string, ...: any): string"),
        ("packsize", F, "number", "string.packsize(format: string): number"),
        ("rep", F, "string", "string.rep(s: string, n: number): string"),
        ("reverse", F, "string", "string.reverse(s: string): string"),
        ("split", F, "", "string.split(s: string, separator: string?): {string}"),
        ("sub", F, "string", "string.sub(s: string, i: number, j: number?): string"),
        ("unpack", F, "", "string.unpack(format: string, data: string, readStart: number?): ...any"),
        ("upper", F, "string", "string.upper(s: string): string"),
    ]),
    ("table", &[
        ("clear", F, "", "table.clear(t: table)"),
        ("clone", F, "", "table.clone(t: table): table"),
        ("concat", F, "string", "table.concat(t: {any}, sep: string?, i: number?, j: number?): string"),
        ("create", F, "", "table.create(count: number, value: any?): table"),
        ("find", F, "", "table.find(haystack: {any}, needle: any, init: number?): number?"),
        ("freeze", F, "", "table.freeze(t: table): table"),
        ("insert", F, "", "table.insert(t: {any}, pos: number?, value: any)"),
        ("isfrozen", F, "boolean", "table.isfrozen(t: table): boolean"),
        ("maxn", F, "number", "table.maxn(t: table): number"),
        ("move", F, "", "table.move(src: table, a: number, b: number, t: number, dst: table?): table"),
        ("pack", F, "", "table.pack(...: any): table"),
        ("remove", F, "", "table.remove(t: {any}, pos: number?): any"),
        ("sort", F, "", "table.sort(t: {any}, comp: function?)"),
        ("unpack", F, "", "table.unpack(list: table, i: number?, j: number?): ...any"),
    ]),
    ("coroutine", &[
        ("close", F, "", "coroutine.close(co: thread): (boolean, any?)"),
        ("create", F, "", "coroutine.create(f: function): thread"),
        ("isyieldable", F, "boolean", "coroutine.isyieldable(): boolean"),
        ("resume", F, "", "coroutine.resume(co: thread, ...: any): (boolean, ...any)"),
        ("running", F, "", "coroutine.running(): thread"),
        ("status", F, "string", "coroutine.status(co: thread): string"),
        ("wrap", F, "", "coroutine.wrap(f: function): function"),
        ("yield", F, "", "coroutine.yield(...: any): ...any"),
    ]),
    ("task", &[
        ("cancel", F, "", "task.cancel(thread: thread)"),
        ("defer", F, "", "task.defer(functionOrThread: function | thread, ...: any): thread"),
        ("delay", F, "", "task.delay(duration: number, functionOrThread: function | thread, ...: any): thread"),
        ("desynchronize", F, "", "task.desynchronize()"),
        ("spawn", F, "", "task.spawn(functionOrThread: function | thread, ...: any): thread"),
        ("synchronize", F, "", "task.synchronize()"),
        ("wait", F, "number", "task.wait(duration: number?): number"),
    ]),
    ("os", &[
        ("clock", F, "number", "os.clock(): number"),
        ("date", F, "", "os.date(formatString: string?, time: number?): table | string"),
        ("difftime", F, "number", "os.difftime(t2: number, t1: number): number"),
        ("time", F, "number", "os.time(time: table?): number"),
    ]),
    ("utf8", &[
        ("char", F, "string", "utf8.char(...: number): string"),
        ("charpattern", P, "string", "utf8.charpattern: string"),
        ("codepoint", F, "", "utf8.codepoint(s: string, i: number?, j: number?): ...number"),
        ("codes", F, "", "utf8.codes(s: string): function"),
        ("len", F, "", "utf8.len(s: string, i: number?, j: number?): (number?, number?)"),
        ("offset", F, "", "utf8.offset(s: string, n: number?, i: number?): number?"),
    ]),
    ("bit32", &[
        ("arshift", F, "number", "bit32.arshift(x: number, disp: number): number"),
        ("band", F, "number", "bit32.band(...: number): number"),
        ("bnot", F, "number", "bit32.bnot(x: number): number"),
        ("bor", F, "number", "bit32.bor(...: number): number"),
        ("btest", F, "boolean", "bit32.btest(...: number): boolean"),
        ("bxor", F, "number", "bit32.bxor(...: number): number"),
        ("byteswap", F, "number", "bit32.byteswap(n: number): number"),
        ("countlz", F, "number", "bit32.countlz(n: number): number"),
        ("countrz", F, "number", "bit32.countrz(n: number): number"),
        ("extract", F, "number", "bit32.extract(n: number, field: number, width: number?): number"),
        ("lrotate", F, "number", "bit32.lrotate(x: number, disp: number): number"),
        ("lshift", F, "number", "bit32.lshift(x: number, disp: number): number"),
        ("replace", F, "number", "bit32.replace(n: number, v: number, field: number, width: number?): number"),
        ("rrotate", F, "number", "bit32.rrotate(x: number, disp: number): number"),
        ("rshift", F, "number", "bit32.rshift(x: number, disp: number): number"),
    ]),
    ("buffer", &[
        ("copy", F, "", "buffer.copy(target: buffer, targetOffset: number, source: buffer, sourceOffset: number?, count: number?)"),
        ("create", F, "", "buffer.create(size: number): buffer"),
        ("fill", F, "", "buffer.fill(b: buffer, offset: number, value: number, count: number?)"),
        ("fromstring", F, "", "buffer.fromstring(str: string): buffer"),
        ("len", F, "number", "buffer.len(b: buffer): number"),
        ("readbits", F, "number", "buffer.readbits(b: buffer, bitOffset: number, bitCount: number): number"),
        ("readf32", F, "number", "buffer.readf32(b: buffer, offset: number): number"),
        ("readf64", F, "number", "buffer.readf64(b: buffer, offset: number): number"),
        ("readi16", F, "number", "buffer.readi16(b: buffer, offset: number): number"),
        ("readi32", F, "number", "buffer.readi32(b: buffer, offset: number): number"),
        ("readi8", F, "number", "buffer.readi8(b: buffer, offset: number): number"),
        ("readstring", F, "string", "buffer.readstring(b: buffer, offset: number, count: number): string"),
        ("readu16", F, "number", "buffer.readu16(b: buffer, offset: number): number"),
        ("readu32", F, "number", "buffer.readu32(b: buffer, offset: number): number"),
        ("readu8", F, "number", "buffer.readu8(b: buffer, offset: number): number"),
        ("tostring", F, "string", "buffer.tostring(b: buffer): string"),
        ("writebits", F, "", "buffer.writebits(b: buffer, bitOffset: number, bitCount: number, value: number)"),
        ("writef32", F, "", "buffer.writef32(b: buffer, offset: number, value: number)"),
        ("writef64", F, "", "buffer.writef64(b: buffer, offset: number, value: number)"),
        ("writei16", F, "", "buffer.writei16(b: buffer, offset: number, value: number)"),
        ("writei32", F, "", "buffer.writei32(b: buffer, offset: number, value: number)"),
        ("writei8", F, "", "buffer.writei8(b: buffer, offset: number, value: number)"),
        ("writestring", F, "", "buffer.writestring(b: buffer, offset: number, value: string, count: number?)"),
        ("writeu16", F, "", "buffer.writeu16(b: buffer, offset: number, value: number)"),
        ("writeu32", F, "", "buffer.writeu32(b: buffer, offset: number, value: number)"),
        ("writeu8", F, "", "buffer.writeu8(b: buffer, offset: number, value: number)"),
    ]),
    ("debug", &[
        ("dumpcodesize", F, "", "debug.dumpcodesize()"),
        ("info", F, "", "debug.info(level: number | function, options: string): ...any"),
        ("profilebegin", F, "", "debug.profilebegin(label: string)"),
        ("profileend", F, "", "debug.profileend()"),
        ("resetmemorycategory", F, "", "debug.resetmemorycategory()"),
        ("setmemorycategory", F, "", "debug.setmemorycategory(tag: string)"),
        ("traceback", F, "string", "debug.traceback(message: string?, level: number?): string"),
    ]),
    ("Instance", &[
        ("new", F, "#Instance", "Instance.new(className: string, parent: Instance?): Instance"),
        ("fromExisting", F, "#Instance", "Instance.fromExisting(existingInstance: Instance): Instance"),
    ]),
    ("Vector3", &[
        ("new", F, "self", "Vector3.new(x: number?, y: number?, z: number?): Vector3"),
        ("fromNormalId", F, "self", "Vector3.fromNormalId(normal: Enum.NormalId): Vector3"),
        ("fromAxis", F, "self", "Vector3.fromAxis(axis: Enum.Axis): Vector3"),
        ("zero", P, "self", "Vector3.zero: Vector3"),
        ("one", P, "self", "Vector3.one: Vector3"),
        ("xAxis", P, "self", "Vector3.xAxis: Vector3"),
        ("yAxis", P, "self", "Vector3.yAxis: Vector3"),
        ("zAxis", P, "self", "Vector3.zAxis: Vector3"),
    ]),
    ("Vector2", &[
        ("new", F, "self", "Vector2.new(x: number?, y: number?): Vector2"),
        ("zero", P, "self", "Vector2.zero: Vector2"),
        ("one", P, "self", "Vector2.one: Vector2"),
        ("xAxis", P, "self", "Vector2.xAxis: Vector2"),
        ("yAxis", P, "self", "Vector2.yAxis: Vector2"),
    ]),
    ("CFrame", &[
        ("new", F, "self", "CFrame.new(...): CFrame"),
        ("lookAt", F, "self", "CFrame.lookAt(at: Vector3, lookAt: Vector3, up: Vector3?): CFrame"),
        ("lookAlong", F, "self", "CFrame.lookAlong(at: Vector3, direction: Vector3, up: Vector3?): CFrame"),
        ("fromRotationBetweenVectors", F, "self", "CFrame.fromRotationBetweenVectors(from: Vector3, to: Vector3): CFrame"),
        ("fromEulerAngles", F, "self", "CFrame.fromEulerAngles(rx: number, ry: number, rz: number, order: Enum.RotationOrder?): CFrame"),
        ("fromEulerAnglesXYZ", F, "self", "CFrame.fromEulerAnglesXYZ(rx: number, ry: number, rz: number): CFrame"),
        ("fromEulerAnglesYXZ", F, "self", "CFrame.fromEulerAnglesYXZ(rx: number, ry: number, rz: number): CFrame"),
        ("Angles", F, "self", "CFrame.Angles(rx: number, ry: number, rz: number): CFrame"),
        ("fromOrientation", F, "self", "CFrame.fromOrientation(rx: number, ry: number, rz: number): CFrame"),
        ("fromAxisAngle", F, "self", "CFrame.fromAxisAngle(v: Vector3, r: number): CFrame"),
        ("fromMatrix", F, "self", "CFrame.fromMatrix(pos: Vector3, vX: Vector3, vY: Vector3, vZ: Vector3?): CFrame"),
        ("identity", P, "self", "CFrame.identity: CFrame"),
    ]),
    ("Color3", &[
        ("new", F, "self", "Color3.new(red: number?, green: number?, blue: number?): Color3"),
        ("fromRGB", F, "self", "Color3.fromRGB(red: number?, green: number?, blue: number?): Color3"),
        ("fromHSV", F, "self", "Color3.fromHSV(hue: number, saturation: number, value: number): Color3"),
        ("fromHex", F, "self", "Color3.fromHex(hex: string): Color3"),
        ("toHSV", F, "", "Color3.toHSV(color: Color3): (number, number, number)"),
    ]),
    ("BrickColor", &[
        ("new", F, "self", "BrickColor.new(value: number | string | Color3): BrickColor"),
        ("palette", F, "self", "BrickColor.palette(paletteValue: number): BrickColor"),
        ("random", F, "self", "BrickColor.random(): BrickColor"),
        ("White", F, "self", "BrickColor.White(): BrickColor"),
        ("Gray", F, "self", "BrickColor.Gray(): BrickColor"),
        ("DarkGray", F, "self", "BrickColor.DarkGray(): BrickColor"),
        ("Black", F, "self", "BrickColor.Black(): BrickColor"),
        ("Red", F, "self", "BrickColor.Red(): BrickColor"),
        ("Yellow", F, "self", "BrickColor.Yellow(): BrickColor"),
        ("Green", F, "self", "BrickColor.Green(): BrickColor"),
        ("Blue", F, "self", "BrickColor.Blue(): BrickColor"),
    ]),
    ("UDim", &[("new", F, "self", "UDim.new(scale: number?, offset: number?): UDim")]),
    ("UDim2", &[
        ("new", F, "self", "UDim2.new(xScale: number?, xOffset: number?, yScale: number?, yOffset: number?): UDim2"),
        ("fromScale", F, "self", "UDim2.fromScale(xScale: number, yScale: number): UDim2"),
        ("fromOffset", F, "self", "UDim2.fromOffset(xOffset: number, yOffset: number): UDim2"),
    ]),
    ("Ray", &[("new", F, "self", "Ray.new(origin: Vector3, direction: Vector3): Ray")]),
    ("Region3", &[("new", F, "self", "Region3.new(min: Vector3, max: Vector3): Region3")]),
    ("Region3int16", &[("new", F, "self", "Region3int16.new(min: Vector3int16, max: Vector3int16): Region3int16")]),
    ("Vector3int16", &[("new", F, "self", "Vector3int16.new(x: number?, y: number?, z: number?): Vector3int16")]),
    ("Vector2int16", &[("new", F, "self", "Vector2int16.new(x: number?, y: number?): Vector2int16")]),
    ("NumberRange", &[("new", F, "self", "NumberRange.new(min: number, max: number?): NumberRange")]),
    ("NumberSequence", &[("new", F, "self", "NumberSequence.new(...): NumberSequence")]),
    ("NumberSequenceKeypoint", &[("new", F, "self", "NumberSequenceKeypoint.new(time: number, value: number, envelope: number?): NumberSequenceKeypoint")]),
    ("ColorSequence", &[("new", F, "self", "ColorSequence.new(...): ColorSequence")]),
    ("ColorSequenceKeypoint", &[("new", F, "self", "ColorSequenceKeypoint.new(time: number, color: Color3): ColorSequenceKeypoint")]),
    ("TweenInfo", &[(
        "new",
        F,
        "self",
        "TweenInfo.new(time: number?, easingStyle: Enum.EasingStyle?, easingDirection: Enum.EasingDirection?, repeatCount: number?, reverses: boolean?, delayTime: number?): TweenInfo",
    )]),
    ("PhysicalProperties", &[(
        "new",
        F,
        "self",
        "PhysicalProperties.new(density: number | Enum.Material, friction: number?, elasticity: number?, frictionWeight: number?, elasticityWeight: number?): PhysicalProperties",
    )]),
    ("Rect", &[("new", F, "self", "Rect.new(...): Rect")]),
    ("Faces", &[("new", F, "self", "Faces.new(...: Enum.NormalId): Faces")]),
    ("Axes", &[("new", F, "self", "Axes.new(...: Enum.Axis | Enum.NormalId): Axes")]),
    ("Random", &[("new", F, "self", "Random.new(seed: number?): Random")]),
    ("RaycastParams", &[("new", F, "self", "RaycastParams.new(): RaycastParams")]),
    ("OverlapParams", &[("new", F, "self", "OverlapParams.new(): OverlapParams")]),
    ("DateTime", &[
        ("now", F, "self", "DateTime.now(): DateTime"),
        ("fromUnixTimestamp", F, "self", "DateTime.fromUnixTimestamp(unixTimestamp: number): DateTime"),
        ("fromUnixTimestampMillis", F, "self", "DateTime.fromUnixTimestampMillis(unixTimestampMillis: number): DateTime"),
        ("fromUniversalTime", F, "self", "DateTime.fromUniversalTime(year: number?, month: number?, day: number?, hour: number?, minute: number?, second: number?, millisecond: number?): DateTime"),
        ("fromLocalTime", F, "self", "DateTime.fromLocalTime(year: number?, month: number?, day: number?, hour: number?, minute: number?, second: number?, millisecond: number?): DateTime"),
        ("fromIsoDate", F, "self", "DateTime.fromIsoDate(isoDate: string): DateTime?"),
    ]),
    ("Font", &[
        ("new", F, "self", "Font.new(family: string, weight: Enum.FontWeight?, style: Enum.FontStyle?): Font"),
        ("fromEnum", F, "self", "Font.fromEnum(value: Enum.Font): Font"),
        ("fromName", F, "self", "Font.fromName(name: string, weight: Enum.FontWeight?, style: Enum.FontStyle?): Font"),
        ("fromId", F, "self", "Font.fromId(id: number, weight: Enum.FontWeight?, style: Enum.FontStyle?): Font"),
    ]),
    ("PathWaypoint", &[("new", F, "self", "PathWaypoint.new(position: Vector3, action: Enum.PathWaypointAction?, label: string?): PathWaypoint")]),
    ("CatalogSearchParams", &[("new", F, "self", "CatalogSearchParams.new(): CatalogSearchParams")]),
    ("SharedTable", &[
        ("new", F, "", "SharedTable.new(t: table?): SharedTable"),
        ("clear", F, "", "SharedTable.clear(st: SharedTable)"),
        ("clone", F, "", "SharedTable.clone(st: SharedTable, deep: boolean?): SharedTable"),
        ("cloneAndFreeze", F, "", "SharedTable.cloneAndFreeze(st: SharedTable, deep: boolean?): SharedTable"),
        ("increment", F, "number", "SharedTable.increment(st: SharedTable, key: string | number, delta: number): number"),
        ("isFrozen", F, "boolean", "SharedTable.isFrozen(st: SharedTable): boolean"),
        ("size", F, "number", "SharedTable.size(st: SharedTable): number"),
        ("update", F, "", "SharedTable.update(st: SharedTable, key: string | number, f: function)"),
    ]),
];

fn lib_members(name: &str) -> Option<&'static [M]> {
    LIBS.iter().find(|(n, _)| *n == name).map(|(_, m)| *m)
}

/// Members of a datatype value: properties after `.`, methods after `:`.
fn data_members(t: &str) -> Option<&'static [M]> {
    Some(match t {
        "Vector3" => &[
            ("X", P, "number", "X: number"),
            ("Y", P, "number", "Y: number"),
            ("Z", P, "number", "Z: number"),
            ("Magnitude", P, "number", "Magnitude: number"),
            ("Unit", P, "self", "Unit: Vector3"),
            ("Abs", Me, "self", "Abs(): Vector3"),
            ("Ceil", Me, "self", "Ceil(): Vector3"),
            ("Floor", Me, "self", "Floor(): Vector3"),
            ("Sign", Me, "self", "Sign(): Vector3"),
            ("Angle", Me, "number", "Angle(other: Vector3, axis: Vector3?): number"),
            ("Cross", Me, "self", "Cross(other: Vector3): Vector3"),
            ("Dot", Me, "number", "Dot(other: Vector3): number"),
            ("FuzzyEq", Me, "boolean", "FuzzyEq(other: Vector3, epsilon: number?): boolean"),
            ("Lerp", Me, "self", "Lerp(goal: Vector3, alpha: number): Vector3"),
            ("Max", Me, "self", "Max(...: Vector3): Vector3"),
            ("Min", Me, "self", "Min(...: Vector3): Vector3"),
        ],
        "Vector2" => &[
            ("X", P, "number", "X: number"),
            ("Y", P, "number", "Y: number"),
            ("Magnitude", P, "number", "Magnitude: number"),
            ("Unit", P, "self", "Unit: Vector2"),
            ("Abs", Me, "self", "Abs(): Vector2"),
            ("Ceil", Me, "self", "Ceil(): Vector2"),
            ("Floor", Me, "self", "Floor(): Vector2"),
            ("Sign", Me, "self", "Sign(): Vector2"),
            ("Angle", Me, "number", "Angle(other: Vector2, isSigned: boolean?): number"),
            ("Cross", Me, "number", "Cross(other: Vector2): number"),
            ("Dot", Me, "number", "Dot(v: Vector2): number"),
            ("FuzzyEq", Me, "boolean", "FuzzyEq(other: Vector2, epsilon: number?): boolean"),
            ("Lerp", Me, "self", "Lerp(v: Vector2, alpha: number): Vector2"),
            ("Max", Me, "self", "Max(...: Vector2): Vector2"),
            ("Min", Me, "self", "Min(...: Vector2): Vector2"),
        ],
        "CFrame" => &[
            ("Position", P, "Vector3", "Position: Vector3"),
            ("Rotation", P, "self", "Rotation: CFrame"),
            ("X", P, "number", "X: number"),
            ("Y", P, "number", "Y: number"),
            ("Z", P, "number", "Z: number"),
            ("LookVector", P, "Vector3", "LookVector: Vector3"),
            ("RightVector", P, "Vector3", "RightVector: Vector3"),
            ("UpVector", P, "Vector3", "UpVector: Vector3"),
            ("XVector", P, "Vector3", "XVector: Vector3"),
            ("YVector", P, "Vector3", "YVector: Vector3"),
            ("ZVector", P, "Vector3", "ZVector: Vector3"),
            ("Inverse", Me, "self", "Inverse(): CFrame"),
            ("Lerp", Me, "self", "Lerp(goal: CFrame, alpha: number): CFrame"),
            ("Orthonormalize", Me, "self", "Orthonormalize(): CFrame"),
            ("ToWorldSpace", Me, "self", "ToWorldSpace(...: CFrame): CFrame"),
            ("ToObjectSpace", Me, "self", "ToObjectSpace(...: CFrame): CFrame"),
            ("PointToWorldSpace", Me, "Vector3", "PointToWorldSpace(...: Vector3): Vector3"),
            ("PointToObjectSpace", Me, "Vector3", "PointToObjectSpace(...: Vector3): Vector3"),
            ("VectorToWorldSpace", Me, "Vector3", "VectorToWorldSpace(...: Vector3): Vector3"),
            ("VectorToObjectSpace", Me, "Vector3", "VectorToObjectSpace(...: Vector3): Vector3"),
            ("GetComponents", Me, "", "GetComponents(): (number, number, number, number, number, number, number, number, number, number, number, number)"),
            ("ToEulerAngles", Me, "", "ToEulerAngles(order: Enum.RotationOrder?): (number, number, number)"),
            ("ToEulerAnglesXYZ", Me, "", "ToEulerAnglesXYZ(): (number, number, number)"),
            ("ToEulerAnglesYXZ", Me, "", "ToEulerAnglesYXZ(): (number, number, number)"),
            ("ToOrientation", Me, "", "ToOrientation(): (number, number, number)"),
            ("ToAxisAngle", Me, "", "ToAxisAngle(): (Vector3, number)"),
            ("FuzzyEq", Me, "boolean", "FuzzyEq(other: CFrame, epsilon: number?): boolean"),
        ],
        "Color3" => &[
            ("R", P, "number", "R: number"),
            ("G", P, "number", "G: number"),
            ("B", P, "number", "B: number"),
            ("Lerp", Me, "self", "Lerp(color: Color3, alpha: number): Color3"),
            ("ToHSV", Me, "", "ToHSV(): (number, number, number)"),
            ("ToHex", Me, "string", "ToHex(): string"),
        ],
        "UDim2" => &[
            ("X", P, "UDim", "X: UDim"),
            ("Y", P, "UDim", "Y: UDim"),
            ("Width", P, "UDim", "Width: UDim"),
            ("Height", P, "UDim", "Height: UDim"),
            ("Lerp", Me, "self", "Lerp(goal: UDim2, alpha: number): UDim2"),
        ],
        "UDim" => &[("Scale", P, "number", "Scale: number"), ("Offset", P, "number", "Offset: number")],
        "BrickColor" => &[
            ("Number", P, "number", "Number: number"),
            ("Name", P, "string", "Name: string"),
            ("Color", P, "Color3", "Color: Color3"),
            ("r", P, "number", "r: number"),
            ("g", P, "number", "g: number"),
            ("b", P, "number", "b: number"),
        ],
        "Ray" => &[
            ("Origin", P, "Vector3", "Origin: Vector3"),
            ("Direction", P, "Vector3", "Direction: Vector3"),
            ("Unit", P, "self", "Unit: Ray"),
            ("ClosestPoint", Me, "Vector3", "ClosestPoint(point: Vector3): Vector3"),
            ("Distance", Me, "number", "Distance(point: Vector3): number"),
        ],
        "Region3" => &[
            ("CFrame", P, "CFrame", "CFrame: CFrame"),
            ("Size", P, "Vector3", "Size: Vector3"),
            ("ExpandToGrid", Me, "self", "ExpandToGrid(resolution: number): Region3"),
        ],
        "NumberRange" => &[("Min", P, "number", "Min: number"), ("Max", P, "number", "Max: number")],
        "NumberSequence" => &[("Keypoints", P, "", "Keypoints: {NumberSequenceKeypoint}")],
        "NumberSequenceKeypoint" => &[
            ("Time", P, "number", "Time: number"),
            ("Value", P, "number", "Value: number"),
            ("Envelope", P, "number", "Envelope: number"),
        ],
        "ColorSequence" => &[("Keypoints", P, "", "Keypoints: {ColorSequenceKeypoint}")],
        "ColorSequenceKeypoint" => &[("Time", P, "number", "Time: number"), ("Value", P, "Color3", "Value: Color3")],
        "TweenInfo" => &[
            ("Time", P, "number", "Time: number"),
            ("EasingStyle", P, "Enum.EasingStyle", "EasingStyle: Enum.EasingStyle"),
            ("EasingDirection", P, "Enum.EasingDirection", "EasingDirection: Enum.EasingDirection"),
            ("RepeatCount", P, "number", "RepeatCount: number"),
            ("Reverses", P, "boolean", "Reverses: boolean"),
            ("DelayTime", P, "number", "DelayTime: number"),
        ],
        "Rect" => &[
            ("Min", P, "Vector2", "Min: Vector2"),
            ("Max", P, "Vector2", "Max: Vector2"),
            ("Width", P, "number", "Width: number"),
            ("Height", P, "number", "Height: number"),
        ],
        "PhysicalProperties" => &[
            ("Density", P, "number", "Density: number"),
            ("Friction", P, "number", "Friction: number"),
            ("Elasticity", P, "number", "Elasticity: number"),
            ("FrictionWeight", P, "number", "FrictionWeight: number"),
            ("ElasticityWeight", P, "number", "ElasticityWeight: number"),
        ],
        "Vector3int16" => &[("X", P, "number", "X: number"), ("Y", P, "number", "Y: number"), ("Z", P, "number", "Z: number")],
        "Vector2int16" => &[("X", P, "number", "X: number"), ("Y", P, "number", "Y: number")],
        "RaycastResult" => &[
            ("Instance", P, "#BasePart", "Instance: BasePart"),
            ("Position", P, "Vector3", "Position: Vector3"),
            ("Normal", P, "Vector3", "Normal: Vector3"),
            ("Material", P, "Enum.Material", "Material: Enum.Material"),
            ("Distance", P, "number", "Distance: number"),
        ],
        "RaycastParams" => &[
            ("FilterDescendantsInstances", P, "", "FilterDescendantsInstances: {Instance}"),
            ("FilterType", P, "Enum.RaycastFilterType", "FilterType: Enum.RaycastFilterType"),
            ("IgnoreWater", P, "boolean", "IgnoreWater: boolean"),
            ("CollisionGroup", P, "string", "CollisionGroup: string"),
            ("RespectCanCollide", P, "boolean", "RespectCanCollide: boolean"),
            ("BruteForceAllSlow", P, "boolean", "BruteForceAllSlow: boolean"),
            ("AddToFilter", Me, "", "AddToFilter(instances: Instance | {Instance})"),
        ],
        "OverlapParams" => &[
            ("FilterDescendantsInstances", P, "", "FilterDescendantsInstances: {Instance}"),
            ("FilterType", P, "Enum.RaycastFilterType", "FilterType: Enum.RaycastFilterType"),
            ("MaxParts", P, "number", "MaxParts: number"),
            ("CollisionGroup", P, "string", "CollisionGroup: string"),
            ("RespectCanCollide", P, "boolean", "RespectCanCollide: boolean"),
            ("BruteForceAllSlow", P, "boolean", "BruteForceAllSlow: boolean"),
            ("AddToFilter", Me, "", "AddToFilter(instances: Instance | {Instance})"),
        ],
        "Random" => &[
            ("NextInteger", Me, "number", "NextInteger(min: number, max: number): number"),
            ("NextNumber", Me, "number", "NextNumber(min: number?, max: number?): number"),
            ("NextUnitVector", Me, "Vector3", "NextUnitVector(): Vector3"),
            ("Shuffle", Me, "", "Shuffle(tb: table)"),
            ("Clone", Me, "self", "Clone(): Random"),
        ],
        "DateTime" => &[
            ("UnixTimestamp", P, "number", "UnixTimestamp: number"),
            ("UnixTimestampMillis", P, "number", "UnixTimestampMillis: number"),
            ("ToUniversalTime", Me, "", "ToUniversalTime(): table"),
            ("ToLocalTime", Me, "", "ToLocalTime(): table"),
            ("ToIsoDate", Me, "string", "ToIsoDate(): string"),
            ("FormatUniversalTime", Me, "string", "FormatUniversalTime(format: string, locale: string): string"),
            ("FormatLocalTime", Me, "string", "FormatLocalTime(format: string, locale: string): string"),
        ],
        "Font" => &[
            ("Family", P, "string", "Family: string"),
            ("Weight", P, "Enum.FontWeight", "Weight: Enum.FontWeight"),
            ("Style", P, "Enum.FontStyle", "Style: Enum.FontStyle"),
            ("Bold", P, "boolean", "Bold: boolean"),
        ],
        "RBXScriptSignal" => &[
            ("Connect", Me, "RBXScriptConnection", "Connect(func: (...any) -> ()): RBXScriptConnection"),
            ("ConnectParallel", Me, "RBXScriptConnection", "ConnectParallel(func: (...any) -> ()): RBXScriptConnection"),
            ("Once", Me, "RBXScriptConnection", "Once(func: (...any) -> ()): RBXScriptConnection"),
            ("Wait", Me, "", "Wait(): ...any"),
        ],
        "RBXScriptConnection" => &[
            ("Connected", P, "boolean", "Connected: boolean"),
            ("Disconnect", Me, "", "Disconnect()"),
        ],
        "string" => &[
            ("byte", Me, "", "byte(i: number?, j: number?): ...number"),
            ("find", Me, "", "find(pattern: string, init: number?, plain: boolean?): (number?, number?)"),
            ("format", Me, "string", "format(...: any): string"),
            ("gmatch", Me, "", "gmatch(pattern: string): function"),
            ("gsub", Me, "", "gsub(pattern: string, repl: string | table | function, n: number?): (string, number)"),
            ("len", Me, "number", "len(): number"),
            ("lower", Me, "string", "lower(): string"),
            ("match", Me, "", "match(pattern: string, init: number?): ...string?"),
            ("rep", Me, "string", "rep(n: number): string"),
            ("reverse", Me, "string", "reverse(): string"),
            ("split", Me, "", "split(separator: string?): {string}"),
            ("sub", Me, "string", "sub(i: number, j: number?): string"),
            ("upper", Me, "string", "upper(): string"),
        ],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::InstanceBuilder;

    fn place() -> WeakDom {
        WeakDom::new(
            InstanceBuilder::new("DataModel")
                .with_child(
                    InstanceBuilder::new("Workspace")
                        .with_child(InstanceBuilder::new("Part").with_name("Baseplate"))
                        .with_child(InstanceBuilder::new("Model").with_name("Tower")),
                )
                .with_child(InstanceBuilder::new("Players"))
                .with_child(
                    InstanceBuilder::new("ServerScriptService").with_child(InstanceBuilder::new("Script").with_name("Main")),
                )
                .with_child(InstanceBuilder::new("ReplicatedStorage")),
        )
    }

    /// The labels offered at the end of `src`.
    fn labels(dom: &WeakDom, src: &str) -> Vec<String> {
        let sss = dom.root().children()[2];
        let script = dom.get_by_ref(sss).unwrap().children()[0];
        let ctx = CompletionCtx { dom, script, api: crate::api::get(), force: false };
        complete(src, src.len(), &ctx).1.into_iter().map(|i| i.label).collect()
    }

    fn has(list: &[String], want: &[&str]) -> bool {
        want.iter().all(|w| list.iter().any(|l| l == w))
    }

    #[test]
    fn words_and_members() {
        let dom = place();
        let l = labels(&dom, "wo");
        assert_eq!(&l[..2], ["workspace", "Workspace"], "{l:?}");
        assert!(has(&labels(&dom, "workspace.Ba"), &["Baseplate"]));
        assert!(has(&labels(&dom, "workspace.Baseplate."), &["Transparency", "Touched", "Anchored"]));
        let methods = labels(&dom, "workspace.Baseplate:");
        assert!(has(&methods, &["Clone", "Destroy", "GetChildren"]) && !methods.iter().any(|m| m == "Touched"));
        assert!(has(&labels(&dom, "script.Parent:Get"), &["GetChildren", "GetDescendants"]));
        assert!(has(&labels(&dom, "Enum.Material.Gr"), &["Grass", "Granite"]));
        assert!(has(&labels(&dom, "math.cl"), &["clamp"]));
        // A later word of a name matches; letters inside a word don't.
        assert!(has(&labels(&dom, "workspace.Baseplate:child"), &["FindFirstChild", "GetChildren"]));
        assert!(!labels(&dom, "workspace.Baseplate.Tr").iter().any(|l| l == "AncestryChanged"));
        assert!(labels(&dom, "-- wo").is_empty() && labels(&dom, "x = 1.").is_empty());
    }

    #[test]
    fn follows_types() {
        let dom = place();
        // Return and property types from the API dump.
        assert!(has(&labels(&dom, "game:GetService(\"Players\").LocalPlayer."), &["Character", "UserId"]));
        assert!(has(&labels(&dom, "workspace.Baseplate.Position."), &["X", "Magnitude", "Unit"]));
        assert!(has(&labels(&dom, "workspace.Baseplate.CFrame.Position:"), &["Dot", "Cross"]));
        assert!(has(&labels(&dom, "workspace.Baseplate.Touched:"), &["Connect", "Once", "Wait"]));
        assert!(has(&labels(&dom, "Instance.new(\"Part\")."), &["Anchored", "Size"]));
        assert!(has(&labels(&dom, "Vector3.new(1, 2, 3):"), &["Lerp"]));
        // Locals, from `= expr`, annotations, loops and Connect callbacks.
        let src = "local Players = game:GetService(\"Players\")\nlocal plr = Players.LocalPlayer\nplr.";
        assert!(has(&labels(&dom, src), &["Character", "Team"]));
        assert!(has(&labels(&dom, "local h: Humanoid\nh."), &["Health", "WalkSpeed"]));
        assert!(has(&labels(&dom, "for _, c in workspace:GetChildren() do\n c."), &["Name", "Parent"]));
        let touched = "workspace.Baseplate.Touched:Connect(function(hit)\n hit.";
        assert!(has(&labels(&dom, touched), &["Anchored", "CanCollide"]));
        let tower = "local t = workspace.Tower\nt.";
        assert!(has(&labels(&dom, tower), &["PrimaryPart"]));
    }

    #[test]
    fn string_arguments() {
        let dom = place();
        let services = labels(&dom, "game:GetService(\"Pl");
        assert_eq!(services.first().map(String::as_str), Some("Players"), "the place's own first: {services:?}");
        assert!(!services.iter().any(|s| s == "PlaceAssetIdsService"), "{services:?}");
        // No members of its own, but the place has it.
        assert!(has(&labels(&dom, "game:GetService(\"Rep"), &["ReplicatedStorage"]));
        let new = labels(&dom, "Instance.new(\"Pa");
        assert!(has(&new, &["Part"]) && !new.iter().any(|c| c == "Players"), "services aren't creatable");
        assert!(has(&labels(&dom, "workspace:WaitForChild(\"Ba"), &["Baseplate"]));
        assert!(has(&labels(&dom, "workspace.Baseplate:IsA(\"Base"), &["BasePart"]));
        assert!(has(&labels(&dom, "workspace.Baseplate:GetPropertyChangedSignal(\"Tra"), &["Transparency"]));
        assert!(labels(&dom, "print(\"Pla").is_empty());
        assert!(labels(&dom, "game:GetService(\"Players\")").is_empty(), "after the closing quote");
    }

    #[test]
    fn declared_names() {
        let dom = place();
        let src = "local a, b: number = 1, 2\n\
                   const c = 3\n\
                   local function f(x, y: string) end\n\
                   function g(...) end\n\
                   for i, v in pairs(t) do end\n\
                   local s = \"local notme\" -- local notme2\n";
        let ctx = CompletionCtx { dom: &dom, script: Ref::none(), api: None, force: false };
        let s = scan(src, &ctx);
        let names: Vec<&str> = s.names.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "f", "x", "y", "g", "i", "v", "s"]);
        // Naming something new offers nothing (but `local fu` → function).
        assert!(labels(&dom, "local fo").is_empty());
        assert_eq!(labels(&dom, "local fu"), ["function"]);
        assert!(labels(&dom, "function ma").is_empty());
    }

    /// Every function of the vendored Luau's libraries is listed, and every
    /// listed one exists (Roblox's own debug functions aside).
    #[test]
    fn luau_libraries_match() {
        let lua = mlua::Lua::new();
        let roblox_only = ["profilebegin", "profileend", "resetmemorycategory", "setmemorycategory", "dumpcodesize"];
        let deprecated = ["foreach", "foreachi", "getn", "ldexp_"];
        let mut wrong = Vec::new();
        for lib in ["math", "string", "table", "coroutine", "os", "utf8", "bit32", "buffer", "debug"] {
            let t: mlua::Table = lua.globals().get(lib).unwrap();
            let real: Vec<String> = t.pairs::<String, mlua::Value>().map(|p| p.unwrap().0).collect();
            let ours: Vec<&str> = lib_members(lib).unwrap().iter().map(|m| m.0).collect();
            for r in &real {
                if !ours.contains(&r.as_str()) && !deprecated.contains(&r.as_str()) {
                    wrong.push(format!("{lib}.{r} missing"));
                }
            }
            for o in &ours {
                if !real.iter().any(|r| r == o) && !roblox_only.contains(o) {
                    wrong.push(format!("{lib}.{o} isn't in Luau"));
                }
            }
        }
        assert!(wrong.is_empty(), "{wrong:?}");
    }
}
