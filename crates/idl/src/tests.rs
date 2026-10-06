use super::*;

const SAMPLE: &str = r"
/// Sample package.
package sample;

/// An ability id.
newtype AbilityId : u32;

/// A stance.
enum Stance : u8 {
    /// Standing.
    Stand = 0;
    /// Crouching.
    Crouch = 3;
}

/// A slot.
struct Slot {
    /// Which bag.
    bag: u8;
    /// Optional label.
    label: option<string<16>>;
}

/// Cast something.
message Cast inbound {
    /// What.
    ability: AbilityId;
    /// At whom.
    target: option<entity>;
    /// Slots.
    slots: list<Slot, 4>;
    /// Stance.
    stance: Stance;
}

/// Where something is.
message Where outbound {
    /// The position.
    at: vec3;
}
";

const REGISTRY: &str = "# sample ids\n1 Cast\n2 retired Old\n3 Where\n";

#[test]
fn parses_the_sample() {
    let s = parse(SAMPLE).unwrap();
    assert_eq!(s.package, "sample");
    let names: Vec<_> = s.items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["AbilityId", "Stance", "Slot", "Cast", "Where"]);
    let msgs: Vec<_> = s
        .messages()
        .map(|(i, d, f)| (i.name.as_str(), d, f.len()))
        .collect();
    assert_eq!(
        msgs,
        [("Cast", Direction::Inbound, 4), ("Where", Direction::Outbound, 1)]
    );
    assert_eq!(
        s.item("Slot").unwrap().fields()[1].ty,
        Type::Option(Box::new(Type::String(16)))
    );
}

fn err(src: &str) -> String {
    parse(src).unwrap_err().message
}

#[test]
fn refuses_bad_schemas() {
    let head = "/// p.\npackage p;\n";
    assert!(err("package p;").contains("doc comment"));
    assert!(err(&format!("{head}struct S {{ }}")).contains("doc comment"));
    assert!(
        err(&format!("{head}/// s.\nstruct S {{ x: u8; }}")).contains("doc comment"),
        "fields need docs"
    );
    assert!(err(&format!("{head}/// s.\nstruct s {{ }}")).contains("PascalCase"));
    assert!(err(&format!("{head}/// s.\nstruct S {{ /// f.\n Bad: u8; }}")).contains("snake_case"));
    assert!(
        err(&format!("{head}/// s.\nstruct S {{ /// f.\n type: u8; }}")).contains("snake_case"),
        "keywords refused"
    );
    assert!(err(&format!("{head}/// s.\nstruct S {{ /// f.\n x: Missing; }}")).contains("unknown type"));
    assert!(
        err(&format!(
            "{head}/// s.\nstruct S {{ /// f.\n x: u8; /// g.\n x: u8; }}"
        ))
        .contains("duplicate field")
    );
    assert!(err(&format!("{head}/// s.\nstruct S {{ }}\n/// t.\nstruct S {{ }}")).contains("duplicate type"));
    assert!(err(&format!("{head}/// s.\nstruct S {{ /// f.\n x: list<u8, 0>; }}")).contains("bound"));
    assert!(
        err(&format!(
            "{head}/// s.\nstruct S {{ /// f.\n x: list<u8, 70000>; }}"
        ))
        .contains("bound")
    );
    assert!(err(&format!("{head}/// e.\nenum E : u8 {{ /// a.\n A = 256; }}")).contains("fit"));
    assert!(
        err(&format!(
            "{head}/// e.\nenum E : u8 {{ /// a.\n A = 1; /// b.\n B = 1; }}"
        ))
        .contains("duplicate variant")
    );
    assert!(err(&format!("{head}/// e.\nenum E : u32 {{ }}")).contains("repr"));
    assert!(err(&format!("{head}/// n.\nnewtype N : f32;")).contains("integer"));
    assert!(err(&format!("{head}/// m.\nmessage M sideways {{ }}")).contains("direction"));
    assert!(err(&format!("{head}/// s.\nstruct S {{ /// f.\n x: option<S>; }}")).contains("contains itself"));
    assert!(
        err(&format!(
            "{head}/// a.\nstruct A {{ /// b.\n b: B; }}\n/// b.\nstruct B {{ /// a.\n a: list<A, 2>; }}"
        ))
        .contains("contains itself")
    );
    assert!(
        err(&format!(
            "{head}/// m.\nmessage M inbound {{ }}\n/// s.\nstruct S {{ /// m.\n m: M; }}"
        ))
        .contains("message")
    );
    assert!(err(&format!("{head}$")).contains("unexpected character"));
    let e = parse(&format!("{head}/// s.\nstruct S {{ /// f.\n x: Missing; }}")).unwrap_err();
    assert_eq!((e.pos.line, e.pos.col), (5, 5), "positions are reported");
}

