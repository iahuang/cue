//! Generates Rust FFI declarations from the Zig sources of `libopentui`.
//!
//! The Zig core has no C header, so the `export fn` signatures and the
//! `extern struct` definitions they reference are the ABI source of truth.
//! This is a deliberately small parser for the subset of Zig that appears in
//! those declarations. Anything it does not understand is a hard error, so ABI
//! drift in the vendored sources fails the build instead of producing wrong
//! bindings.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const ROOT_MODULE: &str = "lib.zig";

/// Zig aliases that are kept by name in the generated Rust instead of being
/// expanded, because they carry meaning at call sites.
const PRESERVED_ALIASES: &[(&str, &str, &str, &str)] = &[
    // (module, zig name, rust name, rust definition)
    ("lib.zig", "NativeHandle", "Handle", "u32"),
    ("ansi.zig", "RGBA", "Rgba", "[u16; 4]"),
];

/// Types that come from C imports (`@import("yoga")`), keyed by C name.
const C_TYPES: &[(&str, &str, &str)] = &[
    // (c name, pointer constness, opaque pointee)
    ("YGNodeRef", "*mut", "YGNode"),
    ("YGNodeConstRef", "*const", "YGNode"),
    ("YGConfigRef", "*mut", "YGConfig"),
    ("YGConfigConstRef", "*const", "YGConfig"),
];

const PRIMITIVES: &[(&str, &str)] = &[
    ("u8", "u8"),
    ("u16", "u16"),
    ("u32", "u32"),
    ("u64", "u64"),
    ("i8", "i8"),
    ("i16", "i16"),
    ("i32", "i32"),
    ("i64", "i64"),
    ("f32", "f32"),
    ("f64", "f64"),
    ("bool", "bool"),
    ("usize", "usize"),
    ("isize", "isize"),
    ("anyopaque", "c_void"),
];

/// Rust keywords that are legal as raw identifiers.
const KEYWORDS: &[&str] = &[
    "as", "async", "await", "box", "break", "const", "continue", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "static", "struct", "trait", "true", "type", "unsafe", "use", "where",
    "while", "yield", "try", "macro",
];

pub struct Output {
    pub bindings: String,
    /// Body of a test that references every declaration, so a symbol missing
    /// from the library fails at link time.
    pub link_test: String,
    /// Name of every exported function: the library's entire public ABI.
    pub exports: Vec<String>,
    /// Every Zig file the output depends on, for `rerun-if-changed`.
    pub inputs: Vec<PathBuf>,
}

pub fn generate(src_dir: &Path) -> Result<Output, String> {
    let mut gen = Generator::new(src_dir);
    gen.run()?;
    let mut link_test = String::from("{\nlet symbols: &[*const ()] = &[\n");
    for f in &gen.fns {
        let _ = writeln!(link_test, "    opentui_sys::{} as *const (),", f.name);
    }
    link_test.push_str("];\nassert!(symbols.iter().all(|p| !p.is_null()));\n}\n");
    Ok(Output {
        bindings: gen.render(),
        link_test,
        exports: gen.fns.iter().map(|f| f.name.clone()).collect(),
        inputs: gen.modules.values().map(|m| m.path.clone()).collect(),
    })
}

// ---------------------------------------------------------------------------
// Zig type syntax
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Ty {
    Void,
    Name(String),
    Optional(Box<Ty>),
    Pointer { is_const: bool, inner: Box<Ty> },
    Array { len: String, inner: Box<Ty> },
    Fn { params: Vec<Ty>, ret: Box<Ty> },
}

fn parse_ty(src: &str) -> Result<Ty, String> {
    let s = src.trim();
    if s.is_empty() {
        return Err("empty type".into());
    }
    if let Some(rest) = s.strip_prefix('?') {
        return Ok(Ty::Optional(Box::new(parse_ty(rest)?)));
    }
    if let Some(rest) = s.strip_prefix("[*") {
        // Many-item pointer, possibly with a sentinel: `[*]T`, `[*:0]T`.
        let close = rest
            .find(']')
            .ok_or_else(|| format!("unterminated pointer in `{s}`"))?;
        return parse_pointee(&rest[close + 1..]);
    }
    if let Some(rest) = s.strip_prefix('[') {
        let close = rest
            .find(']')
            .ok_or_else(|| format!("unterminated array in `{s}`"))?;
        let len = rest[..close].trim().to_string();
        if len.is_empty() || len.contains(':') {
            return Err(format!("slices are not C ABI types: `{s}`"));
        }
        return Ok(Ty::Array {
            len,
            inner: Box::new(parse_ty(&rest[close + 1..])?),
        });
    }
    if let Some(rest) = s.strip_prefix('*') {
        return parse_pointee(rest);
    }
    if s.starts_with("fn ") || s.starts_with("fn(") {
        return parse_fn_ty(s);
    }
    if s == "void" {
        return Ok(Ty::Void);
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        return Ok(Ty::Name(s.to_string()));
    }
    Err(format!("unsupported Zig type `{s}`"))
}

