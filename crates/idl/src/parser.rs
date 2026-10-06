//! Parser and semantic checks for the schema language.

use std::collections::BTreeMap;

use crate::IdlError;
use crate::lexer::{Pos, Tok, lex};
use crate::model::{Builtin, Direction, EnumDef, Field, Item, ItemKind, Prim, Schema, Type, Variant};

struct Parser {
    toks: Vec<(Tok, Pos)>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at).map(|(t, _)| t)
    }

    fn pos(&self) -> Pos {
        self.toks
            .get(self.at)
            .or_else(|| self.toks.last())
            .map(|(_, p)| *p)
            .unwrap_or_default()
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.at).map(|(t, _)| t.clone());
        self.at += 1;
        t
    }

    fn sym(&mut self, c: char) -> Result<(), IdlError> {
        let pos = self.pos();
        match self.next() {
            Some(Tok::Sym(s)) if s == c => Ok(()),
            _ => Err(IdlError::at(pos, &format!("expected `{c}`"))),
        }
    }

    fn ident(&mut self, what: &str) -> Result<(String, Pos), IdlError> {
        let pos = self.pos();
        match self.next() {
            Some(Tok::Ident(s)) => Ok((s, pos)),
            _ => Err(IdlError::at(pos, &format!("expected {what}"))),
        }
    }

    fn keyword(&mut self, kw: &str) -> Result<(), IdlError> {
        let pos = self.pos();
        match self.next() {
            Some(Tok::Ident(s)) if s == kw => Ok(()),
            _ => Err(IdlError::at(pos, &format!("expected `{kw}`"))),
        }
    }

    fn int(&mut self, what: &str) -> Result<u64, IdlError> {
        let pos = self.pos();
        match self.next() {
            Some(Tok::Int(v)) => Ok(v),
            _ => Err(IdlError::at(pos, &format!("expected {what}"))),
        }
    }

    fn docs(&mut self) -> Vec<String> {
        let mut docs = Vec::new();
        while let Some(Tok::Doc(d)) = self.peek() {
            docs.push(d.clone());
            self.at += 1;
        }
        docs
    }

    fn require_docs(&mut self, what: &str) -> Result<Vec<String>, IdlError> {
        let pos = self.pos();
        let docs = self.docs();
        if docs.is_empty() {
            return Err(IdlError::at(pos, &format!("{what} needs a `///` doc comment")));
        }
        Ok(docs)
    }

    fn bound(&mut self) -> Result<u16, IdlError> {
        let pos = self.pos();
        let v = self.int("a bound")?;
        match u16::try_from(v) {
            Ok(b) if b > 0 => Ok(b),
            _ => Err(IdlError::at(pos, "bound must be 1..=65535")),
        }
    }

    fn ty(&mut self) -> Result<Type, IdlError> {
        let (name, pos) = self.ident("a type")?;
        Ok(match name.as_str() {
            "list" => {
                self.sym('<')?;
                let inner = self.ty()?;
                self.sym(',')?;
                let n = self.bound()?;
                self.sym('>')?;
                Type::List(Box::new(inner), n)
            }
            "string" => {
                self.sym('<')?;
                let n = self.bound()?;
                self.sym('>')?;
                Type::String(n)
            }
            "option" => {
                self.sym('<')?;
                let inner = self.ty()?;
                self.sym('>')?;
                Type::Option(Box::new(inner))
            }
            other => {
                if let Some(p) = Prim::from_name(other) {
                    Type::Prim(p)
                } else if let Some(b) = Builtin::from_name(other) {
                    Type::Builtin(b)
                } else if is_type_name(other) {
                    Type::Named(other.to_owned(), pos)
                } else {
                    return Err(IdlError::at(pos, &format!("unknown type `{other}`")));
                }
            }
        })
    }

    fn fields(&mut self) -> Result<Vec<Field>, IdlError> {
        self.sym('{')?;
        let mut fields = Vec::new();
        loop {
            if self.peek() == Some(&Tok::Sym('}')) {
                self.at += 1;
                return Ok(fields);
            }
            let docs = self.require_docs("a field")?;
            let (name, pos) = self.ident("a field name")?;
            if !is_field_name(&name) {
                return Err(IdlError::at(
                    pos,
                    &format!("field `{name}` must be snake_case and not a Rust keyword"),
                ));
            }
            if fields.iter().any(|f: &Field| f.name == name) {
                return Err(IdlError::at(pos, &format!("duplicate field `{name}`")));
            }
            self.sym(':')?;
            let ty = self.ty()?;
            self.sym(';')?;
            fields.push(Field { docs, name, ty });
        }
    }

    fn item(&mut self) -> Result<Item, IdlError> {
        let docs = self.require_docs("an item")?;
        let (kw, kw_pos) = self.ident("`enum`, `struct`, `message`, or `newtype`")?;
        let (name, pos) = self.ident("a type name")?;
        if !is_type_name(&name) || Prim::from_name(&name).is_some() || Builtin::from_name(&name).is_some() {
            return Err(IdlError::at(
                pos,
                &format!("type `{name}` must be PascalCase and not reserved"),
            ));
        }
        let kind = match kw.as_str() {
            "newtype" => {
                self.sym(':')?;
                let (prim_name, prim_pos) = self.ident("a primitive")?;
                let prim = Prim::from_name(&prim_name)
                    .filter(|p| *p != Prim::F32 && *p != Prim::Bool)
                    .ok_or_else(|| IdlError::at(prim_pos, "newtype wraps an integer primitive"))?;
                self.sym(';')?;
                ItemKind::Newtype(prim)
            }
            "enum" => {
                self.sym(':')?;
                let (r, rpos) = self.ident("`u8` or `u16`")?;
                let repr = match r.as_str() {
                    "u8" => Prim::U8,
                    "u16" => Prim::U16,
                    _ => return Err(IdlError::at(rpos, "enum repr must be `u8` or `u16`")),
                };
                self.sym('{')?;
                let mut variants: Vec<Variant> = Vec::new();
                loop {
                    if self.peek() == Some(&Tok::Sym('}')) {
                        self.at += 1;
                        break;
                    }
                    let vdocs = self.require_docs("a variant")?;
                    let (vname, vpos) = self.ident("a variant name")?;
                    if !is_type_name(&vname) {
                        return Err(IdlError::at(vpos, "variant must be PascalCase"));
                    }
                    self.sym('=')?;
                    let value = self.int("a variant value")?;
                    let max = if repr == Prim::U8 {
                        u64::from(u8::MAX)
                    } else {
                        u64::from(u16::MAX)
                    };
                    if value > max {
                        return Err(IdlError::at(vpos, "variant value does not fit the repr"));
                    }
                    if variants.iter().any(|v| v.name == vname || v.value == value) {
                        return Err(IdlError::at(vpos, "duplicate variant name or value"));
                    }
                    self.sym(';')?;
                    variants.push(Variant {
                        docs: vdocs,
                        name: vname,
                        value,
                    });
                }
                if variants.is_empty() {
                    return Err(IdlError::at(pos, "enum needs at least one variant"));
                }
                ItemKind::Enum(EnumDef { repr, variants })
            }
            "struct" => ItemKind::Struct(self.fields()?),
            "message" => {
                let (d, dpos) = self.ident("`inbound` or `outbound`")?;
                let dir = match d.as_str() {
                    "inbound" => Direction::Inbound,
                    "outbound" => Direction::Outbound,
                    _ => {
                        return Err(IdlError::at(
                            dpos,
                            "message direction must be `inbound` or `outbound`",
                        ));
                    }
                };
                ItemKind::Message(dir, self.fields()?)
            }
            _ => return Err(IdlError::at(kw_pos, &format!("unknown item `{kw}`"))),
        };
        Ok(Item {
            docs,
            name,
            pos,
            kind,
        })
    }
}

