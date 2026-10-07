//! Rust code generation.
//!
//! The output is deterministic (schema order, no timestamps) and clean under
//! the workspace lints, so it is checked in and diffed in review.

use core::fmt::Write as _;

use crate::IdlError;
use crate::lexer::Pos;
use crate::model::{Builtin, Direction, EnumDef, Field, Item, ItemKind, Prim, Schema, Type};
use crate::registry::Registry;

fn rust_type(ty: &Type) -> String {
    match ty {
        Type::Prim(p) => p.rust().to_owned(),
        Type::Builtin(b) => b.rust().to_owned(),
        Type::Named(n, _) => n.clone(),
        Type::List(inner, n) => format!("::mantis_core::wire::BoundedArray<{}, {n}>", rust_type(inner)),
        Type::String(n) => format!("::mantis_core::wire::WireString<{n}>"),
        Type::Option(inner) => format!("Option<{}>", rust_type(inner)),
    }
}

fn has_float(schema: &Schema, ty: &Type) -> bool {
    match ty {
        Type::Prim(p) => *p == Prim::F32,
        Type::Builtin(b) => *b == Builtin::Vec3,
        Type::String(_) => false,
        Type::List(inner, _) | Type::Option(inner) => has_float(schema, inner),
        Type::Named(n, _) => schema
            .item(n)
            .is_some_and(|i| i.fields().iter().any(|f| has_float(schema, &f.ty))),
    }
}

/// `MoveClaim` to `move_claim`.
fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn docs(out: &mut String, indent: &str, lines: &[String]) {
    for l in lines {
        if l.is_empty() {
            let _ = writeln!(out, "{indent}///");
        } else {
            let _ = writeln!(out, "{indent}/// {l}");
        }
    }
}

fn wire_impl(out: &mut String, name: &str, fields: &[Field]) {
    let _ = writeln!(out, "impl ::mantis_core::wire::Wire for {name} {{");
    let _ = writeln!(
        out,
        "    fn encode(&self, e: &mut ::mantis_core::wire::Encoder<'_>) {{"
    );
    if fields.is_empty() {
        let _ = writeln!(out, "        let _ = e;");
    }
    for f in fields {
        let _ = writeln!(
            out,
            "        ::mantis_core::wire::Wire::encode(&self.{}, e);",
            f.name
        );
    }
    let _ = writeln!(out, "    }}");
    let _ = writeln!(
        out,
        "    fn decode(d: &mut ::mantis_core::wire::Decoder<'_>) -> Result<Self, ::mantis_core::wire::DecodeError> {{"
    );
    if fields.is_empty() {
        let _ = writeln!(out, "        let _ = d;");
    }
    let _ = writeln!(out, "        Ok(Self {{");
    for f in fields {
        let _ = writeln!(
            out,
            "            {}: ::mantis_core::wire::Wire::decode(d)?,",
            f.name
        );
    }
    let _ = writeln!(out, "        }})");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
    let _ = writeln!(out, "impl ::mantis_core::wire::FuzzSample for {name} {{");
    let _ = writeln!(
        out,
        "    fn fuzz_sample(rng: &mut ::mantis_core::rng::Rng) -> Self {{"
    );
    if fields.is_empty() {
        let _ = writeln!(out, "        let _ = rng;");
    }
    let _ = writeln!(out, "        Self {{");
    for f in fields {
        let _ = writeln!(
            out,
            "            {}: ::mantis_core::wire::FuzzSample::fuzz_sample(rng),",
            f.name
        );
    }
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");
}

fn gen_struct(out: &mut String, schema: &Schema, item: &Item, fields: &[Field]) {
    docs(out, "", &item.docs);
    let eq = if fields.iter().any(|f| has_float(schema, &f.ty)) {
        ""
    } else {
        ", Eq"
    };
    let _ = writeln!(out, "#[derive(Clone, Copy, PartialEq{eq}, Debug)]");
    let _ = writeln!(out, "pub struct {} {{", item.name);
    for f in fields {
        docs(out, "    ", &f.docs);
        let _ = writeln!(out, "    pub {}: {},", f.name, rust_type(&f.ty));
    }
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
    wire_impl(out, &item.name, fields);
}

