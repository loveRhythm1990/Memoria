//! Bounded source adjacency. Positions and links describe provenance, not truth.
use std::collections::{BTreeMap, HashMap, HashSet};

use memoria_core::{
    source::{
        SourceContext, SourceContextLink, SourcePosition, SOURCE_CONTEXT_KEY,
        SOURCE_CONTEXT_MAX_CHUNKS_PER_MESSAGE, SOURCE_CONTEXT_MAX_FETCH, SOURCE_CONTEXT_MAX_LINKS,
        SOURCE_CONTEXT_MAX_RADIUS, SOURCE_CONTEXT_VERSION,
    },
    MemoriaError, Memory,
};

use crate::service::RetrieveOptions;

/// Opt-in controls. Default disables expansion for existing consumers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceContextOptions {
    pub(crate) radius: u64,
    pub(crate) anchors: usize,
    pub(crate) max_records: usize,
}

impl SourceContextOptions {
    pub fn new(radius: u64, anchors: usize, max_records: usize) -> Result<Self, MemoriaError> {
        if radius > SOURCE_CONTEXT_MAX_RADIUS
            || !(1..=8).contains(&anchors)
            || !(1..=100).contains(&max_records)
        {
            return Err(MemoriaError::Validation(
                "source context requires radius 0..=2, anchors 1..=8, max_records 1..=100".into(),
            ));
        }
        Ok(Self {
            radius,
            anchors,
            max_records,
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.radius > 0 && self.anchors > 0 && self.max_records > 0
    }
}

/// Build links from the complete ordered ingestion batch, before its atomic commit.
/// Legacy callers without positions retain their existing ingestion behavior.
pub(crate) fn attach_source_context(
    batch_id: &str,
    memories: &mut [Memory],
) -> Result<(), MemoriaError> {
    let positions: Vec<_> = memories.iter().map(Memory::source_position).collect();
    if positions.iter().all(Option::is_none) {
        return Ok(());
    }
    let mut groups: BTreeMap<u64, Vec<(u64, String)>> = BTreeMap::new();
    let mut seen = HashSet::new();
    let mut session = None;
    for (memory, position) in memories.iter().zip(&positions) {
        let Some(position) = position else {
            return Err(MemoriaError::Validation(
                "mixed source positions in one batch".into(),
            ));
        };
        if position.batch_id != batch_id
            || memory.session_id.as_deref().is_none_or(str::is_empty)
            || !seen.insert((position.message_index, position.chunk_index))
        {
            return Err(MemoriaError::Validation(
                "invalid or duplicate source position".into(),
            ));
        }
        match session {
            Some(value) if memory.session_id.as_deref() != Some(value) => {
                return Err(MemoriaError::Validation(
                    "source positions span multiple sessions".into(),
                ));
            }
            None => session = memory.session_id.as_deref(),
            _ => {}
        }
        groups
            .entry(position.message_index)
            .or_default()
            .push((position.chunk_index, memory.memory_id.clone()));
    }
    for group in groups.values_mut() {
        group.sort_by_key(|(chunk, _)| *chunk);
    }
    for (memory, position) in memories.iter_mut().zip(positions) {
        let position = position.expect("all positions validated above");
        let mut links = Vec::new();
        let mut message_indices = vec![position.message_index];
        for distance in 1..=SOURCE_CONTEXT_MAX_RADIUS {
            if let Some(previous) = position.message_index.checked_sub(distance) {
                message_indices.push(previous);
            }
            if let Some(next) = position.message_index.checked_add(distance) {
                message_indices.push(next);
            }
        }
        for message_index in message_indices {
            let Some(group) = groups.get(&message_index) else {
                continue;
            };
            // Same-message chunks are centered on the hit. Previous messages
            // contribute their tail, following messages their start; source stays stored.
            let start = if message_index == position.message_index {
                group
                    .partition_point(|(chunk, _)| *chunk < position.chunk_index)
                    .saturating_sub(SOURCE_CONTEXT_MAX_CHUNKS_PER_MESSAGE / 2)
            } else if message_index < position.message_index {
                group
                    .len()
                    .saturating_sub(SOURCE_CONTEXT_MAX_CHUNKS_PER_MESSAGE)
            } else {
                0
            };
            for (chunk_index, id) in group
                .iter()
                .skip(start)
                .filter(|(_, id)| id != &memory.memory_id)
                .take(SOURCE_CONTEXT_MAX_CHUNKS_PER_MESSAGE)
            {
                links.push(SourceContextLink {
                    memory_id: id.clone(),
                    message_index,
                    chunk_index: *chunk_index,
                });
            }
        }
        links.truncate(SOURCE_CONTEXT_MAX_LINKS);
        memory
            .extra_metadata
            .get_or_insert_with(HashMap::new)
            .insert(
                SOURCE_CONTEXT_KEY.into(),
                serde_json::to_value(SourceContext {
                    version: SOURCE_CONTEXT_VERSION,
                    links,
                })?,
            );
    }
    Ok(())
}

#[derive(Debug, serde::Serialize)]
pub struct ContextAttribution {
    pub memory_id: String,
    pub anchor_id: String,
    pub message_distance: u64,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct SourceContextExplain {
    pub base_result_count: usize,
    pub candidate_count: usize,
    pub added_count: usize,
    pub displaced_count: usize,
    pub selected_context_count: usize,
    pub elapsed_ms: f64,
    pub failed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub records: Vec<ContextAttribution>,
}

pub(crate) struct ContextCandidate {
    pub anchor_rank: usize,
    pub anchor_id: String,
    pub session_id: String,
    pub position: SourcePosition,
    pub link: SourceContextLink,
}

pub(crate) fn candidates(base: &[Memory], policy: SourceContextOptions) -> Vec<ContextCandidate> {
    let mut result = Vec::new();
    let mut unique = HashSet::new();
    for (rank, anchor) in base.iter().take(policy.anchors).enumerate() {
        if !anchor.is_source_evidence() {
            continue;
        }
        let (Some(position), Some(context), Some(session_id)) = (
            anchor.source_position(),
            anchor.source_context(),
            anchor.session_id.as_ref(),
        ) else {
            continue;
        };
        let mut links = context.links;
        links.sort_by_key(|link| {
            (
                position.message_index.abs_diff(link.message_index),
                link.message_index,
                link.chunk_index,
                link.memory_id.clone(),
            )
        });
        for link in links {
            if link.memory_id == anchor.memory_id
                || link.memory_id.is_empty()
                || link.memory_id.len() > 64
                || position.message_index.abs_diff(link.message_index) > policy.radius
                || !unique.insert(link.memory_id.clone())
            {
                continue;
            }
            result.push(ContextCandidate {
                anchor_rank: rank,
                anchor_id: anchor.memory_id.clone(),
                session_id: session_id.clone(),
                position: position.clone(),
                link,
            });
            if result.len() == SOURCE_CONTEXT_MAX_FETCH {
                return result;
            }
        }
    }
    result
}

/// Verify persisted links after the scoped read; never trust an ID alone.
pub(crate) fn matches(
    candidate: &ContextCandidate,
    memory: &Memory,
    user_id: &str,
    options: &RetrieveOptions,
) -> bool {
    memory.memory_id == candidate.link.memory_id
        && memory.user_id == user_id
        && memory.is_active
        && memory.superseded_by.is_none()
        && memory.is_source_evidence()
        && !memory.content.trim().is_empty()
        && memory.initial_confidence.is_finite()
        && memory.initial_confidence >= 0.05
        && memory.session_id.as_deref() == Some(candidate.session_id.as_str())
        && memory.source_position().is_some_and(|position| {
            position.batch_id == candidate.position.batch_id
                && position.message_index == candidate.link.message_index
                && position.chunk_index == candidate.link.chunk_index
        })
        && options
            .strict_session_id()
            .is_none_or(|session| memory.session_id.as_deref() == Some(session))
        && options
            .subject_id()
            .is_none_or(|subject| memory.subject_id.as_deref() == Some(subject))
        && options
            .memory_types()
            .is_none_or(|types| types.contains(&memory.memory_type))
}

pub(crate) fn compose(
    base: &[Memory],
    neighbors: &HashMap<String, Memory>,
    candidates: &[ContextCandidate],
    user_id: &str,
    top_k: usize,
    options: &RetrieveOptions,
    policy: SourceContextOptions,
) -> (Vec<Memory>, SourceContextExplain) {
    let mut explanation = SourceContextExplain {
        base_result_count: base.len(),
        candidate_count: candidates.len(),
        ..Default::default()
    };
    let base_by_id: HashMap<_, _> = base.iter().map(|m| (&m.memory_id, m)).collect();
    let char_budget: usize = base.iter().map(|m| m.content.chars().count()).sum();
    let mut selected: Vec<(usize, Memory)> = Vec::new();
    let mut seen = HashSet::new();
    let mut used_chars = 0;
    // Keep the original top anchors before spending slots or chars on context.
    for (rank, memory) in base.iter().take(policy.anchors.min(top_k)).enumerate() {
        if seen.insert(memory.memory_id.clone()) {
            used_chars += memory.content.chars().count();
            selected.push((rank, memory.clone()));
        }
    }
    for candidate in candidates {
        if explanation.selected_context_count >= policy.max_records {
            break;
        }
        let memory = base_by_id
            .get(&candidate.link.memory_id)
            .copied()
            .or_else(|| neighbors.get(&candidate.link.memory_id));
        let Some(memory) = memory.filter(|m| matches(candidate, m, user_id, options)) else {
            continue;
        };
        if seen.contains(&memory.memory_id) {
            if let Some((group, _)) = selected
                .iter_mut()
                .find(|(_, selected)| selected.memory_id == memory.memory_id)
            {
                if *group > candidate.anchor_rank {
                    *group = candidate.anchor_rank;
                    explanation.selected_context_count += 1;
                    explanation.records.push(ContextAttribution {
                        memory_id: memory.memory_id.clone(),
                        anchor_id: candidate.anchor_id.clone(),
                        message_distance: candidate
                            .position
                            .message_index
                            .abs_diff(candidate.link.message_index),
                    });
                }
            }
            continue;
        }
        let chars = memory.content.chars().count();
        if selected.len() >= top_k || chars > char_budget.saturating_sub(used_chars) {
            continue;
        }
        used_chars += chars;
        seen.insert(memory.memory_id.clone());
        selected.push((candidate.anchor_rank, memory.clone()));
        explanation.selected_context_count += 1;
        explanation.added_count += usize::from(!base_by_id.contains_key(&memory.memory_id));
        explanation.records.push(ContextAttribution {
            memory_id: memory.memory_id.clone(),
            anchor_id: candidate.anchor_id.clone(),
            message_distance: candidate
                .position
                .message_index
                .abs_diff(candidate.link.message_index),
        });
    }
    for (rank, memory) in base.iter().enumerate() {
        if selected.len() >= top_k {
            break;
        }
        if seen.contains(&memory.memory_id) {
            continue;
        }
        let chars = memory.content.chars().count();
        if chars > char_budget.saturating_sub(used_chars) {
            continue;
        }
        used_chars += chars;
        seen.insert(memory.memory_id.clone());
        selected.push((rank, memory.clone()));
    }
    explanation.displaced_count = base_by_id
        .keys()
        .filter(|id| !seen.contains(id.as_str()))
        .count();
    // Within each anchor's window preserve source order, with stable ID ties.
    selected.sort_by_key(|(rank, memory)| {
        let position = memory.source_position();
        (
            *rank,
            position.as_ref().map(|p| p.message_index).unwrap_or(0),
            position.as_ref().map(|p| p.chunk_index).unwrap_or(0),
            memory.memory_id.clone(),
        )
    });
    (
        selected.into_iter().map(|(_, memory)| memory).collect(),
        explanation,
    )
}