fn is_type_name(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_uppercase()) && c.all(|ch| ch.is_ascii_alphanumeric())
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false",
    "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "self", "static", "struct", "super", "trait", "true", "try", "type", "unsafe", "use", "where",
    "while", "yield",
];

fn is_field_name(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase())
        && c.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        && !RUST_KEYWORDS.contains(&s)
}

/// Parses and checks a schema.
///
/// # Errors
/// The first syntax or semantic [`IdlError`], with its position.
pub fn parse(source: &str) -> Result<Schema, IdlError> {
    let mut p = Parser {
        toks: lex(source)?,
        at: 0,
    };
    let package_docs = p.require_docs("the package")?;
    p.keyword("package")?;
    let (package, ppos) = p.ident("a package name")?;
    if !is_field_name(&package) {
        return Err(IdlError::at(ppos, "package name must be snake_case"));
    }
    p.sym(';')?;
    let mut items: Vec<Item> = Vec::new();
    while p.peek().is_some() {
        let item = p.item()?;
        if items.iter().any(|i| i.name == item.name) {
            return Err(IdlError::at(item.pos, &format!("duplicate type `{}`", item.name)));
        }
        items.push(item);
    }
    let schema = Schema {
        package_docs,
        package,
        items,
    };
    check_references(&schema)?;
    check_acyclic(&schema)?;
    Ok(schema)
}

fn named_refs(ty: &Type, out: &mut Vec<(String, Pos)>) {
    match ty {
        Type::Named(n, p) => out.push((n.clone(), *p)),
        Type::List(inner, _) | Type::Option(inner) => named_refs(inner, out),
        Type::Prim(_) | Type::Builtin(_) | Type::String(_) => {}
    }
}

fn check_references(schema: &Schema) -> Result<(), IdlError> {
    for item in &schema.items {
        let mut refs = Vec::new();
        for f in item.fields() {
            named_refs(&f.ty, &mut refs);
        }
        for (name, pos) in refs {
            match schema.item(&name) {
                None => return Err(IdlError::at(pos, &format!("unknown type `{name}`"))),
                Some(target) if matches!(target.kind, ItemKind::Message(..)) => {
                    return Err(IdlError::at(
                        pos,
                        &format!("`{name}` is a message; messages cannot be fields"),
                    ));
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

/// Inline storage makes a recursive type infinitely large: refuse cycles.
fn check_acyclic(schema: &Schema) -> Result<(), IdlError> {
    fn visit<'a>(
        schema: &'a Schema,
        item: &'a Item,
        state: &mut BTreeMap<&'a str, u8>,
    ) -> Result<(), IdlError> {
        match state.get(item.name.as_str()) {
            Some(2) => return Ok(()),
            Some(_) => {
                return Err(IdlError::at(
                    item.pos,
                    &format!("type `{}` contains itself", item.name),
                ));
            }
            None => {}
        }
        state.insert(&item.name, 1);
        let mut refs = Vec::new();
        for f in item.fields() {
            named_refs(&f.ty, &mut refs);
        }
        for (name, _) in refs {
            if let Some(target) = schema.items.iter().find(|i| i.name == name) {
                visit(schema, target, state)?;
            }
        }
        state.insert(&item.name, 2);
        Ok(())
    }
    let mut state: BTreeMap<&str, u8> = BTreeMap::new(); // 1 visiting, 2 done
    for item in &schema.items {
        visit(schema, item, &mut state)?;
    }
    Ok(())
}
