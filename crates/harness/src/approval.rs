use comet_proto::{UserInputAnswer, UserInputQuestion};
use serde_json::{Map, Number, Value, json};

const CONFIRM_ID: &str = "__comet_elicitation_confirm";
const SECRET_PREFIX: &str = "__comet_elicitation_secret:";

/// Map an ACP/MCP elicitation onto Comet's one shared question surface.
pub(crate) fn elicitation_questions(params: &Value) -> Vec<UserInputQuestion> {
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("The provider needs confirmation.");
    if params.get("mode").and_then(Value::as_str) == Some("url") {
        let url = params
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return vec![UserInputQuestion {
            id: CONFIRM_ID.into(),
            header: "Provider request".into(),
            question: if url.is_empty() {
                message.into()
            } else {
                format!("{message}\n\n{url}")
            },
            options: vec!["Continue".into(), "Cancel".into()],
            multi_select: false,
        }];
    }

    let schema = params.get("requestedSchema").unwrap_or(&Value::Null);
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return confirmation_question(message);
    };
    if properties.is_empty() {
        return confirmation_question(message);
    }

    properties
        .iter()
        .map(|(key, property)| {
            let title = property
                .get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.is_empty())
                .unwrap_or(key);
            let description = property
                .get("description")
                .and_then(Value::as_str)
                .filter(|description| !description.is_empty());
            let secret = property.get("writeOnly").and_then(Value::as_bool) == Some(true)
                || property.get("format").and_then(Value::as_str) == Some("password");
            if secret {
                return UserInputQuestion {
                    id: format!("{SECRET_PREFIX}{key}"),
                    header: title.into(),
                    question: "This request needs secret input, which Comet can’t collect safely."
                        .into(),
                    options: vec!["Cancel".into()],
                    multi_select: false,
                };
            }

            let (options, multi_select) = property_options(property);
            UserInputQuestion {
                id: key.clone(),
                header: title.into(),
                question: match description {
                    Some(description) => format!("{message}\n\n{description}"),
                    None => message.into(),
                },
                options,
                multi_select,
            }
        })
        .collect()
}

/// Convert answers from Comet's shared question surface back to ACP/MCP.
pub(crate) fn elicitation_response(
    params: &Value,
    questions: &[UserInputQuestion],
    answers: &[UserInputAnswer],
) -> Value {
    if questions
        .iter()
        .any(|question| question.id.starts_with(SECRET_PREFIX))
    {
        return json!({ "action": "decline" });
    }
    if params.get("mode").and_then(Value::as_str) == Some("url")
        || questions.iter().any(|question| question.id == CONFIRM_ID)
    {
        return if answer_labels(CONFIRM_ID, answers)
            .first()
            .is_some_and(|label| label.eq_ignore_ascii_case("continue"))
        {
            json!({ "action": "accept", "content": {} })
        } else {
            json!({ "action": "decline" })
        };
    }

    let properties = params
        .pointer("/requestedSchema/properties")
        .and_then(Value::as_object);
    let mut content = Map::new();
    for question in questions {
        let labels = answer_labels(&question.id, answers);
        if labels.is_empty() || labels.iter().all(String::is_empty) {
            continue;
        }
        let property = properties.and_then(|properties| properties.get(&question.id));
        content.insert(
            question.id.clone(),
            answer_value(property, labels, question.multi_select),
        );
    }
    if content.is_empty() && !questions.is_empty() {
        json!({ "action": "decline" })
    } else {
        json!({ "action": "accept", "content": content })
    }
}

fn confirmation_question(message: &str) -> Vec<UserInputQuestion> {
    vec![UserInputQuestion {
        id: CONFIRM_ID.into(),
        header: "Provider request".into(),
        question: message.into(),
        options: vec!["Continue".into(), "Cancel".into()],
        multi_select: false,
    }]
}

fn property_options(property: &Value) -> (Vec<String>, bool) {
    let property_type = property.get("type").and_then(Value::as_str);
    let values = if property_type == Some("array") {
        property.pointer("/items/enum").and_then(Value::as_array)
    } else {
        property.get("enum").and_then(Value::as_array)
    };
    let options = values
        .map(|values| values.iter().map(option_label).collect())
        .unwrap_or_else(|| {
            if property_type == Some("boolean") {
                vec!["Yes".into(), "No".into()]
            } else {
                Vec::new()
            }
        });
    (options, property_type == Some("array"))
}

fn option_label(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn answer_labels<'a>(id: &str, answers: &'a [UserInputAnswer]) -> &'a [String] {
    answers
        .iter()
        .find(|answer| answer.question_id == id)
        .map(|answer| answer.labels.as_slice())
        .unwrap_or_default()
}

fn answer_value(property: Option<&Value>, labels: &[String], multi_select: bool) -> Value {
    if multi_select {
        return Value::Array(labels.iter().cloned().map(Value::String).collect());
    }
    let value = labels.first().cloned().unwrap_or_default();
    match property
        .and_then(|property| property.get("type"))
        .and_then(Value::as_str)
    {
        Some("boolean") => Value::Bool(value.eq_ignore_ascii_case("yes") || value == "true"),
        Some("integer") => value
            .parse::<i64>()
            .map(Number::from)
            .map(Value::Number)
            .unwrap_or(Value::String(value)),
        Some("number") => value
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::String(value)),
        _ => Value::String(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_round_trips_typed_answers() {
        let params = json!({
            "message": "Provider setup",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "enabled": { "type": "boolean", "title": "Enable" },
                    "region": { "type": "string", "title": "Region", "enum": ["East", "West"] }
                }
            }
        });
        let questions = elicitation_questions(&params);
        let answers = vec![
            UserInputAnswer {
                question_id: "enabled".into(),
                labels: vec!["Yes".into()],
            },
            UserInputAnswer {
                question_id: "region".into(),
                labels: vec!["East".into()],
            },
        ];
        assert_eq!(
            elicitation_response(&params, &questions, &answers),
            json!({ "action": "accept", "content": { "enabled": true, "region": "East" } })
        );
    }

    #[test]
    fn secret_fields_fail_closed() {
        let params = json!({
            "message": "Provider setup",
            "mode": "form",
            "requestedSchema": {
                "properties": { "token": { "type": "string", "format": "password" } }
            }
        });
        let questions = elicitation_questions(&params);
        assert_eq!(questions[0].options, ["Cancel"]);
        assert_eq!(
            elicitation_response(&params, &questions, &[]),
            json!({ "action": "decline" })
        );
    }
}
