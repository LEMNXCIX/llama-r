//! Semantic index over skill metadata.
//!
//! Separate from [`crate::ports::rag::RagStore`] on purpose. `query_scoped` only
//! reads `scope.rag_sources`, so indexing skills there would force every agent
//! manifest to list a shared catalog among its own private memory collections.
//! This is a catalog lookup, not agent memory: read-only, derived from the skill
//! files, and never persisted.

use crate::adapters::rag::file_store::cosine_similarity;
use crate::domain::models::Skill;
use crate::ports::rag::EmbeddingProvider;
use std::collections::HashMap;
use std::sync::Arc;

/// One indexed skill and its ranking-relevant text.
#[derive(Debug, Clone)]
struct Entry {
    id: String,
    vector: Vec<f32>,
    /// True when the agent's manifest listed this skill explicitly.
    manual: bool,
}

/// A skill with its similarity score.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillMatch {
    pub id: String,
    pub score: f32,
    /// Whether the agent's manifest listed it.
    pub manual: bool,
}

pub struct SkillIndex {
    entries: HashMap<String, Entry>,
    embeddings: Arc<dyn EmbeddingProvider>,
}

/// Text used to embed a skill: id, description and tags carry the meaning,
/// the body does not, and the body is far too large to embed per query.
fn embeddable_text(skill: &Skill) -> String {
    let tags = skill
        .metadata
        .tags
        .as_ref()
        .map(|t| t.join(", "))
        .unwrap_or_default();
    format!(
        "{}\n{}\n{}",
        skill.id.replace(['-', '_'], " "),
        skill.metadata.description,
        tags
    )
}

impl SkillIndex {
    pub fn new(embeddings: Arc<dyn EmbeddingProvider>) -> Self {
        Self {
            entries: HashMap::new(),
            embeddings,
        }
    }

    /// (Re)build the index from a set of skills.
    ///
    /// `manual_ids` are the skills the agent's manifest listed by hand; they get
    /// a ranking boost so a thin description cannot silently drop them.
    pub async fn build(&mut self, skills: &[Skill], manual_ids: &[String]) -> Result<(), String> {
        self.entries.clear();
        if skills.is_empty() {
            return Ok(());
        }

        let texts: Vec<String> = skills.iter().map(embeddable_text).collect();
        let vectors = self.embeddings.embed(&texts).await?;
        if vectors.len() != skills.len() {
            return Err(format!(
                "skill index: embedding count mismatch (got {}, expected {})",
                vectors.len(),
                skills.len()
            ));
        }

        for (skill, vector) in skills.iter().zip(vectors) {
            self.entries.insert(
                skill.id.clone(),
                Entry {
                    id: skill.id.clone(),
                    vector,
                    manual: manual_ids.iter().any(|m| m == &skill.id),
                },
            );
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Rank indexed skills against `query`.
    ///
    /// Manual entries get a boost instead of a guarantee: they can still be
    /// dropped by an unrelated query, but not merely for being thinly described.
    pub async fn query(&self, query: &str, top_k: usize) -> Result<Vec<SkillMatch>, String> {
        if self.entries.is_empty() || top_k == 0 {
            return Ok(Vec::new());
        }
        let query_vec = self
            .embeddings
            .embed(&[query.to_string()])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| "skill index: empty embedding for query".to_string())?;

        let mut matches: Vec<SkillMatch> = self
            .entries
            .values()
            .map(|entry| SkillMatch {
                id: entry.id.clone(),
                score: cosine_similarity(&query_vec, &entry.vector)
                    + if entry.manual { MANUAL_BOOST } else { 0.0 },
                manual: entry.manual,
            })
            .collect();
        matches.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        matches.truncate(top_k);
        Ok(matches)
    }
}

/// Ranking advantage for skills the manifest listed explicitly.
///
/// Large enough to beat a weak-but-tied match, small enough that a clearly more
/// relevant automatic pick still wins.
pub const MANUAL_BOOST: f32 = 0.15;

/// Outcome of choosing which skills an agent gets.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkillSelection {
    /// Ids actually injected.
    pub selected: Vec<String>,
    /// Candidates that ranking dropped.
    ///
    /// Reported rather than applied silently: `skills = [...]` means "eligible",
    /// and the user must be able to see when something was left out.
    pub dropped: Vec<DroppedSkill>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedSkill {
    pub id: String,
    /// Why it was a candidate: listed in the manifest, or chosen by the LLM.
    pub origin: SkillOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillOrigin {
    /// Listed in the agent's `skills`.
    Manual,
    /// Chosen by `sync_project_agent_skills` from the project profile.
    Auto,
}

/// Choose which candidates survive ranking.
///
/// Ranking decides; a candidate missing from `ranked` (or past `top_k`) is
/// reported as dropped. Order follows the ranking, not the manifest, so the most
/// relevant skill leads the injected block.
pub fn apply_ranking(
    candidate_ids: &[String],
    ranked: &[SkillMatch],
    top_k: usize,
) -> SkillSelection {
    let keep: std::collections::HashSet<&str> =
        ranked.iter().take(top_k).map(|m| m.id.as_str()).collect();

    let mut selection = SkillSelection::default();
    for id in candidate_ids {
        if keep.contains(id.as_str()) {
            continue;
        }
        selection.dropped.push(DroppedSkill {
            id: id.clone(),
            origin: SkillOrigin::Manual,
        });
    }
    // Preserve ranking order among the survivors.
    let rank: std::collections::HashMap<&str, usize> = ranked
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.as_str(), i))
        .collect();
    let mut survivors: Vec<(usize, &String)> = candidate_ids
        .iter()
        .filter(|id| keep.contains(id.as_str()))
        .filter_map(|id| rank.get(id.as_str()).map(|r| (*r, id)))
        .collect();
    survivors.sort_by_key(|(rank, _)| *rank);
    selection
        .selected
        .extend(survivors.into_iter().map(|(_, id)| id.clone()));
    selection
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::rag::store::HashEmbeddingProvider;
    use crate::domain::models::SkillMetadata;