fn parse_pointee(rest: &str) -> Result<Ty, String> {
    let rest = rest.trim_start();
    let (is_const, rest) = match rest.strip_prefix("const ") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    if rest.starts_with("align(") || rest.starts_with("volatile ") || rest.starts_with("allowzero ")
    {
        return Err(format!("unsupported pointer qualifier in `{rest}`"));
    }
    Ok(Ty::Pointer {
        is_const,
        inner: Box::new(parse_ty(rest)?),
    })
}

fn parse_fn_ty(s: &str) -> Result<Ty, String> {
    let open = s.find('(').ok_or_else(|| format!("bad fn type `{s}`"))?;
    let close = matching_paren(s, open).ok_or_else(|| format!("unbalanced fn type `{s}`"))?;
    let params = split_top_level(&s[open + 1..close], ',')
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| parse_ty(param_type(&p)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut ret = s[close + 1..].trim();
    for cc in ["callconv(.c)", "callconv(.C)"] {
        if let Some(r) = ret.strip_prefix(cc) {
            ret = r.trim();
        }
    }
    if ret.starts_with("callconv") {
        return Err(format!("unsupported calling convention in `{s}`"));
    }
    Ok(Ty::Fn {
        params,
        ret: Box::new(parse_ty(ret)?),
    })
}

/// `name: T` -> `T`; a bare `T` (unnamed fn-type parameter) is returned as is.
fn param_type(param: &str) -> &str {
    let p = param.trim();
    match split_once_top_level(p, ':') {
        Some((_, ty)) => ty.trim(),
        None => p,
    }
}

fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices().skip_while(|(i, _)| *i < open) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
        if c == sep && depth == 0 {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    parts.push(cur);
    parts
}

fn split_once_top_level(s: &str, sep: char) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ if c == sep && depth == 0 => return Some((&s[..i], &s[i + 1..])),
            _ => {}
        }
    }
    None
}

