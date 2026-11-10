use crate::{HarnessId, Model, ReasoningLevel};

/// Split account-catalog Google effort variants without dropping their provider.
pub fn google_model_variant(id: &str) -> Option<(&str, ReasoningLevel)> {
    let raw = id
        .strip_prefix("google-antigravity/")
        .or_else(|| id.strip_prefix("antigravity/"))
        .unwrap_or(id);
    if !raw.starts_with("gemini-") {
        return None;
    }
    let (base, suffix) = id.rsplit_once('-')?;
    if !base.rsplit('/').next()?.starts_with("gemini-") {
        return None;
    }
    let effort = match suffix {
        "low" => ReasoningLevel::Low,
        "medium" => ReasoningLevel::Medium,
        "high" => ReasoningLevel::High,
        "xhigh" => ReasoningLevel::XHigh,
        "max" => ReasoningLevel::Max,
        _ => return None,
    };
    Some((base, effort))
}

fn effort_label_annotations(effort: ReasoningLevel) -> &'static [&'static str] {
    match effort {
        ReasoningLevel::Low => &[" (low)", " (low thinking)"],
        ReasoningLevel::Medium => &[" (medium)", " (medium thinking)"],
        ReasoningLevel::High => &[" (high)", " (high thinking)"],
        // CCPA/Hub labels `xhigh` as "(Extra)", agy_acp_server SDK metadata as
        // "(Extra High)", and Zeron as "(X-High)".
        ReasoningLevel::XHigh => &[
            " (xhigh)",
            " (x-high)",
            " (extra)",
            " (extra high)",
            " (extra_high)",
            " (xhigh thinking)",
            " (x-high thinking)",
            " (extra thinking)",
            " (extra high thinking)",
            " (extra_high thinking)",
        ],
        ReasoningLevel::Max => &[" (max)", " (max thinking)"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only Google effort suffixes are aliases; provider and model names survive.
    #[test]
    fn google_variant_keeps_provider_boundaries() {
        assert_eq!(
            google_model_variant("google-antigravity/gemini-4-argon-medium"),
            Some(("google-antigravity/gemini-4-argon", ReasoningLevel::Medium))
        );
        assert_eq!(
            google_model_variant("gemini-3.8-flash-high"),
            Some(("gemini-3.8-flash", ReasoningLevel::High))
        );
        assert_eq!(
            google_model_variant("gemini-4-argon-xhigh"),
            Some(("gemini-4-argon", ReasoningLevel::XHigh))
        );
        assert_eq!(
            google_model_variant("google-antigravity/gemini-4-argon-max"),
            Some(("google-antigravity/gemini-4-argon", ReasoningLevel::Max))
        );
        for id in [
            "openai/gemini-4-argon-high",
            "claude-high",
            "gemini-carbon-preview",
            "gemini-high",
            "gemini-max",
        ] {
            assert_eq!(google_model_variant(id), None, "{id}");
        }
    }
}

/// Fold native picker rows while preserving exact provider IDs for older clients.
pub fn google_model_picker_rows(harness: HarnessId, models: Vec<Model>) -> Vec<Model> {
    let mut grouped: Vec<Model> = Vec::new();
    for model in &models {
        if harness != HarnessId::Antigravity
            && !(harness == HarnessId::Omp && model.id.starts_with("google-antigravity/"))
        {
            grouped.push(model.clone());
            continue;
        }
        let Some((base, effort)) = google_model_variant(&model.id) else {
            grouped.push(model.clone());
            continue;
        };
        if let Some(row) = grouped
            .iter_mut()
            .find(|row| google_model_variant(&row.id).is_some_and(|(family, _)| family == base))
        {
            row.reasoning_levels
                .extend(model.reasoning_levels.iter().copied());
            row.reasoning_levels.push(effort);
            row.reasoning_levels.sort();
            row.reasoning_levels.dedup();
            continue;
        }
        let mut row = model.clone();
        // Remove only a matching effort annotation, never a model qualifier.
        let provider_suffix = if harness == HarnessId::Omp {
            " (Google)"
        } else {
            ""
        };
        let label = row
            .label
            .strip_suffix(provider_suffix)
            .unwrap_or(&row.label);
        let lower_label = label.to_ascii_lowercase();
        if let Some(annotation) = effort_label_annotations(effort)
            .iter()
            .copied()
            .find(|annotation| lower_label.ends_with(annotation))
            .filter(|annotation| label.len() > annotation.len())
        {
            row.label = format!(
                "{}{provider_suffix}",
                &label[..label.len() - annotation.len()]
            );
        }
        row.reasoning_levels.push(effort);
        row.reasoning_levels.sort();
        row.reasoning_levels.dedup();
        grouped.push(row);
    }
    grouped
}
#[cfg(test)]
mod picker_tests {
    use super::*;
    use crate::ReasoningLevel::{High, Low, Max, Medium, XHigh};

    fn catalog() -> Vec<Model> {
        [
            ("gemini-4-argon-medium", "Gemini 4 Argon (Medium)"),
            ("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
            ("gemini-4-argon-high", "Gemini 4 Argon (High)"),
            ("gemini-4-argon-low", "Gemini 4 Argon (Low)"),
            ("gemini-carbon-preview", "Gemini Carbon Preview"),
        ]
        .into_iter()
        .map(|(id, label)| Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
        })
        .collect()
    }

    // Failure modes: duplicate family rows, lost default, invented efforts,
    // and accidental stripping of a non-effort suffix.
    #[test]
    fn google_picker_groups_only_available_variants_and_preserves_default() {
        let models = google_model_picker_rows(HarnessId::Antigravity, catalog());
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "gemini-4-argon-medium");
        assert_eq!(models[0].label, "Gemini 4 Argon");
        assert_eq!(models[0].reasoning_levels, vec![Low, Medium, High]);
        assert_eq!(models[1].reasoning_levels, vec![High]);
        assert_eq!(models[2].id, "gemini-carbon-preview");
        assert!(models[2].reasoning_levels.is_empty());
    }

    #[test]
    fn google_picker_groups_xhigh_and_max_variants_and_strips_ccpa_labels() {
        let raw = [
            (
                "google-antigravity/gemini-4-argon-xhigh",
                "Gemini 4 Argon (Extra) (Google)",
            ),
            (
                "google-antigravity/gemini-4-argon-max",
                "Gemini 4 Argon (Max) (Google)",
            ),
            (
                "google-antigravity/gemini-4-argon-high",
                "Gemini 4 Argon (High) (Google)",
            ),
            (
                "google-antigravity/gemini-4-argon-medium",
                "Gemini 4 Argon (Medium) (Google)",
            ),
            (
                "google-antigravity/gemini-4-argon-low",
                "Gemini 4 Argon (Low) (Google)",
            ),
            (
                "google-antigravity/gemini-4-carbon-max",
                "Gemini 4 Carbon (Preview) (Google)",
            ),
            (
                "google-antigravity/gemini-4-carbon-xhigh",
                "Gemini 4 Carbon (Extra High) (Google)",
            ),
            (
                "google-antigravity/gemini-3.8-flash-xhigh",
                "Gemini 3.8 Flash (Extra High) (Google)",
            ),
        ]
        .into_iter()
        .map(|(id, label)| Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
        })
        .collect();

        let models = google_model_picker_rows(HarnessId::Omp, raw);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "google-antigravity/gemini-4-argon-xhigh");
        assert_eq!(models[0].label, "Gemini 4 Argon (Google)");
        assert_eq!(
            models[0].reasoning_levels,
            vec![Low, Medium, High, XHigh, Max]
        );
        assert_eq!(models[1].id, "google-antigravity/gemini-4-carbon-max");
        assert_eq!(models[1].label, "Gemini 4 Carbon (Preview) (Google)");
        assert_eq!(models[1].reasoning_levels, vec![XHigh, Max]);
        assert_eq!(models[2].id, "google-antigravity/gemini-3.8-flash-xhigh");
        assert_eq!(models[2].label, "Gemini 3.8 Flash (Google)");
        assert_eq!(models[2].reasoning_levels, vec![XHigh]);
    }
}
