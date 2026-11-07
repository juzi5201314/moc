//! `ask<T>`/`hole<T>` 的类型实参 → JSON Schema（interface/type → schema 合成）。
//!
//! 可表达子集（spec §1）：基础类型、数组、对象（interface/type literal）、
//! 可选属性、字符串/数字字面量联合、嵌套引用（interface 间递归经 `$defs`/`$ref`）、
//! JSDoc `description`/`@tag`。其余一律 SCHEMA_UNREPRESENTABLE。

use super::analyze::TypeDecl;
use oxc_ast::ast::*;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// 合成 ask/hole 的返回 schema。
///
/// `T` 为 interface/引用名时输出 `{"$defs": {…}, "$ref": "#/$defs/Name"}`；
/// 内联类型直接输出其 schema。`$defs` 收录脚本内所有 interface/type 声明
/// （供嵌套引用解析）。
pub(super) fn synthesize(
    ty: &TSType<'_>,
    decls: &std::collections::HashMap<String, TypeDecl<'_>>,
    source: &str,
) -> Result<Value, String> {
    let mut defs: BTreeMap<String, Value> = BTreeMap::new();
    let mut visiting: BTreeSet<String> = BTreeSet::new();
    for (name, decl) in decls {
        let schema = decl_schema(*decl, decls, source, &mut defs, &mut visiting, name)?;
        defs.insert(name.clone(), schema);
    }
    let schema = type_schema(ty, decls, source, &mut defs, &mut BTreeSet::new())?;
    if let Value::Object(mut obj) = schema {
        if !defs.is_empty() {
            obj.insert(
                "$defs".into(),
                Value::Object(defs.into_iter().collect::<Map<_, _>>()),
            );
        }
        return Ok(Value::Object(obj));
    }
    if defs.is_empty() {
        return Ok(schema);
    }
    Ok(json!({"allOf": [schema], "$defs": defs}))
}

fn decl_schema(
    decl: TypeDecl<'_>,
    decls: &std::collections::HashMap<String, TypeDecl<'_>>,
    source: &str,
    defs: &mut BTreeMap<String, Value>,
    visiting: &mut BTreeSet<String>,
    name: &str,
) -> Result<Value, String> {
    if !visiting.insert(name.to_string()) {
        return Err(format!(
            "recursive type reference through `{name}` is not representable"
        ));
    }
    let schema = match decl {
        TypeDecl::Interface(d) => interface_schema(d, decls, source, defs, visiting)?,
        TypeDecl::Alias(d) => type_schema(&d.type_annotation, decls, source, defs, visiting)?,
    };
    visiting.remove(name);
    Ok(schema)
}

fn interface_schema(
    decl: &TSInterfaceDeclaration<'_>,
    decls: &std::collections::HashMap<String, TypeDecl<'_>>,
    source: &str,
    defs: &mut BTreeMap<String, Value>,
    visiting: &mut BTreeSet<String>,
) -> Result<Value, String> {
    let mut members: Vec<&TSSignature<'_>> = Vec::new();
    collect_interface_members(&decl.body.body, &mut members);
    // extends 子句：属性并入（interface extends 是类型级继承）。
    for heritage in &decl.extends {
        if let Expression::Identifier(id) = &heritage.expression
            && let Some(TypeDecl::Interface(parent)) = decls.get(id.name.as_str())
        {
            let mut parent_members: Vec<&TSSignature<'_>> = Vec::new();
            collect_interface_members(&parent.body.body, &mut parent_members);
            members.extend(parent_members);
        } else {
            return Err(format!(
                "interface `{}` extends a type that is not a declared interface",
                decl.id.name
            ));
        }
    }
    object_schema(
        members.iter().copied(),
        decls,
        source,
        defs,
        visiting,
        Some(decl.span),
        decl.id.name.as_str(),
    )
}

fn collect_interface_members<'a, 'b>(
    body: &'b [TSSignature<'a>],
    out: &mut Vec<&'b TSSignature<'a>>,
) {
    for sig in body {
        out.push(sig);
    }
}