fn gen_enum(out: &mut String, item: &Item, def: &EnumDef) {
    docs(out, "", &item.docs);
    let repr = def.repr.rust();
    let _ = writeln!(out, "#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]");
    let _ = writeln!(out, "#[repr({repr})]");
    // A schema may add values: code outside the crate carries a wildcard
    // arm, so an addition is never a breaking change.
    let _ = writeln!(out, "#[non_exhaustive]");
    let _ = writeln!(out, "pub enum {} {{", item.name);
    for v in &def.variants {
        docs(out, "    ", &v.docs);
        let _ = writeln!(out, "    {} = {},", v.name, v.value);
    }
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
    let _ = writeln!(out, "impl ::mantis_core::wire::Wire for {} {{", item.name);
    let _ = writeln!(
        out,
        "    fn encode(&self, e: &mut ::mantis_core::wire::Encoder<'_>) {{"
    );
    let _ = writeln!(out, "        e.{repr}(*self as {repr});");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(
        out,
        "    fn decode(d: &mut ::mantis_core::wire::Decoder<'_>) -> Result<Self, ::mantis_core::wire::DecodeError> {{"
    );
    let _ = writeln!(out, "        match d.{repr}()? {{");
    for v in &def.variants {
        let _ = writeln!(out, "            {} => Ok(Self::{}),", v.value, v.name);
    }
    let _ = writeln!(
        out,
        "            _ => Err(::mantis_core::wire::DecodeError::Invalid(\"{}\")),",
        item.name
    );
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
    let _ = writeln!(out, "impl ::mantis_core::wire::FuzzSample for {} {{", item.name);
    let _ = writeln!(
        out,
        "    fn fuzz_sample(rng: &mut ::mantis_core::rng::Rng) -> Self {{"
    );
    let _ = writeln!(out, "        match rng.below({}) {{", def.variants.len());
    let (last, rest) = def
        .variants
        .split_last()
        .map_or((None, &[][..]), |(l, r)| (Some(l), r));
    for (i, v) in rest.iter().enumerate() {
        let _ = writeln!(out, "            {i} => Self::{},", v.name);
    }
    if let Some(l) = last {
        let _ = writeln!(out, "            _ => Self::{},", l.name);
    }
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");
}

fn gen_newtype(out: &mut String, item: &Item, prim: Prim) {
    docs(out, "", &item.docs);
    let p = prim.rust();
    let _ = writeln!(
        out,
        "#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]"
    );
    let _ = writeln!(out, "pub struct {}(pub {p});", item.name);
    let _ = writeln!(out);
    let _ = writeln!(out, "impl ::mantis_core::wire::Wire for {} {{", item.name);
    let _ = writeln!(
        out,
        "    fn encode(&self, e: &mut ::mantis_core::wire::Encoder<'_>) {{"
    );
    let _ = writeln!(out, "        e.{p}(self.0);");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(
        out,
        "    fn decode(d: &mut ::mantis_core::wire::Decoder<'_>) -> Result<Self, ::mantis_core::wire::DecodeError> {{"
    );
    let _ = writeln!(out, "        Ok(Self(d.{p}()?))");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
    let _ = writeln!(out, "impl ::mantis_core::wire::FuzzSample for {} {{", item.name);
    let _ = writeln!(
        out,
        "    fn fuzz_sample(rng: &mut ::mantis_core::rng::Rng) -> Self {{"
    );
    let _ = writeln!(
        out,
        "        Self(::mantis_core::wire::FuzzSample::fuzz_sample(rng))"
    );
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");
}

fn gen_dispatch(out: &mut String, schema: &Schema, registry: &Registry, dir: Direction) {
    let msgs: Vec<(&Item, u16, bool)> = schema
        .messages()
        .filter(|(_, d, _)| *d == dir)
        .filter_map(|(item, _, fields)| {
            let float = fields.iter().any(|f| has_float(schema, &f.ty));
            registry.live(&item.name).map(|e| (item, e.id, float))
        })
        .collect();
    if msgs.is_empty() {
        return;
    }
    let (enum_name, what) = match dir {
        Direction::Inbound => ("Inbound", "client to server"),
        Direction::Outbound => ("Outbound", "server to client"),
    };
    let eq = if msgs.iter().any(|m| m.2) { "" } else { ", Eq" };
    let _ = writeln!(out, "/// Every {what} message of this schema.");
    let _ = writeln!(out, "#[derive(Clone, Copy, PartialEq{eq}, Debug)]");
    let _ = writeln!(
        out,
        "#[allow(clippy::large_enum_variant)] // inline, allocation-free values; transient on network threads"
    );
    // A schema may add messages: code outside the crate carries a wildcard
    // arm, so an addition is never a breaking change.
    let _ = writeln!(out, "#[non_exhaustive]");
    let _ = writeln!(out, "pub enum {enum_name} {{");
    for (item, _, _) in &msgs {
        let _ = writeln!(out, "    /// See [`{0}`].", item.name);
        let _ = writeln!(out, "    {0}({0}),", item.name);
    }
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
    let _ = writeln!(out, "impl {enum_name} {{");
    let _ = writeln!(out, "    /// The registered id of the message.");
    let _ = writeln!(out, "    #[must_use]");
    let _ = writeln!(
        out,
        "    pub const fn id(&self) -> ::mantis_core::wire::MessageId {{"
    );
    let _ = writeln!(out, "        match self {{");
    for (item, _, _) in &msgs {
        let _ = writeln!(
            out,
            "            Self::{0}(_) => <{0} as ::mantis_core::wire::Message>::ID,",
            item.name
        );
    }
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);
    let _ = writeln!(out, "    /// Appends the payload encoding (without the id).");
    let _ = writeln!(out, "    pub fn encode(&self, out: &mut Vec<u8>) {{");
    let _ = writeln!(out, "        match self {{");
    for (item, _, _) in &msgs {
        let _ = writeln!(
            out,
            "            Self::{}(m) => ::mantis_core::wire::encode_into(m, out),",
            item.name
        );
    }
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);

    if dir == Direction::Inbound {
        gen_validators(out, &msgs);
    }
    gen_decoder(out, &msgs, dir, enum_name);
}