    fn skill(id: &str, description: &str, tags: Option<Vec<String>>) -> Skill {
        Skill {
            id: id.to_string(),
            path: id.to_string(),
            metadata: SkillMetadata {
                name: id.to_string(),
                description: description.to_string(),
                tags,
            },
            content: String::new(),
            scope: crate::domain::models::SkillScope::LlamaR,
        }
    }

    #[tokio::test]
    async fn ranks_the_relevant_skill_first() {
        let mut index = SkillIndex::new(Arc::new(HashEmbeddingProvider::new(32)));
        index
            .build(
                &[
                    skill(
                        "rust-async",
                        "async rust tokio futures",
                        Some(vec!["rust".into()]),
                    ),
                    skill(
                        "css-grid",
                        "css grid layout and flexbox",
                        Some(vec!["css".into()]),
                    ),
                ],
                &[],
            )
            .await
            .unwrap();

        let matches = index.query("tokio async rust", 2).await.unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(
            matches[0].id, "rust-async",
            "the relevant skill must rank first"
        );
    }

    #[tokio::test]
    async fn manual_skills_get_a_boost() {
        let mut index = SkillIndex::new(Arc::new(HashEmbeddingProvider::new(32)));
        let skills = vec![
            skill("declared", "totally unrelated subject matter", None),
            skill("other", "totally unrelated subject matter too", None),
        ];
        index
            .build(&skills, &["declared".to_string()])
            .await
            .unwrap();

        let matches = index
            .query("totally unrelated subject matter", 1)
            .await
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].id, "declared",
            "a manually declared skill must outrank an identical automatic one"
        );
        assert!(matches[0].manual);
    }

    #[tokio::test]
    async fn top_k_and_empty_query_are_handled() {
        let mut index = SkillIndex::new(Arc::new(HashEmbeddingProvider::new(16)));
        index
            .build(
                &[skill("a", "first", None), skill("b", "second", None)],
                &[],
            )
            .await
            .unwrap();

        assert!(index.query("anything", 0).await.unwrap().is_empty());
        assert_eq!(index.query("anything", 1).await.unwrap().len(), 1);
        assert_eq!(index.query("anything", 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn empty_catalog_returns_nothing() {
        let index = SkillIndex::new(Arc::new(HashEmbeddingProvider::new(16)));
        assert!(index.is_empty());
        assert!(index.query("anything", 5).await.unwrap().is_empty());
    }

    fn match_of(id: &str, score: f32, manual: bool) -> SkillMatch {
        SkillMatch {
            id: id.to_string(),
            score,
            manual,
        }
    }

    #[test]
    fn ranking_can_drop_a_manual_skill_and_says_so() {
        let candidates = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let ranked = vec![match_of("b", 0.9, false), match_of("c", 0.5, false)];
        let selection = apply_ranking(&candidates, &ranked, 10);

        // "a" was never ranked, so it is dropped — and reported.
        assert_eq!(selection.selected, vec!["b", "c"]);
        assert_eq!(selection.dropped.len(), 1);
        assert_eq!(selection.dropped[0].id, "a");
    }

    #[test]
    fn selection_follows_ranking_not_manifest_order() {
        let candidates = vec!["low".to_string(), "high".to_string()];
        let ranked = vec![match_of("high", 0.9, false), match_of("low", 0.1, false)];
        let selection = apply_ranking(&candidates, &ranked, 10);
        assert_eq!(
            selection.selected,
            vec!["high", "low"],
            "most relevant must lead the injected block"
        );
    }

    #[test]
    fn top_k_cuts_and_reports_the_rest() {
        let candidates: Vec<String> = (0..5).map(|i| format!("s{i}")).collect();
        let ranked: Vec<SkillMatch> = (0..5)
            .map(|i| match_of(&format!("s{i}"), 1.0 - i as f32 * 0.1, false))
            .collect();
        let selection = apply_ranking(&candidates, &ranked, 2);
        assert_eq!(selection.selected, vec!["s0", "s1"]);
        assert_eq!(selection.dropped.len(), 3);
    }

    #[test]
    fn selection_ignores_skills_that_were_not_candidates() {
        // A skill the index liked but nobody asked for must not sneak in.
        let candidates = vec!["a".to_string()];
        let ranked = vec![match_of("a", 0.5, false), match_of("unwanted", 0.99, false)];
        let selection = apply_ranking(&candidates, &ranked, 10);
        assert_eq!(selection.selected, vec!["a"]);
    }

    #[test]
    fn embeddable_text_includes_id_description_and_tags() {
        let text = embeddable_text(&skill(
            "api-conventions",
            "How we version our REST API",
            Some(vec!["api".into(), "rest".into()]),
        ));
        assert!(text.contains("api conventions"), "{text}");
        assert!(text.contains("How we version"), "{text}");
        assert!(text.contains("api, rest"), "{text}");
    }
}