fn object_schema<'a>(
    members: impl Iterator<Item = &'a TSSignature<'a>>,
    decls: &std::collections::HashMap<String, TypeDecl<'_>>,
    source: &str,
    defs: &mut BTreeMap<String, Value>,
    visiting: &mut BTreeSet<String>,
    _span: Option<oxc_span::Span>,
    decl_name: &str,
) -> Result<Value, String> {
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    let mut description: Option<String> = None;
    for sig in members {
        match sig {
            TSSignature::TSPropertySignature(p) => {
                let Some(name) = property_name(&p.key) else {
                    return Err(format!(
                        "computed property keys in `{decl_name}` are not representable"
                    ));
                };
                if p.computed {
                    return Err(format!(
                        "computed property `{name}` in `{decl_name}` is not representable"
                    ));
                }
                let Some(annotation) = &p.type_annotation else {
                    return Err(format!(
                        "property `{name}` in `{decl_name}` has no type annotation"
                    ));
                };
                let mut prop =
                    type_schema(&annotation.type_annotation, decls, source, defs, visiting)?;
                if let Some(doc) = jsdoc_text(p.span, source)
                    && let Value::Object(obj) = &mut prop
                {
                    obj.insert("description".into(), Value::String(doc));
                }
                properties.insert(name.clone(), prop);
                if !p.optional {
                    required.push(Value::String(name));
                }
            }
            TSSignature::TSIndexSignature(_) => {
                return Err(format!(
                    "index signatures in `{decl_name}` are not representable"
                ));
            }
            TSSignature::TSMethodSignature(m) => {
                return Err(format!(
                    "method `{}` in `{decl_name}` is not representable",
                    property_name(&m.key).unwrap_or_default()
                ));
            }
            _ => {}
        }
    }
    if let Some(doc) = jsdoc_text(_span.unwrap_or_default(), source) {
        description = Some(doc);
    }
    let mut obj = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });
    if !required.is_empty() {
        obj["required"] = Value::Array(required);
    }
    if let Some(d) = description {
        obj["description"] = Value::String(d);
    }
    Ok(obj)
}

fn type_schema(
    ty: &TSType<'_>,
    decls: &std::collections::HashMap<String, TypeDecl<'_>>,
    source: &str,
    defs: &mut BTreeMap<String, Value>,
    visiting: &mut BTreeSet<String>,
) -> Result<Value, String> {
    match ty {
        TSType::TSStringKeyword(_) => Ok(json!({"type": "string"})),
        TSType::TSNumberKeyword(_) => Ok(json!({"type": "number"})),
        TSType::TSBooleanKeyword(_) => Ok(json!({"type": "boolean"})),
        TSType::TSNullKeyword(_) => Ok(json!({"type": "null"})),
        TSType::TSAnyKeyword(_) | TSType::TSUnknownKeyword(_) => Ok(json!({})),
        TSType::TSNeverKeyword(_) | TSType::TSVoidKeyword(_) | TSType::TSUndefinedKeyword(_) => {
            Err("`never`/`void`/`undefined` result types are not representable".into())
        }
        TSType::TSLiteralType(l) => match &l.literal {
            TSLiteral::StringLiteral(s) => Ok(json!({"enum": [s.value.as_str()]})),
            TSLiteral::NumericLiteral(n) => Ok(json!({"enum": [number_json(n.value)]})),
            TSLiteral::BooleanLiteral(b) => Ok(json!({"enum": [b.value]})),
            _ => Err("literal type is not representable".into()),
        },
        TSType::TSArrayType(a) => Ok(json!({
            "type": "array",
            "items": type_schema(&a.element_type, decls, source, defs, visiting)?,
        })),
        TSType::TSTupleType(t) => {
            let mut items = Vec::new();
            for el in &t.element_types {
                match el {
                    TSTupleElement::TSOptionalType(_) | TSTupleElement::TSRestType(_) => {
                        return Err("tuple rest/optional elements are not representable".into());
                    }
                    other => items.push(type_schema(
                        other.to_ts_type(),
                        decls,
                        source,
                        defs,
                        visiting,
                    )?),
                }
            }
            Ok(json!({"type": "array", "prefixItems": items, "items": false}))
        }
        TSType::TSTypeLiteral(l) => object_schema(
            l.members.iter(),
            decls,
            source,
            defs,
            visiting,
            Some(l.span),
            "type literal",
        ),
        TSType::TSUnionType(u) => union_schema(&u.types, decls, source, defs, visiting),
        TSType::TSTypeReference(r) => {
            let name = match &r.type_name {
                TSTypeName::IdentifierReference(i) => i.name.as_str(),
                _ => return Err("qualified type references are not representable".into()),
            };
            match name {
                "Array" | "ReadonlyArray" => {
                    let arg = r
                        .type_arguments
                        .as_ref()
                        .and_then(|t| t.params.first())
                        .ok_or("Array<T> requires one type argument")?;
                    return Ok(
                        json!({"type": "array", "items": type_schema(arg, decls, source, defs, visiting)?}),
                    );
                }
                "Promise" => {
                    let arg = r
                        .type_arguments
                        .as_ref()
                        .and_then(|t| t.params.first())
                        .ok_or("Promise<T> requires one type argument")?;
                    return type_schema(arg, decls, source, defs, visiting);
                }
                "Map" | "Set" | "Date" | "RegExp" | "Record" | "Partial" | "Required"
                | "Readonly" | "Pick" | "Omit" | "Exclude" | "Extract" | "NonNullable" => {
                    return Err(format!(
                        "`{name}` is not representable as a JSON schema; declare a plain interface instead"
                    ));
                }
                _ => {}
            }
            if decls.contains_key(name) {
                Ok(json!({"$ref": format!("#/$defs/{name}")}))
            } else {
                Err(format!(
                    "type `{name}` is not a script-declared interface or type alias"
                ))
            }
        }
        TSType::TSParenthesizedType(p) => {
            type_schema(&p.type_annotation, decls, source, defs, visiting)
        }
        TSType::TSIntersectionType(_) => {
            Err("intersection types are not representable; declare a merged interface".into())
        }
        TSType::JSDocNullableType(n) => Ok(json!({
            "anyOf": [
                type_schema(&n.type_annotation, decls, source, defs, visiting)?,
                {"type": "null"},
            ],
        })),
        _ => Err(format!(
            "type `{}` is not representable as a JSON schema",
            ts_type_name(ty)
        )),
    }
}