fn gen_validators(out: &mut String, msgs: &[(&Item, u16, bool)]) {
    {
        let _ = writeln!(
            out,
            "/// Hand-written validators: one required method per inbound message, so a"
        );
        let _ = writeln!(
            out,
            "/// new message without a validator does not compile (plan 6.7, decision 0017)."
        );
        let _ = writeln!(out, "pub trait Validators {{");
        for (item, _, _) in msgs {
            let _ = writeln!(out, "    /// Validates a decoded [`{}`].", item.name);
            let _ = writeln!(out, "    ///");
            let _ = writeln!(out, "    /// # Errors");
            let _ = writeln!(out, "    /// The reason the message is refused.");
            let _ = writeln!(
                out,
                "    fn validate_{}(&self, msg: &{}) -> Result<(), ::mantis_core::wire::ValidationError>;",
                snake(&item.name),
                item.name
            );
        }
        let _ = writeln!(out, "}}");
        let _ = writeln!(out);
        let _ = writeln!(out, "impl Inbound {{");
        let _ = writeln!(
            out,
            "    /// Runs this message's hand-written validator. Adapters that build `Inbound`"
        );
        let _ = writeln!(
            out,
            "    /// values from their own byte formats go through the same validators."
        );
        let _ = writeln!(out, "    ///");
        let _ = writeln!(out, "    /// # Errors");
        let _ = writeln!(
            out,
            "    /// [`::mantis_core::wire::WireError::Rejected`] with the validator's reason."
        );
        let _ = writeln!(
            out,
            "    pub fn validate(&self, validators: &impl Validators) -> Result<(), ::mantis_core::wire::WireError> {{"
        );
        let _ = writeln!(out, "        let (message, result) = match self {{");
        for (item, _, _) in msgs {
            let _ = writeln!(
                out,
                "            Self::{0}(m) => (\"{0}\", validators.validate_{1}(m)),",
                item.name,
                snake(&item.name)
            );
        }
        let _ = writeln!(out, "        }};");
        let _ = writeln!(
            out,
            "        result.map_err(|reason| ::mantis_core::wire::WireError::Rejected {{ message, reason }})"
        );
        let _ = writeln!(out, "    }}");
        let _ = writeln!(out, "}}");
        let _ = writeln!(out);
    }
}