#[test]
fn registry_is_append_only() {
    let r = parse_registry(REGISTRY).unwrap();
    assert_eq!(r.entries.len(), 3);
    assert_eq!(r.live("Cast").map(|e| e.id), Some(1));
    assert_eq!(r.live("Old"), None, "retired");
    assert!(
        parse_registry("2 A\n1 B\n")
            .unwrap_err()
            .message
            .contains("ascending")
    );
    assert!(
        parse_registry("1 A\n1 B\n")
            .unwrap_err()
            .message
            .contains("ascending")
    );
    assert!(
        parse_registry("1 A\n2 A\n")
            .unwrap_err()
            .message
            .contains("twice")
    );
    assert!(parse_registry("0 A\n").unwrap_err().message.contains("1..=65535"));
    assert!(
        parse_registry("70000 A\n")
            .unwrap_err()
            .message
            .contains("1..=65535")
    );
    assert!(
        parse_registry("1 A B C\n")
            .unwrap_err()
            .message
            .contains("expected")
    );

    let lock = parse_registry("1 Cast\n2 Old\n").unwrap();
    check_lock(&lock, &r).unwrap(); // retiring a locked entry is allowed
    let renumbered = parse_registry("1 Cast\n4 Old\n5 Where\n").unwrap();
    assert!(
        check_lock(&lock, &renumbered)
            .unwrap_err()
            .message
            .contains("renumbered")
    );
    let unretired = parse_registry("1 Cast\n2 Old\n").unwrap();
    let locked_retired = parse_registry("1 Cast\n2 retired Old\n").unwrap();
    assert!(
        check_lock(&locked_retired, &unretired).is_err(),
        "a retired id never comes back"
    );
    assert!(
        check_lock(&lock, &parse_registry("1 Cast\n").unwrap()).is_err(),
        "removal refused"
    );
}

#[test]
fn codegen_requires_registry_agreement() {
    let s = parse(SAMPLE).unwrap();
    let missing = parse_registry("1 Cast\n").unwrap();
    assert!(
        generate(&s, &missing, "x.idl")
            .unwrap_err()
            .message
            .contains("not in the registry")
    );
    let orphan = parse_registry("1 Cast\n3 Where\n4 Gone\n").unwrap();
    assert!(
        generate(&s, &orphan, "x.idl")
            .unwrap_err()
            .message
            .contains("retired")
    );
}

#[test]
fn codegen_shape_and_determinism() {
    let s = parse(SAMPLE).unwrap();
    let r = parse_registry(REGISTRY).unwrap();
    let a = generate(&s, &r, "sample.idl").unwrap();
    assert_eq!(a, generate(&s, &r, "sample.idl").unwrap(), "deterministic");
    for needle in [
        "// @generated by mantis-idl from `sample.idl`. Do not edit.",
        "//! Sample package.",
        "pub struct AbilityId(pub u32);",
        "#[repr(u8)]",
        "    Crouch = 3,",
        "pub label: Option<::mantis_core::wire::WireString<16>>,",
        "pub slots: ::mantis_core::wire::BoundedArray<Slot, 4>,",
        "const ID: ::mantis_core::wire::MessageId = ::mantis_core::wire::MessageId(1);",
        "const ID: ::mantis_core::wire::MessageId = ::mantis_core::wire::MessageId(3);",
        "pub trait Validators {",
        "fn validate_cast(&self, msg: &Cast) -> Result<(), ::mantis_core::wire::ValidationError>;",
        "pub fn decode_inbound(",
        "pub fn decode_outbound(",
        "_ => Err(::mantis_core::wire::DecodeError::Invalid(\"Stance\")),",
        "_ => return Err(::mantis_core::wire::WireError::UnknownMessage(id)),",
        "1 => Inbound::Cast(::mantis_core::wire::decode_message(bytes)?),",
        "msg.validate(validators)?;",
        "pub fn parse_inbound(",
    ] {
        assert!(a.contains(needle), "missing `{needle}` in:\n{a}");
    }
    // Floats prevent Eq; integer-only types derive it.
    assert!(a.contains("#[derive(Clone, Copy, PartialEq, Debug)]\npub struct Where"));
    assert!(a.contains("#[derive(Clone, Copy, PartialEq, Eq, Debug)]\npub struct Slot"));
    assert!(
        !a.contains("retired") && !a.contains("Old"),
        "retired ids generate nothing"
    );
    assert!(!a.ends_with("\n\n"));
}

#[test]
fn compile_tags_errors_with_the_file() {
    let unit = Unit {
        schema: SAMPLE.to_owned(),
        registry: REGISTRY.to_owned(),
        lock: "1 Cast\n2 Moved\n".to_owned(),
    };
    let e = compile(&unit, "sample.idl").unwrap_err();
    assert!(e.message.starts_with("registry vs lock:"), "{e}");
    let ok = Unit {
        lock: "1 Cast\n".to_owned(),
        ..unit
    };
    assert!(compile(&ok, "sample.idl").is_ok());
}