fn strip_comment(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Zig modules
// ---------------------------------------------------------------------------

enum Decl {
    ExternStruct {
        fields: Vec<(String, String)>,
        line: usize,
    },
    /// A non-`extern` container. Only usable behind a pointer.
    Opaque,
    Alias(String),
}

struct Module {
    path: PathBuf,
    text: String,
    /// alias -> import target (a `.zig` path relative to `src/`, or a module name).
    imports: HashMap<String, String>,
    decls: HashMap<String, Decl>,
}

impl Module {
    fn load(src_dir: &Path, rel: &str) -> Result<Module, String> {
        let path = src_dir.join(rel);
        let text =
            fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        let base = Path::new(rel).parent().unwrap_or(Path::new(""));
        let mut imports = HashMap::new();
        let mut decls = HashMap::new();
        let lines: Vec<&str> = text.lines().collect();

        let mut i = 0;
        while i < lines.len() {
            let line = strip_comment(lines[i]);
            let decl = line
                .strip_prefix("pub const ")
                .or_else(|| line.strip_prefix("const "));
            let Some(decl) = decl else {
                i += 1;
                continue;
            };
            let Some((name, value)) = decl.split_once(" = ") else {
                i += 1;
                continue;
            };
            let name = name.trim().to_string();
            let value = value.trim();

            if let Some(target) = value
                .strip_prefix("@import(\"")
                .and_then(|v| v.split_once("\")"))
            {
                let target = target.0;
                let target = if target.ends_with(".zig") {
                    normalize(&base.join(target))
                } else {
                    target.to_string()
                };
                imports.insert(name, target);
                i += 1;
            } else if value.starts_with("extern struct {") {
                let start = i + 1;
                let (fields, end) = parse_struct_fields(&lines, i)?;
                decls.insert(
                    name,
                    Decl::ExternStruct {
                        fields,
                        line: start,
                    },
                );
                i = end + 1;
            } else if [
                "struct {",
                "opaque {",
                "union",
                "enum",
                "packed ",
                "extern union",
            ]
            .iter()
            .any(|p| value.starts_with(p))
            {
                decls.insert(name, Decl::Opaque);
                i += 1;
            } else if let Some(expr) = value.strip_suffix(';') {
                decls.insert(name, Decl::Alias(expr.trim().to_string()));
                i += 1;
            } else {
                // Multi-line initializer (function call, block, ...): not a type we need.
                i += 1;
            }
        }

        Ok(Module {
            path,
            text,
            imports,
            decls,
        })
    }
}

fn normalize(path: &Path) -> String {
    let mut out: Vec<String> = Vec::new();
    for c in path.components() {
        match c.as_os_str().to_str().unwrap() {
            "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s.to_string()),
        }
    }
    out.join("/")
}

/// Parses the fields of the `extern struct` opening at `lines[open]`.
/// Returns the fields and the index of the closing line.
fn parse_struct_fields(
    lines: &[&str],
    open: usize,
) -> Result<(Vec<(String, String)>, usize), String> {
    let mut fields = Vec::new();
    let mut depth = 1i32;
    for (i, raw) in lines.iter().enumerate().skip(open + 1) {
        let line = strip_comment(raw).trim();
        if depth == 1 {
            if line.starts_with("};") {
                return Ok((fields, i));
            }
            if let Some((name, rest)) = line.split_once(':') {
                let name = name.trim();
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    let rest = rest.trim().trim_end_matches(',');
                    let ty = match split_once_top_level(rest, '=') {
                        Some((ty, _default)) => ty,
                        None => rest,
                    };
                    fields.push((name.to_string(), collapse_ws(ty)));
                }
            }
        }
        depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
        if depth == 0 {
            return Ok((fields, i));
        }
    }
    Err(format!("unterminated extern struct at line {}", open + 1))
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// A Zig type with every name resolved to something Rust can express.
#[derive(Clone, Debug)]
enum RTy {
    Void,
    /// Primitive, preserved alias, or `#[repr(C)]` struct: a plain Rust type path.
    Value(String),
    /// Only valid behind a pointer.
    Opaque(String),
    Optional(Box<RTy>),
    Pointer {
        is_const: bool,
        inner: Box<RTy>,
    },
    Array {
        len: String,
        inner: Box<RTy>,
    },
    Fn {
        params: Vec<RTy>,
        ret: Box<RTy>,
    },
}

struct ExportFn {
    name: String,
    module: String,
    line: usize,
    params: Vec<(String, RTy)>,
    ret: RTy,
}

struct Struct {
    zig: String,
    module: String,
    line: usize,
    fields: Vec<(String, RTy)>,
}

struct Generator {
    src_dir: PathBuf,
    modules: BTreeMap<String, Module>,
    fns: Vec<ExportFn>,
    /// rust name -> struct
    structs: BTreeMap<String, Struct>,
    opaques: BTreeSet<String>,
    aliases: BTreeMap<String, String>,
    size_asserts: Vec<(String, String)>,
}

impl Generator {
    fn new(src_dir: &Path) -> Self {
        Generator {
            src_dir: src_dir.to_path_buf(),
            modules: BTreeMap::new(),
            fns: Vec::new(),
            structs: BTreeMap::new(),
            opaques: BTreeSet::new(),
            aliases: BTreeMap::new(),
            size_asserts: Vec::new(),
        }
    }

    fn module(&mut self, rel: &str) -> Result<&Module, String> {
        if !self.modules.contains_key(rel) {
            let m = Module::load(&self.src_dir, rel)?;
            self.modules.insert(rel.to_string(), m);
        }
        Ok(&self.modules[rel])
    }

    fn run(&mut self) -> Result<(), String> {
        let root = self.module(ROOT_MODULE)?;
        let root_text = root.text.clone();
        let root_imports = root.imports.clone();

        // Modules forced into the library by `_ = alias;` in a top-level
        // comptime block contribute their exports too.
        let mut export_modules = vec![ROOT_MODULE.to_string()];
        for block in comptime_blocks(&root_text) {
            for line in block.lines() {
                let line = strip_comment(line).trim();
                if let Some(alias) = line.strip_prefix("_ = ").and_then(|l| l.strip_suffix(';')) {
                    if let Some(target) = root_imports.get(alias.trim()) {
                        if target.ends_with(".zig") && !export_modules.contains(target) {
                            export_modules.push(target.clone());
                        }
                    }
                }
                if let Some(rest) = line.strip_prefix("std.debug.assert(@sizeOf(") {
                    if let Some((name, size)) = rest.split_once(") == ") {
                        let size = size.trim_end_matches(");").trim();
                        self.size_asserts
                            .push((name.trim().to_string(), size.to_string()));
                    }
                }
            }
        }

        for module in export_modules {
            let text = self.module(&module)?.text.clone();
            for (line, name, params, ret) in
                export_fns(&text).map_err(|e| format!("{module}: {e}"))?
            {
                let ctx = |e: String| format!("{module}:{line} `{name}`: {e}");
                let mut rparams = Vec::new();
                for p in split_top_level(&params, ',') {
                    let p = collapse_ws(&p);
                    if p.is_empty() {
                        continue;
                    }
                    let (pname, pty) = split_once_top_level(&p, ':')
                        .ok_or_else(|| ctx(format!("bad param `{p}`")))?;
                    let ty = self
                        .resolve(&module, &parse_ty(pty).map_err(ctx)?)
                        .map_err(ctx)?;
                    check_abi(&ty).map_err(ctx)?;
                    rparams.push((pname.trim().to_string(), ty));
                }
                let ret = self
                    .resolve(&module, &parse_ty(&ret).map_err(ctx)?)
                    .map_err(ctx)?;
                check_abi(&ret).map_err(ctx)?;
                self.fns.push(ExportFn {
                    name,
                    module: module.clone(),
                    line,
                    params: rparams,
                    ret,
                });
            }
        }

        // Size assertions name lib.zig structs; make sure they exist in the output.
        let asserts = std::mem::take(&mut self.size_asserts);
        for (name, size) in asserts {
            let rty = self.resolve(ROOT_MODULE, &Ty::Name(name))?;
            if let RTy::Value(rust) = rty {
                self.size_asserts.push((rust, size));
            }
        }
        Ok(())
    }

    fn resolve(&mut self, module: &str, ty: &Ty) -> Result<RTy, String> {
        Ok(match ty {
            Ty::Void => RTy::Void,
            Ty::Optional(t) => RTy::Optional(Box::new(self.resolve(module, t)?)),
            Ty::Pointer { is_const, inner } => RTy::Pointer {
                is_const: *is_const,
                inner: Box::new(self.resolve(module, inner)?),
            },
            Ty::Array { len, inner } => {
                if !len.chars().all(|c| c.is_ascii_digit()) {
                    return Err(format!("array length `{len}` is not a literal"));
                }
                RTy::Array {
                    len: len.clone(),
                    inner: Box::new(self.resolve(module, inner)?),
                }
            }
            Ty::Fn { params, ret } => RTy::Fn {
                params: params
                    .iter()
                    .map(|p| self.resolve(module, p))
                    .collect::<Result<_, _>>()?,
                ret: Box::new(self.resolve(module, ret)?),
            },
            Ty::Name(name) => self.resolve_name(module, name)?,
        })
    }

    fn resolve_name(&mut self, module: &str, name: &str) -> Result<RTy, String> {
        if let Some((_, rust)) = PRIMITIVES.iter().find(|(z, _)| *z == name) {
            return Ok(RTy::Value(rust.to_string()));
        }

        if let Some((alias, rest)) = name.split_once('.') {
            let target = self
                .module(module)?
                .imports
                .get(alias)
                .cloned()
                .ok_or_else(|| format!("unknown namespace `{alias}` in {module}"))?;
            if target.ends_with(".zig") {
                return self.resolve_name(&target, rest);
            }
            let (_, ptr, pointee) = C_TYPES
                .iter()
                .find(|(c, _, _)| *c == rest)
                .ok_or_else(|| format!("unmapped C type `{rest}` from `@import(\"{target}\")`"))?;
            self.opaques.insert(pointee.to_string());
            return Ok(RTy::Value(format!("{ptr} {pointee}")));
        }

        if let Some((_, _, rust, def)) = PRESERVED_ALIASES
            .iter()
            .find(|(m, z, _, _)| *m == module && *z == name)
        {
            self.aliases.insert(rust.to_string(), def.to_string());
            return Ok(RTy::Value(rust.to_string()));
        }

        let decl = match self.module(module)?.decls.get(name) {
            Some(Decl::Alias(expr)) => Decl::Alias(expr.clone()),
            Some(Decl::Opaque) => Decl::Opaque,
            Some(Decl::ExternStruct { fields, line }) => Decl::ExternStruct {
                fields: fields.clone(),
                line: *line,
            },
            None => return Err(format!("cannot find type `{name}` in {module}")),
        };
        match decl {
            Decl::Alias(expr) => {
                let ty = parse_ty(&expr).map_err(|e| format!("alias `{name}` in {module}: {e}"))?;
                self.resolve(module, &ty)
            }
            Decl::Opaque => {
                let rust = rust_type_name(module, name);
                self.opaques.insert(rust.clone());
                Ok(RTy::Opaque(rust))
            }
            Decl::ExternStruct { fields, line } => {
                let rust = rust_type_name(module, name);
                if let Some(existing) = self.structs.get(&rust) {
                    if existing.module != module || existing.zig != name {
                        return Err(format!(
                            "`{rust}` names both {}::{} and {module}::{name}",
                            existing.module, existing.zig
                        ));
                    }
                    return Ok(RTy::Value(rust));
                }
                // Insert a placeholder first so self-referential structs terminate.
                self.structs.insert(
                    rust.clone(),
                    Struct {
                        zig: name.to_string(),
                        module: module.to_string(),
                        line,
                        fields: Vec::new(),
                    },
                );
                let mut rfields = Vec::new();
                for (fname, fty) in fields {
                    let ctx = |e: String| format!("{module}:{line} field `{name}.{fname}`: {e}");
                    let ty = self
                        .resolve(module, &parse_ty(&fty).map_err(ctx)?)
                        .map_err(ctx)?;
                    check_abi(&ty).map_err(ctx)?;
                    rfields.push((fname, ty));
                }
                self.structs.get_mut(&rust).unwrap().fields = rfields;
                Ok(RTy::Value(rust))
            }
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "// @generated by opentui-sys/build/gen.rs from the Zig sources. Do not edit.\n"
        );
        let _ = writeln!(out, "use core::ffi::c_void;\n");

        for (name, def) in &self.aliases {
            let _ = writeln!(out, "pub type {name} = {def};");
        }
        out.push('\n');

        for name in &self.opaques {
            let _ = writeln!(
                out,
                "/// Opaque native type; only ever used behind a pointer."
            );
            let _ = writeln!(out, "#[repr(C)]");
            let _ = writeln!(
                out,
                "pub struct {name} {{\n    _data: [u8; 0],\n    _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,\n}}\n"
            );
        }

        for (name, s) in &self.structs {
            let _ = writeln!(out, "/// `{}` (`src/{}:{}`)", s.zig, s.module, s.line);
            let _ = writeln!(
                out,
                "#[repr(C)]\n#[derive(Debug, Clone, Copy)]\npub struct {name} {{"
            );
            for (fname, fty) in &s.fields {
                let _ = writeln!(out, "    pub {}: {},", ident(fname), render_ty(fty));
            }
            let _ = writeln!(out, "}}\n");
        }

        for (name, size) in &self.size_asserts {
            let _ = writeln!(
                out,
                "const _: () = assert!(core::mem::size_of::<{name}>() == {size});"
            );
        }
        out.push('\n');

        let _ = writeln!(out, "extern \"C\" {{");
        for f in &self.fns {
            let params = f
                .params
                .iter()
                .map(|(n, t)| format!("{}: {}", ident(n), render_ty(t)))
                .collect::<Vec<_>>()
                .join(", ");
            let ret = match &f.ret {
                RTy::Void => String::new(),
                t => format!(" -> {}", render_ty(t)),
            };
            let _ = writeln!(out, "    /// `src/{}:{}`", f.module, f.line);
            let _ = writeln!(out, "    pub fn {}({params}){ret};", f.name);
        }
        let _ = writeln!(out, "}}");
        out
    }
}

/// Rejects types that would silently change meaning across the C boundary.
fn check_abi(ty: &RTy) -> Result<(), String> {
    match ty {
        RTy::Optional(inner) => match inner.as_ref() {
            RTy::Pointer { .. } => check_abi(inner),
            RTy::Value(v) if v.starts_with('*') => Ok(()),
            other => Err(format!("optional non-pointer {other:?} has no C ABI")),
        },
        RTy::Opaque(name) => Err(format!("opaque type `{name}` used by value")),
        RTy::Fn { .. } => Err("function type used by value".into()),
        RTy::Pointer { inner, .. } => match inner.as_ref() {
            RTy::Opaque(_) | RTy::Fn { .. } => Ok(()),
            other => check_abi(other),
        },
        RTy::Array { inner, .. } => check_abi(inner),
        RTy::Void | RTy::Value(_) => Ok(()),
    }
}

fn render_ty(ty: &RTy) -> String {
    match ty {
        RTy::Void => "c_void".into(),
        RTy::Value(v) | RTy::Opaque(v) => v.clone(),
        RTy::Optional(inner) => match inner.as_ref() {
            RTy::Pointer { inner: pointee, .. } if matches!(pointee.as_ref(), RTy::Fn { .. }) => {
                format!("Option<{}>", render_ty(inner))
            }
            // Optional data pointers are nullable raw pointers.
            _ => render_ty(inner),
        },
        RTy::Pointer { is_const, inner } => match inner.as_ref() {
            RTy::Fn { params, ret } => {
                let params = params.iter().map(render_ty).collect::<Vec<_>>().join(", ");
                let ret = match ret.as_ref() {
                    RTy::Void => String::new(),
                    t => format!(" -> {}", render_ty(t)),
                };
                format!("unsafe extern \"C\" fn({params}){ret}")
            }
            inner => format!(
                "{} {}",
                if *is_const { "*const" } else { "*mut" },
                render_ty(inner)
            ),
        },
        RTy::Array { len, inner } => format!("[{}; {len}]", render_ty(inner)),
        RTy::Fn { .. } => unreachable!("rejected by check_abi"),
    }
}

/// `audio.zig` + `Stats` -> `AudioStats`. Types from `lib.zig` keep their names;
/// others are prefixed with their module unless the name already mentions it.
fn rust_type_name(module: &str, name: &str) -> String {
    if module == ROOT_MODULE {
        return name.to_string();
    }
    let stem = Path::new(module).file_stem().unwrap().to_str().unwrap();
    let prefix: String = stem
        .split(['-', '_'])
        .map(|w| {
            let mut cs = w.chars();
            cs.next()
                .map(|c| c.to_ascii_uppercase().to_string() + cs.as_str())
                .unwrap_or_default()
        })
        .collect();
    if name.contains(&prefix) {
        name.to_string()
    } else {
        format!("{prefix}{name}")
    }
}

fn ident(name: &str) -> String {
    if KEYWORDS.contains(&name) {
        format!("r#{name}")
    } else {
        name.to_string()
    }
}

/// Top-level `comptime { ... }` block bodies.
fn comptime_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("\ncomptime {") {
        let body = &rest[i + "\ncomptime {".len()..];
        let end = body.find("\n}").unwrap_or(body.len());
        blocks.push(body[..end].to_string());
        rest = &body[end..];
    }
    blocks
}

/// Top-level `export fn` declarations: (line, name, params source, return type source).
fn export_fns(text: &str) -> Result<Vec<(usize, String, String, String)>, String> {
    let mut out = Vec::new();
    let mut offset = 0;
    for (lineno, line) in text.lines().enumerate() {
        let start = offset;
        offset += line.len() + 1;
        let Some(rest) = line
            .strip_prefix("export fn ")
            .or_else(|| line.strip_prefix("pub export fn "))
        else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let open = start
            + line
                .find('(')
                .ok_or_else(|| format!("line {}: no `(`", lineno + 1))?;
        let close =
            matching_paren(text, open).ok_or_else(|| format!("line {}: unbalanced", lineno + 1))?;
        let body = text[close + 1..]
            .find('{')
            .ok_or_else(|| format!("line {}: no body", lineno + 1))?;
        let params = strip_comments_multiline(&text[open + 1..close]);
        let ret = collapse_ws(&text[close + 1..close + 1 + body]);
        out.push((lineno + 1, name, params, ret));
    }
    Ok(out)
}

fn strip_comments_multiline(s: &str) -> String {
    s.lines().map(strip_comment).collect::<Vec<_>>().join("\n")
}