fn gen_decoder(out: &mut String, msgs: &[(&Item, u16, bool)], dir: Direction, enum_name: &str) {
    let parse = if dir == Direction::Inbound {
        "parse_inbound"
    } else {
        "decode_outbound"
    };
    let _ = writeln!(
        out,
        "/// Decodes one {} message without validating it. Unknown ids fail closed.",
        if dir == Direction::Inbound {
            "inbound"
        } else {
            "outbound"
        }
    );
    if dir == Direction::Inbound {
        let _ = writeln!(
            out,
            "/// Adapters use this; the server validates with [`Inbound::validate`]."
        );
    }
    let _ = writeln!(out, "///");
    let _ = writeln!(out, "/// # Errors");
    let _ = writeln!(
        out,
        "/// [`::mantis_core::wire::WireError`] for an unknown id or a payload that does"
    );
    let _ = writeln!(out, "/// not decode exactly.");
    let _ = writeln!(out, "pub fn {parse}(");
    let _ = writeln!(out, "    id: ::mantis_core::wire::MessageId,");
    let _ = writeln!(out, "    bytes: &[u8],");
    let _ = writeln!(out, ") -> Result<{enum_name}, ::mantis_core::wire::WireError> {{");
    let _ = writeln!(out, "    Ok(match id.0 {{");
    for (item, id, _) in msgs {
        let _ = writeln!(
            out,
            "        {id} => {enum_name}::{}(::mantis_core::wire::decode_message(bytes)?),",
            item.name
        );
    }
    let _ = writeln!(
        out,
        "        _ => return Err(::mantis_core::wire::WireError::UnknownMessage(id)),"
    );
    let _ = writeln!(out, "    }})");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
    if dir == Direction::Inbound {
        let _ = writeln!(
            out,
            "/// Decodes and validates one inbound message. Unknown ids fail closed."
        );
        let _ = writeln!(out, "///");
        let _ = writeln!(out, "/// # Errors");
        let _ = writeln!(
            out,
            "/// [`::mantis_core::wire::WireError`] for an unknown id, a payload that does not"
        );
        let _ = writeln!(out, "/// decode exactly, or a validator refusal.");
        let _ = writeln!(out, "pub fn decode_inbound(");
        let _ = writeln!(out, "    id: ::mantis_core::wire::MessageId,");
        let _ = writeln!(out, "    bytes: &[u8],");
        let _ = writeln!(out, "    validators: &impl Validators,");
        let _ = writeln!(out, ") -> Result<Inbound, ::mantis_core::wire::WireError> {{");
        let _ = writeln!(out, "    let msg = parse_inbound(id, bytes)?;");
        let _ = writeln!(out, "    msg.validate(validators)?;");
        let _ = writeln!(out, "    Ok(msg)");
        let _ = writeln!(out, "}}");
        let _ = writeln!(out);
    }
}

/// Generates the Rust module for `schema`. `origin` names the schema file in
/// the header comment.
///
/// # Errors
/// A message missing from the registry, or a live registry entry with no
/// message (retire it instead).
pub fn generate(schema: &Schema, registry: &Registry, origin: &str) -> Result<String, IdlError> {
    for (item, _, _) in schema.messages() {
        if registry.live(&item.name).is_none() {
            return Err(IdlError::at(
                item.pos,
                &format!(
                    "message `{}` is not in the registry: append `<next id> {}`",
                    item.name, item.name
                ),
            ));
        }
    }
    for e in registry.entries.iter().filter(|e| !e.retired) {
        if !matches!(schema.item(&e.name).map(|i| &i.kind), Some(ItemKind::Message(..))) {
            return Err(IdlError::at(
                Pos::default(),
                &format!(
                    "registry entry `{} {}` has no message; mark it `retired`",
                    e.id, e.name
                ),
            ));
        }
    }
    let mut out = String::new();
    let _ = writeln!(out, "// @generated by mantis-idl from `{origin}`. Do not edit.");
    let _ = writeln!(
        out,
        "// Regenerate with `cargo run -p mantis-idl -- <schema>` and review the diff;"
    );
    let _ = writeln!(
        out,
        "// the testkit freshness test fails when this file is stale."
    );
    let _ = writeln!(out);
    for l in &schema.package_docs {
        if l.is_empty() {
            let _ = writeln!(out, "//!");
        } else {
            let _ = writeln!(out, "//! {l}");
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "/// Schema package name.");
    let _ = writeln!(out, "pub const PACKAGE: &str = \"{}\";", schema.package);
    for item in &schema.items {
        let _ = writeln!(out);
        match &item.kind {
            ItemKind::Newtype(p) => gen_newtype(&mut out, item, *p),
            ItemKind::Enum(def) => gen_enum(&mut out, item, def),
            ItemKind::Struct(fields) => gen_struct(&mut out, schema, item, fields),
            ItemKind::Message(_, fields) => {
                gen_struct(&mut out, schema, item, fields);
                if let Some(e) = registry.live(&item.name) {
                    let _ = writeln!(out);
                    let _ = writeln!(out, "impl ::mantis_core::wire::Message for {} {{", item.name);
                    let _ = writeln!(
                        out,
                        "    const ID: ::mantis_core::wire::MessageId = ::mantis_core::wire::MessageId({});",
                        e.id
                    );
                    let _ = writeln!(out, "    const NAME: &'static str = \"{}\";", item.name);
                    let _ = writeln!(out, "}}");
                }
            }
        }
    }
    let _ = writeln!(out);
    gen_dispatch(&mut out, schema, registry, Direction::Inbound);
    gen_dispatch(&mut out, schema, registry, Direction::Outbound);
    while out.ends_with("\n\n") {
        out.pop();
    }
    Ok(out)
}