fn union_schema(
    types: &[TSType<'_>],
    decls: &std::collections::HashMap<String, TypeDecl<'_>>,
    source: &str,
    defs: &mut BTreeMap<String, Value>,
    visiting: &mut BTreeSet<String>,
) -> Result<Value, String> {
    let mut enum_values: Vec<Value> = Vec::new();
    let mut variants: Vec<Value> = Vec::new();
    let mut has_null = false;
    for ty in types {
        match ty {
            TSType::TSNullKeyword(_) | TSType::TSUndefinedKeyword(_) => has_null = true,
            TSType::TSLiteralType(l) => match &l.literal {
                TSLiteral::StringLiteral(s) => enum_values.push(json!(s.value.as_str())),
                TSLiteral::NumericLiteral(n) => enum_values.push(number_json(n.value)),
                TSLiteral::BooleanLiteral(b) => enum_values.push(json!(b.value)),
                _ => variants.push(type_schema(ty, decls, source, defs, visiting)?),
            },
            _ => variants.push(type_schema(ty, decls, source, defs, visiting)?),
        }
    }
    if !enum_values.is_empty() && variants.is_empty() {
        return Ok(json!({"enum": enum_values}));
    }
    if !enum_values.is_empty() {
        variants.push(json!({"enum": enum_values}));
    }
    if has_null {
        variants.push(json!({"type": "null"}));
    }
    if variants.len() == 1 {
        Ok(variants.pop().unwrap())
    } else {
        Ok(json!({"anyOf": variants}))
    }
}

fn property_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(i) => Some(i.name.to_string()),
        PropertyKey::StringLiteral(s) => Some(s.value.to_string()),
        PropertyKey::NumericLiteral(n) => Some(n.value.to_string()),
        _ => None,
    }
}

/// 取成员/声明前的 JSDoc 注释文本作为 description。
/// oxc 的 JSDoc 结构较繁杂，直接定位 span 之前的 `/** … */` 注释更稳。
fn jsdoc_text(span: oxc_span::Span, source: &str) -> Option<String> {
    let before = source[..span.start as usize].trim_end();
    let start = before.rfind("/**")?;
    let end = before.rfind("*/")?;
    if end < start {
        return None;
    }
    let raw = &before[start + 3..end];
    // JSDoc 与成员间只允许空白/属性——若中间还有其他代码则不算它的 doc。
    if !before[end + 2..].trim().is_empty() {
        return None;
    }
    let text: String = raw
        .lines()
        .map(|l| l.trim().trim_start_matches('*').trim())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// 整数值写为 JSON integer（enum 消费者更自然）。
fn number_json(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.0e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

fn ts_type_name(ty: &TSType<'_>) -> &'static str {
    match ty {
        TSType::TSFunctionType(_) => "function type",
        TSType::TSConstructorType(_) => "constructor type",
        TSType::TSTypeOperatorType(_) => "type operator",
        TSType::TSIndexedAccessType(_) => "indexed access",
        TSType::TSConditionalType(_) => "conditional type",
        TSType::TSInferType(_) => "infer type",
        TSType::TSMappedType(_) => "mapped type",
        TSType::TSTemplateLiteralType(_) => "template literal type",
        _ => "unsupported type",
    }
}
