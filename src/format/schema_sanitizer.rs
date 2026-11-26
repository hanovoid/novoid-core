use serde_json::{json, Map, Value};
use std::collections::HashSet;

fn append_description_hint(schema: &mut Map<String, Value>, hint: &str) {
    let new_desc = match schema.get("description").and_then(|v| v.as_str()) {
        Some(existing) if !existing.is_empty() => format!("{existing} ({hint})"),
        _ => hint.to_string(),
    };
    schema.insert("description".to_string(), Value::String(new_desc));
}

fn score_schema_option(schema: &Value) -> i32 {
    let Some(obj) = schema.as_object() else {
        return 0;
    };
    let t = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if t == "object" || obj.contains_key("properties") {
        return 3;
    }
    if t == "array" || obj.contains_key("items") {
        return 2;
    }
    if !t.is_empty() && t != "null" {
        return 1;
    }
    0
}

fn convert_refs_to_hints(schema: Value) -> Value {
    match schema {
        Value::Array(arr) => Value::Array(arr.into_iter().map(convert_refs_to_hints).collect()),
        Value::Object(mut obj) => {
            if let Some(ref_str) = obj.get("$ref").and_then(|v| v.as_str()).map(|s| s.to_string())
            {
                let def_name = ref_str.split('/').next_back().unwrap_or("unknown");
                let hint = format!("See: {def_name}");
                let description = match obj.get("description").and_then(|v| v.as_str()) {
                    Some(d) if !d.is_empty() => format!("{d} ({hint})"),
                    _ => hint,
                };
                return json!({ "type": "object", "description": description });
            }

            if let Some(Value::Object(props)) = obj.remove("properties") {
                let mut new_props = Map::new();
                for (k, v) in props {
                    new_props.insert(k, convert_refs_to_hints(v));
                }
                obj.insert("properties".to_string(), Value::Object(new_props));
            }
            if let Some(items) = obj.remove("items") {
                obj.insert("items".to_string(), convert_refs_to_hints(items));
            }
            for key in ["anyOf", "oneOf", "allOf"] {
                if let Some(Value::Array(arr)) = obj.remove(key) {
                    obj.insert(
                        key.to_string(),
                        Value::Array(arr.into_iter().map(convert_refs_to_hints).collect()),
                    );
                }
            }
            Value::Object(obj)
        }
        other => other,
    }
}

fn add_enum_hints(schema: Value) -> Value {
    match schema {
        Value::Array(arr) => Value::Array(arr.into_iter().map(add_enum_hints).collect()),
        Value::Object(mut obj) => {
            if let Some(arr) = obj.get("enum").and_then(|v| v.as_array()).cloned() {
                if arr.len() > 1 && arr.len() <= 10 {
                    let vals: Vec<String> = arr
                        .iter()
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect();
                    append_description_hint(&mut obj, &format!("Allowed: {}", vals.join(", ")));
                }
            }
            if let Some(Value::Object(props)) = obj.remove("properties") {
                let mut new_props = Map::new();
                for (k, v) in props {
                    new_props.insert(k, add_enum_hints(v));
                }
                obj.insert("properties".to_string(), Value::Object(new_props));
            }
            if let Some(items) = obj.remove("items") {
                obj.insert("items".to_string(), add_enum_hints(items));
            }
            Value::Object(obj)
        }
        other => other,
    }
}

fn add_additional_properties_hints(schema: Value) -> Value {
    match schema {
        Value::Array(arr) => {
            Value::Array(arr.into_iter().map(add_additional_properties_hints).collect())
        }
        Value::Object(mut obj) => {
            if obj.get("additionalProperties").and_then(|v| v.as_bool()) == Some(false) {
                append_description_hint(&mut obj, "No extra properties allowed");
            }
            if let Some(Value::Object(props)) = obj.remove("properties") {
                let mut new_props = Map::new();
                for (k, v) in props {
                    new_props.insert(k, add_additional_properties_hints(v));
                }
                obj.insert("properties".to_string(), Value::Object(new_props));
            }
            if let Some(items) = obj.remove("items") {
                obj.insert("items".to_string(), add_additional_properties_hints(items));
            }
            Value::Object(obj)
        }
        other => other,
    }
}

