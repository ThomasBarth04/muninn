-- Spec 003 §15: does the copilot shorten time to close?
-- Closed tickets from the last 90 days, split by whether they were shown at
-- least one similar case, and whether an agent marked one as helped.
-- Run as the owner role (it bypasses RLS). For one workspace, add
--   AND t.workspace_id = '<uuid>'
-- A reopened ticket counts from creation to its latest close.

SELECT CASE WHEN NOT shown THEN 'no suggestions'
            WHEN helped THEN 'suggestions, one helped'
            ELSE 'suggestions, none helped' END                    AS tickets,
       count(*)                                                    AS n,
       percentile_cont(0.5) WITHIN GROUP (ORDER BY closed_at - created_at) AS median_time_to_close,
       avg(closed_at - created_at)                                 AS mean_time_to_close
FROM (
    SELECT t.created_at, t.closed_at,
           EXISTS (SELECT 1 FROM suggestions s
                   WHERE s.ticket_id = t.id AND s.rank IS NOT NULL)          AS shown,
           EXISTS (SELECT 1 FROM suggestions s
                   JOIN suggestion_feedback f ON f.suggestion_id = s.id
                   WHERE s.ticket_id = t.id AND f.verdict = 'helped')        AS helped
    FROM tickets t
    WHERE t.status = 'closed' AND t.closed_at IS NOT NULL
      AND t.created_at > now() - interval '90 days'
) x
GROUP BY 1
ORDER BY 1;
