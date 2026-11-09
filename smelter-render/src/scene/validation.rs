use std::collections::{HashMap, HashSet};

use crate::{OutputId, RendererId};

use super::{Component, ComponentId, OutputScene, SceneError};

impl Component {
    fn component_id(&self) -> Option<&ComponentId> {
        match self {
            Component::InputStream(input) => input.id.as_ref(),
            Component::Shader(shader) => shader.id.as_ref(),
            Component::WebView(web) => web.id.as_ref(),
            Component::Image(image) => image.id.as_ref(),
            Component::Text(text) => text.id.as_ref(),
            Component::View(view) => view.id.as_ref(),
            Component::Tiles(tiles) => tiles.id.as_ref(),
            Component::Rescaler(rescaler) => rescaler.id.as_ref(),
        }
    }

    fn children(&self) -> Vec<&Component> {
        match self {
            Component::InputStream(_input) => vec![],
            Component::Shader(shader) => shader.children.iter().collect(),
            Component::WebView(view) => view.children.iter().collect(),
            Component::Image(_image) => vec![],
            Component::Text(_text) => vec![],
            Component::View(view) => view.children.iter().collect(),
            Component::Tiles(tiles) => tiles.children.iter().collect(),
            Component::Rescaler(rescaler) => vec![rescaler.child.as_ref()],
        }
    }
}

pub(super) fn validate_scene_update(
    old_outputs: &HashMap<OutputId, OutputScene>,
    updated_output: &OutputScene,
) -> Result<(), SceneError> {
    let mut updated_outputs: HashMap<&OutputId, &OutputScene> = old_outputs.iter().collect();
    updated_outputs.insert(&updated_output.output_id, updated_output);
    let updated_outputs: Vec<&OutputScene> = updated_outputs.values().copied().collect();

    validate_component_ids_uniqueness(&updated_outputs)?;
    validate_web_renderer_ids_uniqueness(&updated_outputs)?;
    Ok(())
}

fn validate_component_ids_uniqueness(outputs: &[&OutputScene]) -> Result<(), SceneError> {
    fn visit<'a>(
        component: &'a Component,
        ids: &mut HashSet<&'a ComponentId>,
    ) -> Result<(), SceneError> {
        let id = component.component_id();
        if let Some(id) = id {
            if ids.contains(id) {
                return Err(SceneError::DuplicateComponentId(id.clone()));
            }

            ids.insert(id);
        }

        component
            .children()
            .into_iter()
            .try_for_each(|c| visit(c, ids))
    }

    outputs.iter().try_for_each(|output| {
        let mut ids: HashSet<&ComponentId> = HashSet::new();
        visit(&output.scene_root, &mut ids)
    })
}

/// A web view embedding children lays them out in its page for that one
/// placement, so its renderer shows nowhere else: one without shows the same
/// page wherever it is placed.
fn validate_web_renderer_ids_uniqueness(outputs: &[&OutputScene]) -> Result<(), SceneError> {
    /// How many web views use each renderer, and whether one embeds children.
    fn visit<'a>(component: &'a Component, uses: &mut HashMap<&'a RendererId, (usize, bool)>) {
        if let Component::WebView(web_view) = component {
            let (count, embeds) = uses.entry(&web_view.instance_id).or_default();
            *count += 1;
            *embeds |= !web_view.children.is_empty();
        }
        component
            .children()
            .into_iter()
            .for_each(|child| visit(child, uses));
    }

    let mut uses = HashMap::new();
    for output in outputs {
        visit(&output.scene_root, &mut uses);
    }
    match uses
        .into_iter()
        .find(|(_, (count, embeds))| *count > 1 && *embeds)
    {
        Some((instance_id, _)) => Err(SceneError::WebRendererUsageNotExclusive(
            instance_id.clone(),
        )),
        None => Ok(()),
    }
}