fn move_constraints_to_description(schema: Value) -> Value {
    const CONSTRAINTS: [&str; 8] = [
        "minLength",
        "maxLength",
        "pattern",
        "minimum",
        "maximum",
        "minItems",
        "maxItems",
        "format",
    ];
    match schema {
        Value::Array(arr) => {
            Value::Array(arr.into_iter().map(move_constraints_to_description).collect())
        }
        Value::Object(mut obj) => {
            for c in CONSTRAINTS {
                if let Some(val) = obj.get(c).cloned() {
                    if !val.is_object() {
                        let s = match val {
                            Value::String(st) => st,
                            other => other.to_string(),
                        };
                        append_description_hint(&mut obj, &format!("{c}: {s}"));
                    }
                }
            }
            if let Some(Value::Object(props)) = obj.remove("properties") {
                let mut new_props = Map::new();
                for (k, v) in props {
                    new_props.insert(k, move_constraints_to_description(v));
                }
                obj.insert("properties".to_string(), Value::Object(new_props));
            }
            if let Some(items) = obj.remove("items") {
                obj.insert("items".to_string(), move_constraints_to_description(items));
            }
            Value::Object(obj)
        }
        other => other,
    }
}

fn merge_all_of(schema: Value) -> Value {
    match schema {
        Value::Array(arr) => Value::Array(arr.into_iter().map(merge_all_of).collect()),
        Value::Object(mut obj) => {
            if let Some(Value::Array(all_of)) = obj.remove("allOf") {
                let mut merged_props = Map::new();
                let mut merged_req: Vec<String> = Vec::new();
                let mut other_fields = Map::new();

                for sub in all_of {
                    if let Some(sub_obj) = sub.as_object() {
                        if let Some(Value::Object(props)) = sub_obj.get("properties") {
                            for (k, v) in props {
                                merged_props.insert(k.clone(), v.clone());
                            }
                        }
                        if let Some(Value::Array(req)) = sub_obj.get("required") {
                            for r in req {
                                if let Some(s) = r.as_str() {
                                    if !merged_req.contains(&s.to_string()) {
                                        merged_req.push(s.to_string());
                                    }
                                }
                            }
                        }
                        for (k, v) in sub_obj {
                            if k != "properties" && k != "required" && !other_fields.contains_key(k)
                            {
                                other_fields.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }

                for (k, v) in other_fields {
                    obj.entry(k).or_insert(v);
                }

                if !merged_props.is_empty() {
                    if let Some(Value::Object(existing_props)) = obj.remove("properties") {
                        for (k, v) in existing_props {
                            merged_props.insert(k, v);
                        }
                    }
                    obj.insert("properties".to_string(), Value::Object(merged_props));
                }

                if !merged_req.is_empty() {
                    if let Some(Value::Array(parent_req)) = obj.get("required") {
                        for r in parent_req {
                            if let Some(s) = r.as_str() {
                                if !merged_req.contains(&s.to_string()) {
                                    merged_req.push(s.to_string());
                                }
                            }
                        }
                    }
                    obj.insert(
                        "required".to_string(),
                        Value::Array(merged_req.into_iter().map(Value::String).collect()),
                    );
                }
            }

            if let Some(Value::Object(props)) = obj.remove("properties") {
                let mut new_props = Map::new();
                for (k, v) in props {
                    new_props.insert(k, merge_all_of(v));
                }
                obj.insert("properties".to_string(), Value::Object(new_props));
            }
            if let Some(items) = obj.remove("items") {
                obj.insert("items".to_string(), merge_all_of(items));
            }
            Value::Object(obj)
        }
        other => other,
    }
}

fn flatten_any_of_one_of(schema: Value) -> Value {
    match schema {
        Value::Array(arr) => Value::Array(arr.into_iter().map(flatten_any_of_one_of).collect()),
        Value::Object(mut obj) => {
            for union_key in ["anyOf", "oneOf"] {
                if let Some(Value::Array(options)) = obj.remove(union_key) {
                    let mut type_names = Vec::new();
                    let mut best_option: Option<Value> = None;
                    let mut best_score = -1;

                    for opt in &options {
                        if let Some(opt_obj) = opt.as_object() {
                            let t_name = opt_obj
                                .get("type")
                                .and_then(|v| v.as_str())
                                .or_else(|| opt_obj.contains_key("properties").then_some("object"));
                            if let Some(tn) = t_name {
                                if tn != "null" && !type_names.contains(&tn.to_string()) {
                                    type_names.push(tn.to_string());
                                }
                            }
                            let score = score_schema_option(opt);
                            if score > best_score {
                                best_score = score;
                                best_option = Some(opt.clone());
                            }
                        }
                    }

                    if let Some(best) = best_option {
                        let parent_desc = obj
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        if let Value::Object(flat) = flatten_any_of_one_of(best) {
                            for (k, v) in flat {
                                if k == "description" {
                                    if let Some(val_s) = v.as_str() {
                                        if !val_s.is_empty()
                                            && parent_desc.as_deref() != Some(val_s)
                                        {
                                            let merged = match &parent_desc {
                                                Some(p) if !p.is_empty() => format!("{p} ({val_s})"),
                                                _ => val_s.to_string(),
                                            };
                                            obj.insert(
                                                "description".to_string(),
                                                Value::String(merged),
                                            );
                                        }
                                    }
                                } else if !obj.contains_key(&k)
                                    || k == "type"
                                    || k == "properties"
                                    || k == "items"
                                {
                                    obj.insert(k, v);
                                }
                            }
                        }
                        if type_names.len() > 1 {
                            append_description_hint(
                                &mut obj,
                                &format!("Accepts: {}", type_names.join(" | ")),
                            );
                        }
                    }
                }
            }

            if let Some(Value::Object(props)) = obj.remove("properties") {
                let mut new_props = Map::new();
                for (k, v) in props {
                    new_props.insert(k, flatten_any_of_one_of(v));
                }
                obj.insert("properties".to_string(), Value::Object(new_props));
            }
            if let Some(items) = obj.remove("items") {
                obj.insert("items".to_string(), flatten_any_of_one_of(items));
            }
            Value::Object(obj)
        }
        other => other,
    }
}

fn flatten_type_arrays(
    schema: Value,
    nullable_props: Option<&mut HashSet<String>>,
    current_prop_name: Option<&str>,
) -> Value {
    match schema {
        Value::Array(arr) => Value::Array(
            arr.into_iter()
                .map(|s| flatten_type_arrays(s, None, None))
                .collect(),
        ),
        Value::Object(mut obj) => {
            if let Some(Value::Array(types)) = obj.get("type").cloned() {
                let strs: Vec<String> = types
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                let has_null = strs.iter().any(|t| t == "null");
                let non_null: Vec<String> = strs
                    .into_iter()
                    .filter(|t| !t.is_empty() && t != "null")
                    .collect();
                let first_type = non_null
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "string".to_string());
                obj.insert("type".to_string(), Value::String(first_type));

                if non_null.len() > 1 {
                    append_description_hint(
                        &mut obj,
                        &format!("Accepts: {}", non_null.join(" | ")),
                    );
                }
                if has_null {
                    append_description_hint(&mut obj, "nullable");
                    if let (Some(set), Some(name)) = (nullable_props, current_prop_name) {
                        set.insert(name.to_string());
                    }
                }
            }

            if let Some(Value::Object(props)) = obj.remove("properties") {
                let mut child_nullable = HashSet::new();
                let mut new_props = Map::new();
                for (k, v) in props {
                    let processed = flatten_type_arrays(v, Some(&mut child_nullable), Some(&k));
                    new_props.insert(k, processed);
                }
                obj.insert("properties".to_string(), Value::Object(new_props));

                if !child_nullable.is_empty() {
                    if let Some(Value::Array(req)) = obj.get_mut("required") {
                        req.retain(|v| {
                            v.as_str()
                                .map(|s| !child_nullable.contains(s))
                                .unwrap_or(true)
                        });
                        if req.is_empty() {
                            obj.remove("required");
                        }
                    }
                }
            }

            if let Some(items) = obj.remove("items") {
                obj.insert("items".to_string(), flatten_type_arrays(items, None, None));
            }
            Value::Object(obj)
        }
        other => other,
    }
}

pub fn sanitize_schema(schema: Option<&Value>) -> Value {
    let placeholder = || {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Reason for calling this tool"
                }
            },
            "required": ["reason"]
        })
    };

    let Some(Value::Object(obj)) = schema else {
        return placeholder();
    };

    let allowed = [
        "type",
        "description",
        "properties",
        "required",
        "items",
        "enum",
        "title",
    ];
    let mut sanitized = Map::new();

    for (k, v) in obj {
        if k == "const" {
            match v {
                Value::String(s) => {
                    sanitized.insert("enum".to_string(), json!([s]));
                }
                Value::Number(n) => {
                    sanitized.insert("enum".to_string(), json!([n.to_string()]));
                }
                Value::Bool(b) => {
                    sanitized.insert("enum".to_string(), json!([b.to_string()]));
                }
                other => {
                    append_description_hint(&mut sanitized, &format!("Must be: {other}"));
                }
            }
            continue;
        }
        if !allowed.contains(&k.as_str()) {
            continue;
        }
        if k == "enum" {
            if let Some(arr) = v.as_array() {
                let strs: Vec<Value> = arr
                    .iter()
                    .map(|item| match item {
                        Value::String(s) => Value::String(s.clone()),
                        other => Value::String(other.to_string()),
                    })
                    .collect();
                sanitized.insert("enum".to_string(), Value::Array(strs));
                continue;
            }
        }
        if k == "properties" {
            if let Some(props) = v.as_object() {
                let mut new_props = Map::new();
                for (pk, pv) in props {
                    new_props.insert(pk.clone(), sanitize_schema(Some(pv)));
                }
                sanitized.insert("properties".to_string(), Value::Object(new_props));
                continue;
            }
        }
        if k == "items" {
            if let Some(arr) = v.as_array() {
                sanitized.insert(
                    "items".to_string(),
                    Value::Array(arr.iter().map(|i| sanitize_schema(Some(i))).collect()),
                );
                continue;
            } else if v.is_object() {
                sanitized.insert("items".to_string(), sanitize_schema(Some(v)));
                continue;
            }
        }
        if v.is_object() {
            sanitized.insert(k.clone(), sanitize_schema(Some(v)));
        } else {
            sanitized.insert(k.clone(), v.clone());
        }
    }

    if !sanitized.contains_key("type") {
        sanitized.insert("type".to_string(), Value::String("object".to_string()));
    }

    if sanitized.get("type").and_then(|v| v.as_str()) == Some("object") {
        let empty_props = sanitized
            .get("properties")
            .and_then(|v| v.as_object())
            .map(|p| p.is_empty())
            .unwrap_or(true);
        if empty_props {
            sanitized.insert(
                "properties".to_string(),
                json!({
                    "reason": {
                        "type": "string",
                        "description": "Reason for calling this tool"
                    }
                }),
            );
            sanitized.insert("required".to_string(), json!(["reason"]));
        }
    }

    Value::Object(sanitized)
}

