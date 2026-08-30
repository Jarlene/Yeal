//! Harness-side adapter bridging the GUI grounding engine's model calls to
//! the shell sampler — the grok-build replacement for upstream PR
//! understudy-ai/codex-cua#3's `ModelGuiGroundingProvider`, which reached the
//! model client through `ToolInvocation.session.services`. Modeled on the
//! `ShellCompactionSampler` pattern: the session layer builds the adapter
//! when the active model supports image inputs and hands it to
//! `AgentBuilder::with_gui_grounding_sampler`, which injects it into every
//! GUI tool call as a `ToolCallContext` extension
//! ([`xai_grok_tools::implementations::gui::grounding::grounding_sampler_from_context`]).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;

use crate::agent::models::ModelsManager;
use crate::sampling::Client as OaiCompatClient;
use crate::sampling::ConversationRequest;
use crate::sampling::SamplerConfig;
use xai_grok_sampling_types::conversation::{ContentPart, ConversationItem, SystemItem, UserItem};
use xai_grok_tools::implementations::gui::grounding::GuiGroundingModelImage;
use xai_grok_tools::implementations::gui::grounding::GuiGroundingModelRequest;
use xai_grok_tools::implementations::gui::grounding::GuiGroundingSampler;
use xai_grok_tools::implementations::gui::grounding::GuiGroundingSamplerError;

/// One grounding round-trip against the session's own model (upstream ran
/// grounding on the conversation's model client; its image-input modality
/// gate becomes the [`ModelsManager`] capability lookup below).
pub(crate) struct ShellGuiGroundingSampler {
    client: OaiCompatClient,
    model: String,
    provider_label: String,
}

impl ShellGuiGroundingSampler {
    pub(crate) fn new(client: OaiCompatClient, model: String) -> Self {
        Self {
            client,
            provider_label: format!("grok-build:{model}"),
            model,
        }
    }
}

#[async_trait::async_trait]
impl GuiGroundingSampler for ShellGuiGroundingSampler {
    fn provider_name(&self) -> String {
        self.provider_label.clone()
    }

    async fn sample_grounding_json(
        &self,
        request: &GuiGroundingModelRequest,
    ) -> Result<String, GuiGroundingSamplerError> {
        let mut parts: Vec<ContentPart> = Vec::with_capacity(1 + request.images.len());
        parts.push(ContentPart::Text {
            text: std::sync::Arc::<str>::from(request.prompt_text.as_str()),
        });
        for image in &request.images {
            parts.push(ContentPart::Image {
                url: std::sync::Arc::<str>::from(grounding_image_data_url(image)),
            });
        }
        let items = vec![
            ConversationItem::System(SystemItem {
                content: std::sync::Arc::<str>::from(request.system_prompt.as_str()),
            }),
            ConversationItem::User(UserItem {
                content: parts,
                ..Default::default()
            }),
        ];
        let req = ConversationRequest::from_items(items)
            .with_model(&self.model)
            .with_json_schema(request.output_schema.clone());
        const GROUNDING_TIMEOUT: Duration = Duration::from_secs(240);
        let response =
            tokio::time::timeout(GROUNDING_TIMEOUT, self.client.conversation_collect(req))
                .await
                .map_err(|_| {
                    GuiGroundingSamplerError(format!(
                        "grounding model call timed out after {}s",
                        GROUNDING_TIMEOUT.as_secs()
                    ))
                })?
                .map_err(|e| {
                    GuiGroundingSamplerError(format!("grounding model call failed: {e}"))
                })?;
        let text = response.assistant_text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(GuiGroundingSamplerError(
                "grounding model returned no content".to_string(),
            ));
        }
        Ok(trimmed.to_owned())
    }
}

fn grounding_image_data_url(image: &GuiGroundingModelImage) -> String {
    format!(
        "data:{};base64,{}",
        image.mime_type,
        base64::engine::general_purpose::STANDARD.encode(&image.bytes)
    )
}

/// Build the GUI grounding sampler for a session when the feature is enabled
/// and the active model accepts image inputs (upstream gate: the model's
/// `InputModality::Image`). Returns `None` when the feature is off or the
/// model is keyboard-only — the GUI engine then degrades exactly like
/// upstream (targeted tools refuse; captures skip semantic resolution).
pub(crate) fn prepare_gui_grounding_sampler(
    sampler_config: &SamplerConfig,
    models_manager: &ModelsManager,
    gui_tools_enabled: bool,
) -> Option<Arc<dyn GuiGroundingSampler>> {
    if !gui_tools_enabled {
        return None;
    }
    if !primary_model_supports_image_input(sampler_config, models_manager) {
        tracing::info!(
            model = %sampler_config.model,
            "gui_tools enabled but the active model does not support image inputs; \
             semantic grounding stays unavailable"
        );
        return None;
    }
    match OaiCompatClient::new(sampler_config.clone()) {
        Ok(client) => Some(Arc::new(ShellGuiGroundingSampler::new(
            client,
            sampler_config.model.clone(),
        ))),
        Err(e) => {
            tracing::warn!(
                error = %e,
                model = %sampler_config.model,
                "failed to build the GUI grounding sampler; semantic grounding stays unavailable"
            );
            None
        }
    }
}

/// Capability lookup against the model catalog (upstream: `InputModality`
/// gate). Unknown model → assumed image-capable, mirroring the
/// `ModelInfo::image_input` `None` default.
fn primary_model_supports_image_input(
    sampler_config: &SamplerConfig,
    models_manager: &ModelsManager,
) -> bool {
    let models = models_manager.models();
    let entry =
        crate::agent::config::find_model_by_id(&models, &sampler_config.model).or_else(|| {
            models
                .values()
                .find(|m| m.info.model == sampler_config.model)
        });
    match entry {
        Some(entry) => entry.info.image_input_supported(),
        None => true,
    }
}
