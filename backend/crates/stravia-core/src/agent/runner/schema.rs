use super::*;

pub(super) fn mime_matches(allowed: &str, actual: &str) -> bool {
    allowed == "*/*"
        || allowed == actual
        || allowed
            .strip_suffix("/*")
            .is_some_and(|prefix| actual.starts_with(&format!("{prefix}/")))
}

pub(super) fn parse_output(content: &str, schema: Option<&Value>) -> Result<Value, AgentRunError> {
    let Some(schema) = schema else {
        return Ok(Value::String(content.to_owned()));
    };
    let output: Value = serde_json::from_str(content).map_err(|error| {
        AgentRunError::new("output_schema_invalid", format!("invalid JSON: {error}"))
    })?;
    validate_schema_value(schema, &output, "$")?;
    Ok(output)
}

pub(super) fn validate_schema_value(
    schema: &Value,
    value: &Value,
    path: &str,
) -> Result<(), AgentRunError> {
    let kind = schema.get("type").and_then(Value::as_str);
    let valid_type = match kind {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("number") => value.is_number(),
        Some("integer") => value.as_i64().is_some() || value.as_u64().is_some(),
        Some("boolean") => value.is_boolean(),
        Some("null") => value.is_null(),
        Some(_) | None => true,
    };
    if !valid_type {
        return Err(AgentRunError::new(
            "output_schema_invalid",
            format!("{path} has the wrong type"),
        ));
    }
    if schema
        .get("const")
        .is_some_and(|expected| expected != value)
    {
        return Err(AgentRunError::new(
            "output_schema_invalid",
            format!("{path} does not match const"),
        ));
    }
    if schema
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|allowed| !allowed.contains(value))
    {
        return Err(AgentRunError::new(
            "output_schema_invalid",
            format!("{path} is not an allowed enum value"),
        ));
    }
    if let Some(number) = value.as_f64() {
        for (keyword, valid) in [
            (
                "minimum",
                schema
                    .get("minimum")
                    .and_then(Value::as_f64)
                    .is_none_or(|limit| number >= limit),
            ),
            (
                "maximum",
                schema
                    .get("maximum")
                    .and_then(Value::as_f64)
                    .is_none_or(|limit| number <= limit),
            ),
            (
                "exclusiveMinimum",
                schema
                    .get("exclusiveMinimum")
                    .and_then(Value::as_f64)
                    .is_none_or(|limit| number > limit),
            ),
            (
                "exclusiveMaximum",
                schema
                    .get("exclusiveMaximum")
                    .and_then(Value::as_f64)
                    .is_none_or(|limit| number < limit),
            ),
        ] {
            if !valid {
                return Err(AgentRunError::new(
                    "output_schema_invalid",
                    format!("{path} violates {keyword}"),
                ));
            }
        }
    }
    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if schema
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|limit| length < limit)
            || schema
                .get("maxLength")
                .and_then(Value::as_u64)
                .is_some_and(|limit| length > limit)
        {
            return Err(AgentRunError::new(
                "output_schema_invalid",
                format!("{path} has an invalid string length"),
            ));
        }
    }
    if let Some(array) = value.as_array() {
        let length = array.len() as u64;
        if schema
            .get("minItems")
            .and_then(Value::as_u64)
            .is_some_and(|limit| length < limit)
            || schema
                .get("maxItems")
                .and_then(Value::as_u64)
                .is_some_and(|limit| length > limit)
        {
            return Err(AgentRunError::new(
                "output_schema_invalid",
                format!("{path} has an invalid item count"),
            ));
        }
    }
    if let (Some(properties), Some(object)) = (
        schema.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    return Err(AgentRunError::new(
                        "output_schema_invalid",
                        format!("{path}.{name} is required"),
                    ));
                }
            }
        }
        for (name, child) in object {
            if let Some(child_schema) = properties.get(name) {
                validate_schema_value(child_schema, child, &format!("{path}.{name}"))?;
            } else if schema
                .get("additionalProperties")
                .is_some_and(|value| value == &Value::Bool(false))
            {
                return Err(AgentRunError::new(
                    "output_schema_invalid",
                    format!("{path}.{name} is not allowed"),
                ));
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), value.as_array()) {
        for (index, item) in array.iter().enumerate() {
            validate_schema_value(items, item, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}
