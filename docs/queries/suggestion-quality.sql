-- The evals behind the two thresholds (ADR 0002: Jev does not explain itself,
-- so stored scores and agent reactions carry the weight). Run as the owner
-- role (it bypasses RLS); add a workspace_id filter for one workspace.

-- 1. Similar cases, SIMILARITY_THRESHOLD = 0.5 (spec 003 §5).
-- Every candidate Jev scored, by score band. Only shown ones (rank set) can
-- be opened or rated, so bands under the threshold show `scored` alone.
-- A low helped_share just above 0.5 says raise it; a high one says lower it.
WITH fb AS (
    SELECT suggestion_id,
           bool_or(opened_at IS NOT NULL)               AS opened,
           count(*) FILTER (WHERE verdict = 'helped')      AS helped,
           count(*) FILTER (WHERE verdict = 'notRelevant') AS not_relevant
    FROM suggestion_feedback
    GROUP BY suggestion_id
)
SELECT floor(s.score * 10) / 10                           AS band,
       count(*)                                           AS scored,
       count(*) FILTER (WHERE s.rank IS NOT NULL)         AS shown,
       count(*) FILTER (WHERE fb.opened)                  AS opened,
       coalesce(sum(fb.helped), 0)                        AS helped_votes,
       coalesce(sum(fb.not_relevant), 0)                  AS not_relevant_votes,
       round(sum(fb.helped)::numeric / nullif(sum(fb.helped + fb.not_relevant), 0), 2) AS helped_share
FROM suggestions s
LEFT JOIN fb ON fb.suggestion_id = s.id
GROUP BY 1
ORDER BY 1 DESC;

-- 2. Categories, CATEGORY_THRESHOLD = 0.6 (spec 004 §6–8).
-- Every ticket Jev judged, by the probability of its pick. At or over 0.6
-- the pick was applied, so `agent_chose_other` is the override rate; under
-- it the agent chose from the chips or not at all. Many overrides just over
-- 0.6 say raise it; chips at 0.5 that agents always accept say lower it.
SELECT floor(t.jev_category_probability * 10) / 10       AS band,
       count(*)                                           AS judged,
       count(*) FILTER (WHERE t.category_id = t.jev_category_id)          AS matches_jev,
       count(*) FILTER (WHERE t.category_id <> t.jev_category_id)         AS agent_chose_other,
       count(*) FILTER (WHERE t.category_id IS NULL)                      AS uncategorised,
       round(count(*) FILTER (WHERE t.category_id <> t.jev_category_id)::numeric
             / nullif(count(*) FILTER (WHERE t.category_id IS NOT NULL), 0), 2) AS override_rate
FROM tickets t
WHERE t.jev_category_id IS NOT NULL
GROUP BY 1
ORDER BY 1 DESC;