fn to_google_type(t: &str) -> String {
    match t.to_lowercase().as_str() {
        "string" => "STRING".to_string(),
        "number" => "NUMBER".to_string(),
        "integer" => "INTEGER".to_string(),
        "boolean" => "BOOLEAN".to_string(),
        "array" => "ARRAY".to_string(),
        "object" => "OBJECT".to_string(),
        "null" => "STRING".to_string(),
        _ => t.to_uppercase(),
    }
}

pub fn clean_schema(schema: Value) -> Value {
    let mut result = convert_refs_to_hints(schema);
    result = add_enum_hints(result);
    result = add_additional_properties_hints(result);
    result = move_constraints_to_description(result);
    result = merge_all_of(result);
    result = flatten_any_of_one_of(result);
    result = flatten_type_arrays(result, None, None);
    clean_schema_final(result)
}

fn clean_schema_final(schema: Value) -> Value {
    const UNSUPPORTED: [&str; 19] = [
        "additionalProperties",
        "default",
        "$schema",
        "$defs",
        "definitions",
        "$ref",
        "$id",
        "$comment",
        "title",
        "minLength",
        "maxLength",
        "pattern",
        "format",
        "minItems",
        "maxItems",
        "examples",
        "allOf",
        "anyOf",
        "oneOf",
    ];

    match schema {
        Value::Array(arr) => Value::Array(arr.into_iter().map(clean_schema_final).collect()),
        Value::Object(mut obj) => {
            for k in UNSUPPORTED {
                obj.remove(k);
            }

            if let Some(Value::Object(props)) = obj.remove("properties") {
                let mut new_props = Map::new();
                for (k, v) in props {
                    new_props.insert(k, clean_schema_final(v));
                }
                obj.insert("properties".to_string(), Value::Object(new_props));
            }

            if let Some(items) = obj.remove("items") {
                obj.insert("items".to_string(), clean_schema_final(items));
            }

            if let Some(Value::Object(props)) = obj.get("properties").cloned() {
                if let Some(Value::Array(req)) = obj.get_mut("required") {
                    req.retain(|v| v.as_str().map(|s| props.contains_key(s)).unwrap_or(false));
                    if req.is_empty() {
                        obj.remove("required");
                    }
                }
            }

            if let Some(t) = obj.get("type").and_then(|v| v.as_str()).map(to_google_type) {
                obj.insert("type".to_string(), Value::String(t));
            }

            if obj.get("type").and_then(|v| v.as_str()) == Some("ARRAY") {
                match obj.get("items").cloned() {
                    None => {
                        obj.insert("items".to_string(), json!({ "type": "STRING" }));
                    }
                    Some(Value::Array(arr)) => {
                        let best = arr
                            .into_iter()
                            .max_by_key(score_schema_option)
                            .unwrap_or_else(|| json!({ "type": "STRING" }));
                        obj.insert("items".to_string(), best);
                    }
                    Some(v) if !v.is_object() => {
                        obj.insert("items".to_string(), json!({ "type": "STRING" }));
                    }
                    _ => {}
                }
            }

            if obj.get("type").and_then(|v| v.as_str()) != Some("OBJECT") {
                obj.remove("properties");
                obj.remove("required");
            }

            Value::Object(obj)
        }
        other => other,
    }
}
